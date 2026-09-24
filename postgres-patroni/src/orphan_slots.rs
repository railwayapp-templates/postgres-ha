//! Standalone boot: drop the physical replication slots a reverted HA
//! cluster leaves behind.
//!
//! Patroni runs with `use_slots: true`, so the leader holds one physical
//! replication slot per member. A slot is a persistent object in PGDATA
//! (`pg_replslot/`): it survives restarts, and PostgreSQL never drops one on
//! its own — only Patroni did, while it ran. Reverting a cluster to
//! standalone deletes the members and boots the root with Patroni off in a
//! single step, so Patroni never sees the members leave and the slots stay
//! behind with nothing to consume them.
//!
//! An inactive slot pins every WAL segment past its `restart_lsn`. With
//! PostgreSQL's default `max_slot_wal_keep_size = -1` that pin has no bound:
//! `pg_wal` grows with every write until the volume fills and the database
//! stops. Observed live on 2026-09-13: a reverted root carrying two member
//! slots (`postgres_2`, `postgres_3`) with 69 GB of retained WAL on an
//! otherwise near-empty volume; dropping them freed the disk. A cluster
//! bootstrapped on an image with the derived cap (#90) is bounded, not
//! spared — the cap still lets each orphan pin up to a quarter of the
//! volume forever.
//!
//! What this module does, once per standalone boot:
//!
//! 1. **Gate on Patroni provenance.** Patroni caches its dynamic
//!    configuration in `{PGDATA}/patroni.dynamic.json`; the file exists iff
//!    Patroni has run against this data directory. A standalone that was
//!    never HA has no orphans by construction and is never touched. The gate
//!    is evidence on the volume, not an env var — the revert removes every
//!    `PATRONI_*` variable, so nothing else survives the transition.
//! 2. **Wait for a consumer to show up.** Every slot is `active = false` the
//!    instant PostgreSQL comes up — a legitimate standby or `pg_receivewal`
//!    has not reconnected yet. Their retry loops are on the order of
//!    `wal_retrieve_retry_interval` (5 s), so we hold for a grace window
//!    (default 60 s, `POSTGRES_ORPHAN_SLOT_GRACE_SECONDS`) after the first
//!    successful connection before judging anything.
//! 3. **Drop what is still unclaimed, physical and permanent.** Logical slots
//!    are never touched: they belong to the customer's CDC pipelines
//!    (Fivetran, Debezium, Electric), which reconnect on their own schedule
//!    and whose slots hold decoding state that cannot be rebuilt. Temporary
//!    slots die with their session and are skipped for cleanliness. A slot
//!    that becomes active between the plan and the drop makes
//!    `pg_drop_replication_slot` fail loudly — that failure is the guard
//!    working, and the other drops still proceed.
//!
//! Every drop is logged with the slot name and the WAL it was retaining, and
//! reported as a telemetry event, so the reclaim is visible rather than
//! silent.
//!
//! **One pass per boot, however long it takes.** The pass retries with
//! backoff until it completes once: a server still in crash recovery, a
//! socket that is not there yet, a login the reaper cannot yet use — none of
//! these may turn into "orphans until the next redeploy", which can be months
//! away with WAL growing the whole time. The pass is NOT periodic, on
//! purpose: orphans only appear at the revert, which is a boot, and a
//! periodic dropper would cost a customer's own external standby its slot on
//! any outage longer than the grace window, not only at a redeploy. A pass
//! that PostgreSQL answered counts as complete even when it refused some
//! drops — a refused slot has a consumer.

use anyhow::{Context, Result};
use common::{Telemetry, TelemetryEvent};
use std::future::Future;
use std::path::Path;
use std::time::Duration;
use tokio_postgres::NoTls;
use tracing::{info, warn};

/// Patroni's dynamic-configuration cache, written into the data directory
/// on every load. Its presence is the provenance signal this module gates on.
pub const PATRONI_DYNAMIC_CONFIG_FILE: &str = "patroni.dynamic.json";

/// Operator knob for the consumer grace window (seconds). Kept public so the
/// e2e harness and the README name the same string.
pub const GRACE_ENV: &str = "POSTGRES_ORPHAN_SLOT_GRACE_SECONDS";

const DEFAULT_GRACE_SECS: u64 = 60;

