//! pgBackRest backup-watcher daemon (leader-only).
//!
//! Long-running task that triggers pgBackRest base backups based on
//! archiving health. Spawned from patroni-runner once Patroni is healthy.
//! Mirrors postgres-ssl's `pgbackrest-backup-watcher.sh` (PRs #45, #59,
//! plus the cadence-override branch).
//!
//! Triggers (any of):
//!   1. NEEDS_INITIAL_BACKUP — no full on record. Take the first full
//!      immediately so PITR is restorable from this LSN forward.
//!      pgbackrest brackets the base in pg_backup_start/stop and waits
//!      for the closing WAL to archive before declaring success, so a
//!      broken archive_command fails the backup loudly instead of
//!      producing an unrestorable base.
//!   2. Gap recovery — `pg_stat_archiver.failed_count` grew since the
//!      last full's checkpoint (or external `.pgbackrest_gap_pending`
//!      marker is set). Once failures are decisively over (grace
//!      period elapsed since last_failed_time), a fresh full re-anchors
//!      the PITR window.
//!   3. Periodic full every `WAL_BACKUP_FULL_INTERVAL_HOURS` (default
//!      168 = 7 days). `WAL_BACKUP_FULL_INTERVAL_SECONDS` overrides the
//!      hours setting for the e2e harness (bash arithmetic precludes
//!      fractional hours).
//!   4. Periodic diff every `WAL_BACKUP_DIFF_INTERVAL_HOURS` (default
//!      24 hours).
//!   5. WAL heartbeat — every iteration, if standby-check passes, emit
//!      `pg_logical_emit_message(false, ...)` so archive_timeout=60
//!      flushes a segment on idle DBs. Cost ~16MB/min raw, zstd-3
//!      compresses to a handful of KB. Skip if WAL_HEARTBEAT_DISABLED=1.
//!
//! HA: this is a leader-only task. Each iteration re-checks the Patroni
//! local API (`http://localhost:8008/leader`) — replicas get HTTP 503
//! and the iteration becomes a no-op. After failover the new leader's
//! watcher takes over within one poll cycle.
//!
//! State persists at `$PGDATA/.pgbackrest_backup_state` (key=value lines,
//! no JSON dep). The bucket-side `pgbackrest --stanza=main info` is the
//! canonical source of truth for backup history; the local file is a
//! cache that survives restarts. pgBackRest's stanza locks prevent
//! concurrent backups across nodes.

use anyhow::Result;
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

const STATE_FILENAME: &str = ".pgbackrest_backup_state";
const GAP_MARKER_FILENAME: &str = ".pgbackrest_gap_pending";
const REPO_PATH_MARKER: &str = ".pgbackrest_repo_path";
const PATRONI_LEADER_URL: &str = "http://localhost:8008/leader";

/// Knobs read from env at startup. All durations in seconds.
struct WatcherConfig {
    poll_interval: u64,
    initial_poll_interval: u64,
    gap_grace: u64,
    full_interval: u64,
    diff_interval: u64,
    heartbeat_disabled: bool,
}

