//! Slot recovery (leader-only) and live `max_slot_wal_keep_size` reconcile
//! (every node).
//!
//! Bounding `max_slot_wal_keep_size` stops a lagging member's replication slot
//! from filling the leader's disk (see [`super::config`]), but the bound alone
//! is a one-way door: when a slot exceeds the cap PostgreSQL invalidates it
//! (`wal_status='lost'`) and frees the WAL — the leader survives — but nothing
//! puts the slot back. This watcher closes that loop:
//!
//! 1. **Recreate invalidated slots** (leader-only — member slots live on the
//!    primary). Patroni 4.1.0's `load_replication_slots`
//!    never selects `wal_status`, so it neither notices the invalidation nor
//!    recreates the slot, and PostgreSQL refuses to let the standby re-stream on
//!    it (`walsender.c` `StartReplication` acquires with `error_if_invalid=true`
//!    → `slot.c` "can no longer access replication slot"). The standby would sit
//!    on `restore_command` (S3) forever and — being >`maximum_lag_on_failover`
//!    behind — could never be promoted, i.e. silent loss of HA. Dropping the
//!    `lost` slot returns it to Patroni's create-if-missing path, which
//!    recreates it, and the standby resumes streaming. An invalidated slot is
//!    filtered on `NOT active`, so a concurrent acquisition (possible
//!    transiently on some PG versions while a standby retries) just skips it
//!    this cycle and the next cycle re-decides.
//!    We only drop slots whose names match current cluster members, so a user's
//!    own slot is never touched.
//!
//! 2. **Track free space** (every node — the cap is a property of each node's
//!    own volume, and every member can become leader). `Config::from_env` sizes
//!    the cap once at startup; a long-lived node whose data grows drifts toward
//!    a cap that's generous relative to *current* free space. Each cycle
//!    re-derives the cap from live `statvfs` and, when the effective GUC has
//!    drifted, applies it via `ALTER SYSTEM SET` + `pg_reload_conf()`.
//!
//!    Why `ALTER SYSTEM` and not a Patroni `PATCH /config` (DCS): Patroni's
//!    `_build_effective_configuration` documents that *local* configuration
//!    takes precedence over *dynamic* (DCS) configuration, and its
//!    `is_local` filter only discards `CMDLINE_OPTIONS` parameters —
//!    `max_slot_wal_keep_size` is not one of them. The boot-time value yaml.rs
//!    renders into `patroni.yml`'s local `postgresql.parameters` therefore
//!    shadows any DCS value forever, and a DCS patch would be a functional
//!    no-op. `postgresql.auto.conf`, by contrast, outranks Patroni's rendered
//!    `postgresql.conf` and survives its re-renders — Patroni's
//!    `_sanitize_auto_conf` only strips recovery parameters — which is exactly
//!    why `reconcile.rs`'s `force_live_archive_gucs` uses the same mechanism.
//!    The GUC is `PGC_SIGHUP`, and `ALTER SYSTEM` works on standbys too
//!    (auto.conf is written without WAL).
//!
//!    Running on every node also self-corrects the pin `pg_basebackup` copies
//!    into fresh clones along with `postgresql.auto.conf`: a clone taken from a
//!    2 TB leader inherits that leader's cap, and its own first reconcile cycle
//!    re-sizes it to the clone's actual volume.
//!
//!    `POSTGRES_MAX_SLOT_WAL_KEEP_SIZE` remains the operator pin: when set (and
//!    valid), the reconciler converges the effective GUC to that value instead
//!    of the derived one, so the pin holds even against an inherited or stale
//!    auto.conf entry. A hand-issued `ALTER SYSTEM` without the env pin is not
//!    a supported override and will be re-converged once it drifts past the
//!    hysteresis band.
//!
//! `SLOT_RECOVERY_DISABLED=1` is the operator kill switch. It disables slot
//! repair, renders the boot-time cap as unlimited, and runs a small one-shot
//! neutralizer that overwrites any stale cap previously persisted in
//! `postgresql.auto.conf`. Disabling repair while leaving the cap active would
//! recreate the one-way-door failure described above.

