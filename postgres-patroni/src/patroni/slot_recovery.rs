//! Leader-side slot recovery and live `max_slot_wal_keep_size` reconcile.
//!
//! Bounding `max_slot_wal_keep_size` stops a lagging member's replication slot
//! from filling the leader's disk (see [`super::config`]), but the bound alone
//! is a one-way door: when a slot exceeds the cap PostgreSQL invalidates it
//! (`wal_status='lost'`) and frees the WAL — the leader survives — but nothing
//! puts the slot back. This watcher closes that loop, leader-only:
//!
//! 1. **Recreate invalidated slots.** Patroni 4.1.0's `load_replication_slots`
//!    never selects `wal_status`, so it neither notices the invalidation nor
//!    recreates the slot, and PostgreSQL refuses to let the standby re-stream on
//!    it (`walsender.c` `StartReplication` acquires with `error_if_invalid=true`
//!    → `slot.c` "can no longer access replication slot"). The standby would sit
//!    on `restore_command` (S3) forever and — being >`maximum_lag_on_failover`
//!    behind — could never be promoted, i.e. silent loss of HA. Dropping the
//!    `lost` slot returns it to Patroni's create-if-missing path, which
//!    recreates it, and the standby resumes streaming. The drop is safe
//!    precisely because an invalidated slot can't be acquired: it is never
//!    `active`, so `pg_drop_replication_slot` can't fail with "slot is active".
//!    We only drop slots whose names match current cluster members, so a user's
//!    own slot is never touched.
//!
//! 2. **Track free space.** `Config::from_env` sizes the cap once at startup.
//!    A long-lived leader whose data grows drifts toward a cap that's generous
//!    relative to *current* free space. Each cycle re-derives the cap from live
//!    `statvfs` and PATCHes Patroni `/config` when it drifts — DCS wins over the
//!    local boot param and Patroni owns the reload, so there's no
//!    `postgresql.auto.conf` fight. Skipped when the operator pinned
//!    `POSTGRES_MAX_SLOT_WAL_KEEP_SIZE`.

use super::config::{
    derive_slot_keep_mib, slot_keep_operator_override, volume_total_and_free_mib,
};
use super::reconcile::{local_node_is_leader, send_patch, wait_for_patroni_rest, PATRONI_REST};
use super::Config;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{info, warn};

const DEFAULT_POLL_SECONDS: u64 = 60;

/// Only re-PATCH the cap when it moves by more than this fraction of the current
/// value, so ordinary disk churn (temp files, a checkpoint) doesn't trigger a
/// config reload every cycle. A real trend (data growth, a large delete) clears
/// it within a poll or two.
const CAP_REPATCH_HYSTERESIS_PCT: u64 = 10;