/// Retry backoff between attempts of the pass: starts at 3 s, doubles, and
/// caps at 60 s, so a long crash recovery is re-checked every minute rather
/// than hammered — and never waited out past the cap.
const BACKOFF_INITIAL: Duration = Duration::from_secs(3);
const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// The unix socket directory and port the standalone server listens on:
/// Patroni's rendered `unix_socket_directories` and the official image's
/// default are the same directory, and Patroni renders `port = 5432`.
const SOCKET_DIR: &str = "/var/run/postgresql";
const PORT: u16 = 5432;

/// True when Patroni has run against this data directory.
pub fn pgdata_was_patroni_managed(pgdata: &str) -> bool {
    Path::new(pgdata)
        .join(PATRONI_DYNAMIC_CONFIG_FILE)
        .is_file()
}

/// Grace window from the environment. Clamped to at least one second — a
/// zero window would judge slots before any consumer could possibly
/// reconnect, which defeats the reason the window exists.
pub fn grace_from_env() -> Duration {
    grace_from_value(std::env::var(GRACE_ENV).ok().as_deref())
}

fn grace_from_value(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_GRACE_SECS)
        .max(1);
    Duration::from_secs(secs)
}

/// One row of `pg_replication_slots`, reduced to what the decision needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRow {
    pub slot_name: String,
    pub slot_type: String,
    pub active: bool,
    pub temporary: bool,
    /// WAL the slot currently pins, in bytes (0 when it reserves none).
    pub retained_wal_bytes: i64,
}

/// The slots to drop: physical, permanent, and still without a consumer
/// after the grace window. Logical slots are never returned, whatever their
/// state. Pure so the rule itself is unit-tested.
pub fn plan_orphan_slot_drops(rows: &[SlotRow]) -> Vec<SlotRow> {
    rows.iter()
        .filter(|r| r.slot_type == "physical" && !r.active && !r.temporary)
        .cloned()
        .collect()
}

/// Human-readable size for the log line (binary units, like pg_size_pretty).
pub fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes.max(0), UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// What one pass over `pg_replication_slots` did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReapOutcome {
    /// Slots present before the pass, of every type.
    pub seen: usize,
    /// Orphaned member slots dropped, in drop order.
    pub dropped: Vec<String>,
    /// Slots the plan selected but PostgreSQL refused to drop — a consumer
    /// attached between the plan and the drop. Kept on purpose.
    pub refused: Vec<String>,
    /// WAL the dropped slots were pinning, released at the next checkpoint.
    pub reclaimed_wal_bytes: i64,
}

/// Backoff before retry number `attempt` (1-based): 3 s, 6 s, 12 s, ...,
/// capped at 60 s. Pure so the schedule is unit-tested.
pub fn backoff_for_attempt(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    BACKOFF_INITIAL
        .saturating_mul(1u32 << doublings)
        .min(BACKOFF_CAP)
}

/// Runs beside the standalone server for one boot: connect, hold the grace
/// window, reap — and retry the whole attempt with backoff until it completes
/// once. Never returns an error to the caller — every failure is logged and
/// retried, because a database that comes up with stale slots is strictly
/// better than one that does not come up, and one that keeps the orphans
/// until the next redeploy is not acceptable either.
pub async fn reap_after_boot(pgdata: String, telemetry: Telemetry) {
    let grace = grace_from_env();
    info!(
        pgdata = %pgdata,
        grace_secs = grace.as_secs(),
        marker = PATRONI_DYNAMIC_CONFIG_FILE,
        "orphan-slots: data directory was Patroni-managed; will drop physical replication slots still unclaimed after the grace window"
    );

    let outcome = run_until_complete(
        || async {
            let client = connect(SOCKET_DIR, PORT).await?;
            // The grace runs after EVERY successful connection, not once per
            // boot: a retry here means the previous session was lost, and
            // whatever cost us the session may have cost a consumer its
            // connection too.
            tokio::time::sleep(grace).await;
            reap(&client).await
        },
        backoff_for_attempt,
    )
    .await;

    if outcome.dropped.is_empty() {
        info!(
            slots_seen = outcome.seen,
            refused = outcome.refused.len(),
            "orphan-slots: no unclaimed physical replication slots; nothing to drop"
        );
        return;
    }
    info!(
        dropped = outcome.dropped.len(),
        reclaimed_wal = %human_bytes(outcome.reclaimed_wal_bytes),
        "orphan-slots: the retained WAL is released at the next checkpoint"
    );
    telemetry.send(TelemetryEvent::StandaloneOrphanSlotsDropped {
        slots: outcome.dropped,
        retained_wal_bytes: outcome.reclaimed_wal_bytes.max(0) as u64,
        grace_secs: grace.as_secs(),
    });
}