impl WatcherConfig {
    fn from_env() -> Self {
        let full_hours = env_u64("WAL_BACKUP_FULL_INTERVAL_HOURS", 168);
        let diff_hours = env_u64("WAL_BACKUP_DIFF_INTERVAL_HOURS", 24);
        Self {
            poll_interval: env_u64("WAL_BACKUP_POLL_INTERVAL_SECONDS", 60),
            initial_poll_interval: env_u64("WAL_BACKUP_INITIAL_POLL_SECONDS", 5),
            gap_grace: env_u64("WAL_BACKUP_GAP_RESOLVED_GRACE_SECONDS", 300),
            // WAL_BACKUP_FULL_INTERVAL_SECONDS overrides hours for the e2e
            // harness (bash arithmetic in postgres-ssl precludes fractional
            // hours; we mirror the override here for parity). 0 means "no
            // periodic full" — gap-recovery and NEEDS_INITIAL_BACKUP still
            // fire.
            full_interval: env_u64_optional("WAL_BACKUP_FULL_INTERVAL_SECONDS")
                .unwrap_or(full_hours.saturating_mul(3600)),
            diff_interval: env_u64_optional("WAL_BACKUP_DIFF_INTERVAL_SECONDS")
                .unwrap_or(diff_hours.saturating_mul(3600)),
            heartbeat_disabled: env::var("WAL_HEARTBEAT_DISABLED")
                .ok()
                .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
        }
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    env_u64_optional(key).unwrap_or(default)
}

fn env_u64_optional(key: &str) -> Option<u64> {
    env::var(key).ok().and_then(|s| s.parse::<u64>().ok())
}

/// Snapshot of `pg_stat_archiver` for a single iteration.
#[derive(Default, Clone)]
struct ArchiverStats {
    archived_count: i64,
    failed_count: i64,
    last_failed_epoch: i64,
}

/// Spawn the watcher as a tokio task. Returns immediately. Bails (logs
/// and exits the task) if WAL_ARCHIVE_BUCKET is unset.
pub fn spawn(data_dir: String) {
    if env::var("WAL_ARCHIVE_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        return;
    }

    tokio::spawn(async move {
        if let Err(e) = run(data_dir).await {
            warn!(error = %e, "pgbackrest-watcher: terminated");
        }
    });
}

async fn run(data_dir: String) -> Result<()> {
    let config = WatcherConfig::from_env();
    info!(
        poll = config.poll_interval,
        initial_poll = config.initial_poll_interval,
        full = config.full_interval,
        diff = config.diff_interval,
        gap_grace = config.gap_grace,
        heartbeat_disabled = config.heartbeat_disabled,
        "pgbackrest-watcher: starting"
    );

    // HTTP client used to ask Patroni "are we the leader right now". 5s
    // timeout because the API is local and any latency above that
    // probably means Patroni is wedged — we skip backups in that case
    // anyway via the "not leader" path.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    loop {
        watcher_iteration(&data_dir, &config, &client).await;
        let state_path = format!("{data_dir}/{STATE_FILENAME}");
        let interval = if read_state_field(&state_path, "last_full_at").is_none() {
            config.initial_poll_interval
        } else {
            config.poll_interval
        };
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

async fn watcher_iteration(data_dir: &str, config: &WatcherConfig, client: &reqwest::Client) {
    // Sync per-cluster repo path on every iteration. The marker may not
    // exist on the very first iteration if patroni-runner's bootstrap
    // subshell hasn't run yet; later iterations pick it up.
    sync_repo_path_from_marker(data_dir);

    if !pg_isready().await {
        info!("pgbackrest-watcher: iteration skipped (pg_isready=fail)");
        return;
    }

    // Leader check — every iteration. Replica skips backups; new leader
    // takes over within one poll cycle after failover.
    match is_patroni_leader(client).await {
        Ok(true) => {}
        Ok(false) => {
            info!("pgbackrest-watcher: iteration skipped (not patroni leader)");
            return;
        }
        Err(e) => {
            warn!(error = %e, "pgbackrest-watcher: iteration skipped (patroni /leader unreachable)");
            return;
        }
    }

    // Standby check via pg_is_in_recovery() — second-line guarantee
    // beyond the Patroni leader API. A node could be in standby mode
    // briefly after promotion before pg_is_in_recovery() flips, or
    // could be Patroni-leader but Postgres-recovery during a contended
    // failover. Skip in either case.
    match pg_is_in_recovery().await {
        Ok(true) => {
            info!("pgbackrest-watcher: iteration skipped (pg_is_in_recovery)");
            return;
        }
        Ok(false) => {}
        Err(e) => {
            warn!(error = %e, "pgbackrest-watcher: iteration skipped (in_recovery probe failed)");
            return;
        }
    }

    if !config.heartbeat_disabled {
        emit_wal_heartbeat().await;
    }

    let stats = match refresh_archiver_stats().await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "pgbackrest-watcher: pg_stat_archiver query failed (transient)");
            return;
        }
    };

    let action = decide_action(data_dir, config, &stats);
    match action {
        Action::None { reason } => {
            info!(reason = %reason, "pgbackrest-watcher: no action");
        }
        Action::Full | Action::Diff => {
            run_backup(data_dir, action, &stats).await;
        }
    }
}

fn sync_repo_path_from_marker(data_dir: &str) {
    let marker = format!("{data_dir}/{REPO_PATH_MARKER}");
    if let Ok(value) = fs::read_to_string(&marker) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            env::set_var("PGBACKREST_REPO1_PATH", trimmed);
        }
    }
}

async fn pg_isready() -> bool {
    let res = Command::new("pg_isready")
        .args(["-h", "127.0.0.1", "-p", "5432", "-U", "postgres", "-q"])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .status()
        .await;
    matches!(res, Ok(s) if s.success())
}

/// Patroni's `/leader` returns HTTP 200 only on the leader; non-leaders
/// (and unhealthy nodes) get 503. This is the canonical "am I leader"
/// check used by HAProxy too.
async fn is_patroni_leader(client: &reqwest::Client) -> Result<bool> {
    let resp = client.get(PATRONI_LEADER_URL).send().await?;
    Ok(resp.status() == 200)
}