/// Spawn the leader-side slot-recovery watcher. Mirrors the self-heal watcher's
/// respawn shape: an outer loop wraps `run` in `spawn` so a panic surfaces as a
/// `JoinError` and respawns rather than taking down patroni-runner.
pub fn spawn(volume_root: String) {
    let poll_secs = std::env::var("SLOT_RECOVERY_POLL_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_POLL_SECONDS);

    info!(
        poll_secs,
        volume_root = %volume_root,
        "slot-recovery: starting leader-side watcher"
    );

    tokio::spawn(async move {
        loop {
            let vr = volume_root.clone();
            let h = tokio::task::spawn(async move { run(vr, poll_secs).await });
            match h.await {
                Ok(Ok(())) => warn!("slot-recovery: run loop returned cleanly — respawning in 5s"),
                Ok(Err(e)) => warn!(error = %e, "slot-recovery: run loop errored — respawning in 5s"),
                Err(e) if e.is_panic() => {
                    warn!(panic = ?e, "slot-recovery: run loop panicked — respawning in 5s")
                }
                Err(e) => warn!(error = %e, "slot-recovery: join error — respawning in 5s"),
            }
            sleep(Duration::from_secs(5)).await;
        }
    });
}

async fn run(volume_root: String, poll_secs: u64) -> Result<()> {
    let config = Config::from_env().context("load config for slot-recovery")?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;

    wait_for_patroni_rest(&client).await?;

    loop {
        // Everything here is a leader-only action: member slots live on the
        // primary, and only the primary retains WAL for them. Re-checked every
        // cycle so a new leader picks the work up within one poll after
        // failover, and an ex-leader drops it just as fast.
        if local_node_is_leader(&client).await {
            if let Err(e) = recreate_lost_slots(&client, &config.superuser).await {
                warn!(error = %e, "slot-recovery: recreate-lost-slots iteration errored (continuing)");
            }
            if let Err(e) = reconcile_cap(&client, &config, &volume_root).await {
                warn!(error = %e, "slot-recovery: cap reconcile iteration errored (continuing)");
            }
        }
        sleep(Duration::from_secs(poll_secs)).await;
    }
}

// ====================================================================
// 1. Recreate invalidated slots
// ====================================================================

async fn recreate_lost_slots(client: &reqwest::Client, superuser: &str) -> Result<()> {
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
        match drop_replication_slot(superuser, &slot).await {
            Ok(()) => info!(
                slot = %slot,
                "slot-recovery: dropped invalidated member slot; Patroni will recreate it and the standby will resume streaming"
            ),
            Err(e) => warn!(slot = %slot, error = %e, "slot-recovery: failed to drop invalidated slot"),
        }
    }
    Ok(())
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

async fn reconcile_cap(client: &reqwest::Client, config: &Config, volume_root: &str) -> Result<()> {
    // An operator who pinned the value owns it — never override a hand-set cap.
    if slot_keep_operator_override().is_some() {
        return Ok(());
    }

    let (total_mib, free_mib) = volume_total_and_free_mib(volume_root);
    let Some(desired_mib) = derive_slot_keep_mib(total_mib, free_mib, config.wal_archive_bucket.is_some())
    else {
        // Volume unmeasurable — leave whatever's in place rather than force `-1`.
        return Ok(());
    };

    let current_mib = current_dcs_cap_mib(client).await;
    if !should_repatch_cap(current_mib, desired_mib, CAP_REPATCH_HYSTERESIS_PCT) {
        return Ok(());
    }

    let patch = json!({
        "postgresql": { "parameters": { "max_slot_wal_keep_size": format!("{desired_mib}MB") } }
    });
    send_patch(client, &patch).await.context("PATCH cap into DCS")?;
    info!(
        current_mib = ?current_mib,
        desired_mib,
        total_mib,
        free_mib,
        "slot-recovery: max_slot_wal_keep_size drifted from live free space — reconciled DCS"
    );
    Ok(())
}

/// The cap currently in Patroni DCS, in MiB. `None` when unset or `-1`
/// (unlimited) — either way a derived numeric cap is an unconditional upgrade.
async fn current_dcs_cap_mib(client: &reqwest::Client) -> Option<u64> {
    let cfg: Value = client
        .get(format!("{PATRONI_REST}/config"))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let raw = cfg
        .get("postgresql")?
        .get("parameters")?
        .get("max_slot_wal_keep_size")?;
    // Patroni may store it as a string ("512GB") or a bare number (MB).
    let s = raw
        .as_str()
        .map(str::to_string)
        .or_else(|| raw.as_i64().map(|n| n.to_string()))?;
    parse_pg_size_to_mib(&s)
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
/// non-`[a-z0-9_]` → `uXXXX` (Unicode code point). Kept faithful so our
/// member-slot set matches the names Patroni actually creates.
fn slot_name_from_member_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
            out.push(c);
        } else if c == '-' || c == '.' {
            out.push('_');
        } else {
            out.push_str(&format!("u{:04}", c as u32));
        }
    }
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

/// Whether a derived cap is different enough from the current DCS value to be
/// worth a reload. Always patch when the current value is missing/unlimited
/// (`None`) — a bound is an unconditional improvement over none. Otherwise only
/// when it moved by more than `hysteresis_pct` of the current value.
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
    let out = Command::new("psql")
        .args([
            "-U",
            superuser,
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
}