/// Drive `attempt` until it returns `Ok`, sleeping `backoff(n)` after the
/// n-th failure. Every failure is a warning naming the attempt and the wait,
/// so a pass that cannot complete is loud for as long as it lasts. Generic
/// over the attempt and the backoff so the loop itself is unit-tested
/// without a database.
pub async fn run_until_complete<F, Fut>(
    mut attempt: F,
    backoff: impl Fn(u32) -> Duration,
) -> ReapOutcome
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<ReapOutcome>>,
{
    let mut attempts: u32 = 0;
    loop {
        attempts = attempts.saturating_add(1);
        match attempt().await {
            Ok(outcome) => {
                if attempts > 1 {
                    info!(attempts, "orphan-slots: pass completed after retries");
                }
                return outcome;
            }
            Err(e) => {
                let wait = backoff(attempts);
                warn!(
                    attempt = attempts,
                    retry_in_secs = wait.as_secs(),
                    error = %e,
                    "orphan-slots: pass did not complete; retrying until one pass completes"
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
}

async fn connect(socket_dir: &str, port: u16) -> Result<tokio_postgres::Client> {
    let user = std::env::var("POSTGRES_USER").unwrap_or_else(|_| "postgres".to_string());
    // Local connections are `trust` in both Patroni's rendered pg_hba and the
    // official image's default, so the password is a fallback for a
    // customer-tightened pg_hba, not the primary path.
    let password = std::env::var("POSTGRES_PASSWORD")
        .or_else(|_| std::env::var("PGPASSWORD"))
        .unwrap_or_default();

    let mut config = tokio_postgres::Config::new();
    config
        .host(socket_dir)
        .port(port)
        .user(&user)
        .dbname("postgres")
        .application_name("postgres-wrapper orphan-slots")
        .connect_timeout(Duration::from_secs(5));
    if !password.is_empty() {
        config.password(&password);
    }

    let (client, connection) = config
        .connect(NoTls)
        .await
        .context("connect to PostgreSQL over the local socket")?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            warn!(error = %e, "orphan-slots: connection closed");
        }
    });
    Ok(client)
}

/// One pass: read every slot, drop the orphaned member slots, log each drop
/// with the WAL it was retaining.
pub async fn reap(client: &tokio_postgres::Client) -> Result<ReapOutcome> {
    let rows = client
        .query(
            "SELECT slot_name::text, slot_type::text, active, temporary, \
                    COALESCE(pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn), 0)::bigint \
             FROM pg_replication_slots",
            &[],
        )
        .await
        .context("query pg_replication_slots")?;
    let rows: Vec<SlotRow> = rows
        .iter()
        .map(|r| SlotRow {
            slot_name: r.get(0),
            slot_type: r.get(1),
            active: r.get(2),
            temporary: r.get(3),
            retained_wal_bytes: r.get(4),
        })
        .collect();

    let mut outcome = ReapOutcome {
        seen: rows.len(),
        ..ReapOutcome::default()
    };
    for slot in plan_orphan_slot_drops(&rows) {
        match client
            .execute("SELECT pg_drop_replication_slot($1)", &[&slot.slot_name])
            .await
        {
            Ok(_) => {
                info!(
                    slot = %slot.slot_name,
                    retained_wal = %human_bytes(slot.retained_wal_bytes),
                    retained_wal_bytes = slot.retained_wal_bytes,
                    "orphan-slots: dropped orphaned replication slot left by a reverted HA cluster member"
                );
                outcome.reclaimed_wal_bytes += slot.retained_wal_bytes.max(0);
                outcome.dropped.push(slot.slot_name);
            }
            // A consumer that attached between the plan and the drop makes
            // PostgreSQL refuse ("replication slot is active for PID"). That
            // is the guard doing its job: keep the slot, say so, move on.
            Err(e) => {
                warn!(
                    slot = %slot.slot_name,
                    error = %e,
                    "orphan-slots: could not drop replication slot; keeping it"
                );
                outcome.refused.push(slot.slot_name);
            }
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, slot_type: &str, active: bool, temporary: bool) -> SlotRow {
        SlotRow {
            slot_name: name.to_string(),
            slot_type: slot_type.to_string(),
            active,
            temporary,
            retained_wal_bytes: 0,
        }
    }

    #[test]
    fn drops_only_unclaimed_permanent_physical_slots() {
        let rows = vec![
            row("postgres_2", "physical", false, false),
            row("postgres_3", "physical", false, false),
        ];
        let names: Vec<_> = plan_orphan_slot_drops(&rows)
            .into_iter()
            .map(|r| r.slot_name)
            .collect();
        assert_eq!(names, vec!["postgres_2", "postgres_3"]);
    }

    #[test]
    fn never_touches_logical_slots_whatever_their_state() {
        let rows = vec![
            row("fivetran_pgoutput_slot", "logical", false, false),
            row("debezium", "logical", true, false),
        ];
        assert!(plan_orphan_slot_drops(&rows).is_empty());
    }

    #[test]
    fn keeps_a_physical_slot_that_has_a_consumer_attached() {
        let rows = vec![
            row("external_standby", "physical", true, false),
            row("postgres_2", "physical", false, false),
        ];
        let names: Vec<_> = plan_orphan_slot_drops(&rows)
            .into_iter()
            .map(|r| r.slot_name)
            .collect();
        assert_eq!(names, vec!["postgres_2"]);
    }

    #[test]
    fn skips_temporary_slots() {
        let rows = vec![row("pg_basebackup_123", "physical", false, true)];
        assert!(plan_orphan_slot_drops(&rows).is_empty());
    }

    #[test]
    fn provenance_gate_is_the_patroni_dynamic_config_cache() {
        let dir = tempfile::tempdir().unwrap();
        let pgdata = dir.path().to_str().unwrap();
        assert!(!pgdata_was_patroni_managed(pgdata));
        std::fs::write(dir.path().join(PATRONI_DYNAMIC_CONFIG_FILE), "{}").unwrap();
        assert!(pgdata_was_patroni_managed(pgdata));
    }

    #[test]
    fn provenance_gate_ignores_a_directory_of_that_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(PATRONI_DYNAMIC_CONFIG_FILE)).unwrap();
        assert!(!pgdata_was_patroni_managed(dir.path().to_str().unwrap()));
    }

    #[test]
    fn grace_defaults_and_is_clamped_to_at_least_one_second() {
        assert_eq!(
            grace_from_value(None),
            Duration::from_secs(DEFAULT_GRACE_SECS)
        );
        assert_eq!(grace_from_value(Some("20")), Duration::from_secs(20));
        assert_eq!(grace_from_value(Some(" 20 ")), Duration::from_secs(20));
        assert_eq!(grace_from_value(Some("0")), Duration::from_secs(1));
        assert_eq!(
            grace_from_value(Some("not-a-number")),
            Duration::from_secs(DEFAULT_GRACE_SECS)
        );
    }

    #[test]
    fn backoff_doubles_from_three_seconds_and_caps_at_a_minute() {
        let secs: Vec<u64> = (1..=8).map(|n| backoff_for_attempt(n).as_secs()).collect();
        assert_eq!(secs, vec![3, 6, 12, 24, 48, 60, 60, 60]);
        assert_eq!(backoff_for_attempt(0).as_secs(), 3);
        assert_eq!(backoff_for_attempt(u32::MAX).as_secs(), 60);
    }

    #[tokio::test]
    async fn loop_retries_until_one_pass_completes_then_stops() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let calls = Arc::new(AtomicU32::new(0));
        let seen = calls.clone();
        let outcome = run_until_complete(
            move || {
                let seen = seen.clone();
                async move {
                    let n = seen.fetch_add(1, Ordering::SeqCst) + 1;
                    if n < 3 {
                        anyhow::bail!("connection refused (attempt {n})")
                    }
                    Ok(ReapOutcome {
                        seen: 2,
                        dropped: vec!["postgres_2".into()],
                        ..ReapOutcome::default()
                    })
                }
            },
            |_| Duration::ZERO,
        )
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "two failures, then success"
        );
        assert_eq!(outcome.dropped, vec!["postgres_2".to_string()]);
    }

    #[tokio::test]
    async fn a_pass_with_refused_drops_still_counts_as_complete() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let calls = Arc::new(AtomicU32::new(0));
        let seen = calls.clone();
        let outcome = run_until_complete(
            move || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Ok(ReapOutcome {
                        seen: 1,
                        refused: vec!["external_standby".into()],
                        ..ReapOutcome::default()
                    })
                }
            },
            |_| Duration::ZERO,
        )
        .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a refused drop has a consumer; no retry"
        );
        assert_eq!(outcome.refused, vec!["external_standby".to_string()]);
    }

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 kB");
        assert_eq!(human_bytes(69 * 1024 * 1024 * 1024), "69.0 GB");
        assert_eq!(human_bytes(-5), "0 B");
    }
}