async fn pg_is_in_recovery() -> Result<bool> {
    let out = Command::new("psql")
        .args([
            "-U",
            "postgres",
            "-h",
            "/var/run/postgresql",
            "-tAXq",
            "-c",
            "SELECT pg_is_in_recovery()",
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "pg_is_in_recovery failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim() == "t")
}

async fn refresh_archiver_stats() -> Result<ArchiverStats> {
    let out = Command::new("psql")
        .args([
            "-U",
            "postgres",
            "-h",
            "/var/run/postgresql",
            "-tAXq",
            "-F",
            " ",
            "-c",
            "SELECT archived_count, failed_count, \
             COALESCE(EXTRACT(EPOCH FROM last_archived_time)::bigint, 0), \
             COALESCE(EXTRACT(EPOCH FROM last_failed_time)::bigint, 0) \
             FROM pg_stat_archiver",
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "pg_stat_archiver query failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        anyhow::bail!("pg_stat_archiver returned empty result");
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 4 {
        anyhow::bail!("pg_stat_archiver malformed: {line}");
    }
    Ok(ArchiverStats {
        archived_count: parts[0].parse().unwrap_or(0),
        failed_count: parts[1].parse().unwrap_or(0),
        last_failed_epoch: parts[3].parse().unwrap_or(0),
    })
}

/// Emit a tiny non-transactional WAL record so archive_timeout=60 has
/// something to flush on idle DBs. Without this, idle Postgres never
/// advances the LSN, archive_timeout never forces a segment switch, and
/// pg_stat_archiver.last_archived_time stalls until the next CHECKPOINT
/// (default 5 min). Failure is non-fatal — a temporary blocked emit
/// just postpones the next switch by one tick. Mirrors postgres-ssl
/// PR #45.
async fn emit_wal_heartbeat() {
    let _ = Command::new("psql")
        .args([
            "-U",
            "postgres",
            "-h",
            "/var/run/postgresql",
            "-tAXq",
            "-c",
            "SELECT pg_logical_emit_message(false, 'rwy_pitr_heartbeat', '')",
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await;
}

#[derive(Debug)]
enum Action {
    Full,
    Diff,
    None { reason: String },
}

fn decide_action(data_dir: &str, config: &WatcherConfig, stats: &ArchiverStats) -> Action {
    let state_path = format!("{data_dir}/{STATE_FILENAME}");
    let gap_marker = format!("{data_dir}/{GAP_MARKER_FILENAME}");

    let last_full =
        read_state_field(&state_path, "last_full_at").and_then(|s| s.parse::<i64>().ok());
    let last_diff =
        read_state_field(&state_path, "last_diff_at").and_then(|s| s.parse::<i64>().ok());
    let last_full_failed = read_state_field(&state_path, "last_full_failed_count")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    let now = now_epoch();

    // NEEDS_INITIAL_BACKUP — no full on record, take it now. PR #59
    // dropped the "archived_count > 0" gate: pgbackrest backup brackets
    // pg_backup_start/stop and waits for the closing WAL to archive
    // before declaring success, so a broken archive_command fails the
    // backup loudly instead of producing an unrestorable base.
    if last_full.is_none() {
        return Action::Full;
    }
    let last_full = last_full.unwrap();

    // Gap recovery — explicit drop marker OR failed_count grew since
    // last full. Either signal indicates archive-push had problems
    // since the last LSN-coordinated baseline; a fresh full re-anchors
    // the PITR window.
    let has_marker = Path::new(&gap_marker).exists();
    let failed_grew = stats.failed_count > last_full_failed;
    if has_marker || failed_grew {
        let last_failed = stats.last_failed_epoch;
        let gap_resolved = last_failed == 0 || (now - last_failed) >= config.gap_grace as i64;
        if gap_resolved {
            return Action::Full;
        }
        return Action::None {
            reason: format!(
                "gap open (marker={has_marker}, failed_grew={failed_grew}, last_failed={}, grace={}s)",
                last_failed, config.gap_grace
            ),
        };
    }

    // Periodic full. full_interval=0 disables periodic fulls (gap +
    // initial still fire).
    if config.full_interval > 0 && now >= last_full + config.full_interval as i64 {
        return Action::Full;
    }

    // Periodic diff.
    if config.diff_interval > 0 {
        let diff_anchor = last_diff.unwrap_or(last_full);
        if now >= diff_anchor + config.diff_interval as i64 {
            return Action::Diff;
        }
    }

    Action::None {
        reason: format!(
            "all gates clean (last_full={last_full}, last_diff={:?}, archived={}, failed={}, last_full_failed={last_full_failed})",
            last_diff, stats.archived_count, stats.failed_count
        ),
    }
}

async fn run_backup(data_dir: &str, action: Action, _stats_pre: &ArchiverStats) {
    let backup_type = match action {
        Action::Full => "full",
        Action::Diff => "diff",
        Action::None { .. } => return,
    };
    info!(backup_type = %backup_type, "pgbackrest-watcher: running backup");

    let res = Command::new("pgbackrest")
        .args(["--stanza=main", "backup", &format!("--type={backup_type}")])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .status()
        .await;

    match res {
        Ok(s) if s.success() => {
            let now = now_epoch();
            let state_path = format!("{data_dir}/{STATE_FILENAME}");
            let gap_marker = format!("{data_dir}/{GAP_MARKER_FILENAME}");
            match backup_type {
                "full" => {
                    let _ = write_state_field(&state_path, "last_full_at", &now.to_string());
                    let _ = write_state_field(&state_path, "last_diff_at", &now.to_string());
                    // Re-read failed_count *after* the backup so a
                    // failure during the backup itself is folded into
                    // the high-water mark; otherwise the next iteration
                    // would see growth and re-trigger immediately.
                    if let Ok(post_stats) = refresh_archiver_stats().await {
                        let _ = write_state_field(
                            &state_path,
                            "last_full_failed_count",
                            &post_stats.failed_count.to_string(),
                        );
                    }
                    if Path::new(&gap_marker).exists() {
                        let _ = fs::remove_file(&gap_marker);
                        info!("pgbackrest-watcher: cleared gap marker");
                    }
                }
                "diff" => {
                    let _ = write_state_field(&state_path, "last_diff_at", &now.to_string());
                }
                _ => {}
            }
            info!(backup_type = %backup_type, "pgbackrest-watcher: backup completed");
            emit_pitr_anchor().await;
        }
        Ok(s) => {
            warn!(status = ?s, backup_type = %backup_type, "pgbackrest-watcher: backup failed (will retry next poll)")
        }
        Err(e) => warn!(error = %e, "pgbackrest-watcher: backup invocation failed"),
    }
}

/// Emits one transactional commit right after a successful backup so the
/// PITR picker has a commit-timestamp anchor to clamp `recovery_target_time`
/// against. Without this, a brand-new cluster with a base backup but zero
/// user commits leaves `pg_last_committed_xact()` and
/// `pg_xact_commit_timestamp(newest_commit_ts_xid from pg_control_checkpoint())`
/// both NULL — the picker has no safe ceiling and any restore target FATALs
/// recovery with "recovery ended before configured recovery target was
/// reached" (it only stops at XLOG_XACT_COMMIT records).
///
/// `transactional=true` produces a real XLOG_XACT_COMMIT record with a
/// commit timestamp, populates `pg_commit_ts/`, and the next checkpoint
/// persists `newest_commit_ts_xid` into pg_control. The picker's
/// GREATEST-of-two-sources query picks it up on the next 30s probe refresh.
///
/// Idempotent: every subsequent backup re-fires the emit. If the cluster
/// already has user commits, the extra anchor is invisible noise (one
/// trivial transaction, no table side effect). Failure is non-fatal — the
/// next iteration's backup retries.
async fn emit_pitr_anchor() {
    let res = Command::new("psql")
        .args([
            "-U",
            "postgres",
            "-h",
            "/var/run/postgresql",
            "-tAXq",
            "-c",
            "SELECT pg_logical_emit_message(true, 'rwy_pitr_anchor', '')",
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await;
    match res {
        Ok(o) if o.status.success() => info!("pgbackrest-watcher: pitr anchor emitted"),
        Ok(o) => warn!(
            status = ?o.status,
            stderr = %String::from_utf8_lossy(&o.stderr),
            "pgbackrest-watcher: pitr anchor emit failed (non-fatal)"
        ),
        Err(e) => warn!(error = %e, "pgbackrest-watcher: pitr anchor invocation failed (non-fatal)"),
    }
}

fn read_state_field(state_path: &str, field: &str) -> Option<String> {
    let content = fs::read_to_string(state_path).ok()?;
    let prefix = format!("{field}=");
    content
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .next_back()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_state_field(state_path: &str, field: &str, value: &str) -> Result<()> {
    let prefix = format!("{field}=");
    let existing = fs::read_to_string(state_path).unwrap_or_default();
    let mut new_lines: Vec<String> = existing
        .lines()
        .filter(|line| !line.starts_with(&prefix))
        .map(|s| s.to_string())
        .collect();
    new_lines.push(format!("{field}={value}"));
    let mut out = new_lines.join("\n");
    out.push('\n');
    fs::write(state_path, out)?;
    Ok(())
}

fn now_epoch() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