use super::config::{
    derive_slot_keep_mib, slot_keep_operator_override, slot_recovery_disabled,
    volume_total_and_free_mib,
};
use super::reconcile::{local_node_is_leader, wait_for_patroni_rest, PATRONI_REST};
use super::Config;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{info, warn};

const DEFAULT_POLL_SECONDS: u64 = 60;
const DEFAULT_REPAIR_BACKOFF_SECONDS: u64 = 900;

/// Only rewrite the cap when it moves by more than this fraction of the current
/// value, so ordinary disk churn (temp files, a checkpoint) doesn't trigger a
/// config reload every cycle. A real trend (data growth, a large delete) clears
/// it within a poll or two.
const CAP_REPATCH_HYSTERESIS_PCT: u64 = 10;

/// Spawn the slot-recovery watcher. Mirrors the self-heal watcher's
/// respawn shape: an outer loop wraps `run` in `spawn` so a panic surfaces as a
/// `JoinError` and respawns rather than taking down patroni-runner.
pub fn spawn(volume_root: String) {
    if slot_recovery_disabled() {
        warn!(
            "slot-recovery: SLOT_RECOVERY_DISABLED=1 — slot repair disabled; neutralizing the effective WAL-retention cap"
        );
        spawn_disabled_cap_neutralizer();
        return;
    }

    // Clamped to ≥1: 0 would hot-loop against /leader and psql.
    let poll_secs = std::env::var("SLOT_RECOVERY_POLL_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_POLL_SECONDS)
        .max(1);
    let repair_backoff_secs = std::env::var("SLOT_RECOVERY_REPAIR_BACKOFF_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_REPAIR_BACKOFF_SECONDS)
        .max(1);

    info!(
        poll_secs,
        repair_backoff_secs,
        volume_root = %volume_root,
        "slot-recovery: starting watcher (slot repair on the leader, cap reconcile on every node)"
    );

    tokio::spawn(async move {
        loop {
            let vr = volume_root.clone();
            let h =
                tokio::task::spawn(async move { run(vr, poll_secs, repair_backoff_secs).await });
            match h.await {
                Ok(Ok(())) => warn!("slot-recovery: run loop returned cleanly — respawning in 5s"),
                Ok(Err(e)) => {
                    warn!(error = %e, "slot-recovery: run loop errored — respawning in 5s")
                }
                Err(e) if e.is_panic() => {
                    warn!(panic = ?e, "slot-recovery: run loop panicked — respawning in 5s")
                }
                Err(e) => warn!(error = %e, "slot-recovery: join error — respawning in 5s"),
            }
            sleep(Duration::from_secs(5)).await;
        }
    });
}

/// A cap written through ALTER SYSTEM outranks the generated Patroni YAML and
/// survives restarts.  The emergency switch therefore needs an active cleanup
/// step; merely rendering `-1` would leave the old auto.conf value effective.
fn spawn_disabled_cap_neutralizer() {
    tokio::spawn(async move {
        loop {
            match neutralize_disabled_cap().await {
                Ok(()) => return,
                Err(e) => warn!(
                    error = %e,
                    "slot-recovery: failed to neutralize cap while disabled — retrying in 5s"
                ),
            }
            sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn neutralize_disabled_cap() -> Result<()> {
    let config = Config::from_env().context("load config for disabled slot-recovery cleanup")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;
    wait_for_patroni_rest(&client).await?;
    if current_effective_cap_mib(&config.superuser).await != Some(None) {
        alter_system_set_cap(&config.superuser, "-1").await?;
    }
    info!("slot-recovery: effective max_slot_wal_keep_size is unlimited while disabled");
    Ok(())
}

async fn run(volume_root: String, poll_secs: u64, repair_backoff_secs: u64) -> Result<()> {
    let config = Config::from_env().context("load config for slot-recovery")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;

    wait_for_patroni_rest(&client).await?;
    let mut last_repairs = HashMap::new();

    loop {
        // Slot repair is leader-only: member slots live on the primary, and
        // only the primary retains WAL for them. Re-checked every cycle so a
        // new leader picks the work up within one poll after failover, and an
        // ex-leader drops it just as fast.
        if local_node_is_leader(&client).await {
            if let Err(e) = recreate_lost_slots(
                &client,
                &config.superuser,
                &mut last_repairs,
                Duration::from_secs(repair_backoff_secs),
            )
            .await
            {
                warn!(error = %e, "slot-recovery: recreate-lost-slots iteration errored (continuing)");
            }
        }
        // The cap reconcile runs on EVERY node: the cap is sized to this
        // node's own volume, every member can become leader, and a clone
        // inherits its donor's auto.conf pin (see the module doc) — which
        // only the clone's own reconcile can re-size.
        if let Err(e) = reconcile_cap(&config, &volume_root).await {
            warn!(error = %e, "slot-recovery: cap reconcile iteration errored (continuing)");
        }
        sleep(Duration::from_secs(poll_secs)).await;
    }
}

// ====================================================================
// 1. Recreate invalidated slots
// ====================================================================

async fn recreate_lost_slots(
    client: &reqwest::Client,
    superuser: &str,
    last_repairs: &mut HashMap<String, Instant>,
    repair_backoff: Duration,
) -> Result<()> {
    let lost = query_lost_physical_slots(superuser).await?;
    if lost.is_empty() {
        return Ok(());
    }

    // Restrict to slots Patroni will actually recreate — the current cluster
    // members' slots. A user's hand-made physical slot that happens to be lost
    // is theirs to manage; dropping it here would delete it for good, since
    // Patroni only recreates member slots.
    let members = cluster_member_slot_names(client).await.unwrap_or_default();
    if members.is_empty() {
        warn!(
            lost = ?lost,
            "slot-recovery: found lost slots but couldn't read cluster members — skipping this cycle rather than risk dropping a non-member slot"
        );
        return Ok(());
    }

    for slot in lost {
        if !members.contains(&slot) {
            info!(slot = %slot, "slot-recovery: lost slot is not a cluster member slot — leaving it for the operator");
            continue;
        }
        if repair_is_backed_off(
            last_repairs.get(&slot).copied(),
            Instant::now(),
            repair_backoff,
        ) {
            warn!(
                slot = %slot,
                backoff_secs = repair_backoff.as_secs(),
                "slot-recovery: lost member slot was repaired recently — suppressing a recreate loop"
            );
            continue;
        }
        match drop_replication_slot(superuser, &slot).await {
            Ok(()) => {
                last_repairs.insert(slot.clone(), Instant::now());
                info!(
                    slot = %slot,
                    "slot-recovery: dropped invalidated member slot; Patroni will recreate it and the standby will resume streaming"
                )
            }
            Err(e) => {
                warn!(slot = %slot, error = %e, "slot-recovery: failed to drop invalidated slot")
            }
        }
    }
    Ok(())
}

fn repair_is_backed_off(last: Option<Instant>, now: Instant, backoff: Duration) -> bool {
    last.is_some_and(|last| now.saturating_duration_since(last) < backoff)
}

/// Names of physical slots PostgreSQL has invalidated and that no backend holds.
/// `wal_status='lost'` is the terminal state after a slot's retained WAL was
/// removed; `NOT active` guarantees the drop can't race a live consumer.
async fn query_lost_physical_slots(superuser: &str) -> Result<Vec<String>> {
    let out = psql(
        superuser,
        "SELECT slot_name FROM pg_catalog.pg_replication_slots \
         WHERE slot_type = 'physical' AND wal_status = 'lost' AND NOT active",
    )
    .await
    .context("query lost slots")?;
    Ok(parse_slot_name_rows(&out))
}

async fn drop_replication_slot(superuser: &str, slot: &str) -> Result<()> {
    // Parameterised through a prepared value would need a driver; the slot name
    // comes from pg_replication_slots (PostgreSQL-validated identifiers), and
    // we additionally gate on the cluster-member set before calling this, so
    // there's no untrusted input. Still quote it defensively.
    let sql = format!(
        "SELECT pg_catalog.pg_drop_replication_slot('{}')",
        slot.replace('\'', "''")
    );
    psql(superuser, &sql).await.context("drop slot")?;
    Ok(())
}

/// Member slot names from Patroni `/cluster`, mapped through Patroni's own
/// member-name → slot-name rule so the set matches what it will recreate.
async fn cluster_member_slot_names(client: &reqwest::Client) -> Option<Vec<String>> {
    let cluster: Value = client
        .get(format!("{PATRONI_REST}/cluster"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let members = cluster.get("members")?.as_array()?;
    Some(
        members
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
            .map(slot_name_from_member_name)
            .collect(),
    )
}

// ====================================================================
// 2. Re-derive the cap from live free space
// ====================================================================

async fn reconcile_cap(config: &Config, volume_root: &str) -> Result<()> {
    // What the effective GUC SHOULD be: the operator's env pin when set (the
    // reconcile then holds the pin against an inherited or stale auto.conf
    // entry), the live-derived value otherwise.
    let (desired, is_pin) = match slot_keep_operator_override() {
        Some(pin) => (parse_pg_size_to_mib(&pin), true),
        None => {
            let (total_mib, free_mib) = volume_total_and_free_mib(volume_root);
            match derive_slot_keep_mib(total_mib, free_mib, config.wal_archive_bucket.is_some()) {
                Some(mib) => (Some(mib), false),
                // Volume unmeasurable — leave whatever's in place rather than
                // force `-1`.
                None => return Ok(()),
            }
        }
    };

    // The live effective value, not DCS intent: postgresql.auto.conf and the
    // boot-time local param both outrank DCS (see the module doc), so the
    // running GUC is the only reading that reflects what PostgreSQL will
    // actually enforce. Postgres not answering (mid-clone, starting up) skips
    // the cycle.
    let Some(current) = current_effective_cap_mib(&config.superuser).await else {
        return Ok(());
    };

    let rewrite = match (is_pin, desired) {
        // Derived path: hysteresis, so disk churn doesn't reload every cycle.
        (false, Some(desired_mib)) => {
            should_repatch_cap(current, desired_mib, CAP_REPATCH_HYSTERESIS_PCT)
        }
        // The derived value is never `None` (handled above).
        (false, None) => false,
        // Pinned path: exact convergence, including an explicit `-1` pin.
        (true, want) => current != want,
    };
    if !rewrite {
        return Ok(());
    }

    let value = match desired {
        Some(mib) => format!("{mib}MB"),
        None => "-1".to_string(),
    };
    alter_system_set_cap(&config.superuser, &value).await?;
    info!(
        current_mib = ?current,
        desired = %value,
        pinned = is_pin,
        "slot-recovery: max_slot_wal_keep_size drifted from intent — rewrote via ALTER SYSTEM + reload"
    );
    Ok(())
}

/// The cap PostgreSQL is currently enforcing, read from
/// `pg_settings.setting` (always the raw value in the GUC's base unit — MB
/// here — never unit-formatted the way `SHOW` is). Outer `None` when Postgres
/// isn't answering; inner `None` when the GUC is `-1` (unlimited).
async fn current_effective_cap_mib(superuser: &str) -> Option<Option<u64>> {
    let out = psql(
        superuser,
        "SELECT setting FROM pg_catalog.pg_settings WHERE name = 'max_slot_wal_keep_size'",
    )
    .await
    .ok()?;
    Some(parse_pg_size_to_mib(out.trim()))
}

/// `ALTER SYSTEM SET max_slot_wal_keep_size` + `pg_reload_conf()`. Separate
/// `-c` flags because `ALTER SYSTEM` refuses to run inside psql's implicit
/// multi-statement transaction (same shape as reconcile.rs's
/// `force_live_archive_gucs`). Works on standbys — auto.conf is written
/// without WAL. `value` is produced by this module (`{n}MB` or `-1`), never
/// operator input, but quote-escape anyway.
async fn alter_system_set_cap(superuser: &str, value: &str) -> Result<()> {
    let set_sql = format!(
        "ALTER SYSTEM SET max_slot_wal_keep_size = '{}';",
        value.replace('\'', "''")
    );
    let out = Command::new("psql")
        .args([
            "-U",
            superuser,
            "-d",
            "postgres",
            "-h",
            "/var/run/postgresql",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            &set_sql,
            "-c",
            "SELECT pg_reload_conf();",
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await
        .context("spawn psql for ALTER SYSTEM cap rewrite")?;
    if !out.status.success() {
        anyhow::bail!(
            "ALTER SYSTEM cap rewrite failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

// ====================================================================
// Pure helpers (unit-tested)
// ====================================================================

/// Parse a one-column `psql -tAXq` result into trimmed, non-empty row values.
fn parse_slot_name_rows(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Patroni's `slot_name_from_member_name`: lowercase, `-`/`.` → `_`, any other
/// non-`[a-z0-9_]` → `uXXXX` (decimal Unicode code point), truncated to 63
/// characters (`slot_name[0:63]` in `patroni/dcs/__init__.py`). Kept faithful
/// so our member-slot set matches the names Patroni actually creates — without
/// the truncation, a long member name would map to a set entry no real slot
/// ever matches and its lost slot would be skipped forever.
fn slot_name_from_member_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.to_lowercase().chars() {
        if out.len() >= 63 {
            break;
        }
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
            out.push(c);
        } else if c == '-' || c == '.' {
            out.push('_');
        } else {
            out.push_str(&format!("u{:04}", c as u32));
        }
    }
    out.truncate(63);
    out
}

/// Parse a PostgreSQL size string to MiB. `-1` (or any negative) → `None`,
/// meaning unlimited. PostgreSQL size units are powers of 1024, and a bare
/// integer is MB for this GUC.
fn parse_pg_size_to_mib(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.starts_with('-') {
        return None;
    }
    let (num, mult_kib) = if let Some(n) = s.strip_suffix("kB") {
        (n, 1u64)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix("GB") {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("TB") {
        (n, 1024 * 1024 * 1024)
    } else {
        (s, 1024) // bare = MB for this GUC
    };
    let kib = num.trim().parse::<u64>().ok()?.checked_mul(mult_kib)?;
    Some(kib / 1024)
}

/// Whether a derived cap is different enough from the current effective GUC to
/// be worth a rewrite + reload. Always rewrite when the current value is
/// unlimited (`-1` → `None`) — a bound is an unconditional improvement over
/// none. Otherwise only when it moved by more than `hysteresis_pct` of the
/// current value.
fn should_repatch_cap(current_mib: Option<u64>, desired_mib: u64, hysteresis_pct: u64) -> bool {
    match current_mib {
        None => true,
        Some(current) => {
            let delta = current.abs_diff(desired_mib);
            delta * 100 > current.max(1) * hysteresis_pct
        }
    }
}

async fn psql(superuser: &str, sql: &str) -> Result<String> {
    // `-d postgres` explicitly: psql's default database is the user name, so a
    // customized PATRONI_SUPERUSER_USERNAME would otherwise target a database
    // that doesn't exist (same as self_heal.rs's psql calls).
    let out = Command::new("psql")
        .args([
            "-U",
            superuser,
            "-d",
            "postgres",
            "-h",
            "/var/run/postgresql",
            "-v",
            "ON_ERROR_STOP=1",
            "-tAXq",
            "-c",
            sql,
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await
        .context("spawn psql")?;
    if !out.status.success() {
        anyhow::bail!(
            "psql failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slot_rows_trims_and_drops_blanks() {
        assert_eq!(
            parse_slot_name_rows("postgres_1\npostgres_2\n\n  postgres_3  \n"),
            vec!["postgres_1", "postgres_2", "postgres_3"]
        );
        assert!(parse_slot_name_rows("\n  \n").is_empty());
    }

    #[test]
    fn member_slot_name_matches_patroni_rule() {
        // The names this cluster actually uses: START_REPLICATION SLOT
        // "postgres_1" for member postgres-1 (seen in the live logs).
        assert_eq!(slot_name_from_member_name("postgres-1"), "postgres_1");
        assert_eq!(slot_name_from_member_name("Postgres-2"), "postgres_2");
        assert_eq!(slot_name_from_member_name("node.a"), "node_a");
        // Already-valid names pass through untouched.
        assert_eq!(slot_name_from_member_name("pg_x9"), "pg_x9");
        // Exotic chars become uXXXX (a plus sign, code point 43).
        assert_eq!(slot_name_from_member_name("a+b"), "au0043b");
    }

    #[test]
    fn member_slot_name_truncates_to_63_like_patroni() {
        // Patroni slices the mapped name to 63 chars (dcs/__init__.py,
        // `slot_name[0:63]`) — NAMEDATALEN-1. Without matching that, a long
        // member name maps to a set entry no real slot matches and its lost
        // slot is never repaired.
        let long = "m".repeat(80);
        let mapped = slot_name_from_member_name(&long);
        assert_eq!(mapped.len(), 63);
        assert_eq!(mapped, "m".repeat(63));
        // A uXXXX expansion crossing the boundary is cut mid-token, exactly
        // like Python's slice.
        let mut name = "m".repeat(60);
        name.push('+');
        assert_eq!(slot_name_from_member_name(&name).len(), 63);
        assert!(slot_name_from_member_name(&name).ends_with("u00"));
    }

    #[test]
    fn parse_pg_size_units_are_powers_of_1024() {
        assert_eq!(parse_pg_size_to_mib("512MB"), Some(512));
        assert_eq!(parse_pg_size_to_mib("512GB"), Some(512 * 1024));
        assert_eq!(parse_pg_size_to_mib("1TB"), Some(1024 * 1024));
        assert_eq!(parse_pg_size_to_mib("1048576kB"), Some(1024));
        // Bare integer is MB for this GUC.
        assert_eq!(parse_pg_size_to_mib("387000"), Some(387000));
        // -1 (and any negative) means unlimited.
        assert_eq!(parse_pg_size_to_mib("-1"), None);
        assert_eq!(parse_pg_size_to_mib("garbage"), None);
    }

    #[test]
    fn repatch_when_current_is_missing_or_unlimited() {
        // None models both "unset" and "-1 / unlimited" — a bound always wins.
        assert!(should_repatch_cap(None, 387_000, 10));
    }

    #[test]
    fn no_repatch_within_hysteresis() {
        // 5% drift under a 10% band — leave it, don't churn a reload.
        assert!(!should_repatch_cap(Some(400_000), 380_000, 10));
        // 20% drift clears the band.
        assert!(should_repatch_cap(Some(400_000), 320_000, 10));
    }

    #[test]
    fn repatch_triggers_on_real_growth() {
        // Leader's data grew; free space shrank; cap should fall materially.
        assert!(should_repatch_cap(Some(387_000), 250_000, 10));
    }

    #[test]
    fn repair_backoff_suppresses_recreate_loops() {
        let now = Instant::now();
        let backoff = Duration::from_secs(900);
        assert!(!repair_is_backed_off(None, now, backoff));
        assert!(repair_is_backed_off(
            Some(now - Duration::from_secs(899)),
            now,
            backoff
        ));
        assert!(!repair_is_backed_off(
            Some(now - Duration::from_secs(900)),
            now,
            backoff
        ));
    }
}
