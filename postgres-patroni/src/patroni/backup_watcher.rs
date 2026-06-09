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
//!   2. Gap recovery — a state machine that fires whenever WAL coverage is
//!      diverging from the S3 catalog, regardless of cause. Entry conditions
//!      (any of):
//!        - pgbackrest-archive-push-wrapper.sh dropped a segment and touched
//!          `.pgbackrest_gap_pending` (bucket gone, hard archive-push failure)
//!        - LSN-lag probe found `pg_stat_archiver.last_archived_wal` more
//!          than `WAL_LAG_GAP_THRESHOLD_SEGMENTS` ahead of the catalog max
//!          (async worker silently wedged — queue-max-trip or hung
//!          connection)
//!      Recovery flow once in the state:
//!        - Wait `GAP_RECOVERY_BACKOFF_SECONDS` (default 10 min) for natural
//!          async recovery. Most short Tigris/S3 hiccups self-heal here.
//!        - Still no catalog progress → pkill the async daemon. Foreground
//!          archive-push respawns it on the next WAL switch. Cycle repeats
//!          every `GAP_RECOVERY_BACKOFF_SECONDS` until catalog advances or
//!          the postgres process exits.
//!        - Catalog max > catalog max at detection (proof async pushed to
//!          S3 successfully) → take a diff backup to re-anchor
//!          `latestRestorableAt`, then clear the gap marker.
//!      Diff (not full) is enough because the customer-visible state is
//!      "latestRestorableAt is fresh", which a diff produces in seconds.
//!      Historical missing segments stay missing in the old chain;
//!      retention eventually rolls them off.
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
//! LSN-lag detection: pgBackRest async mode returns archive_command
//! success to Postgres as soon as the WAL segment lands in the local
//! spool, BEFORE the async worker uploads it to S3. If the async worker
//! hangs or hits an unrecoverable upload error, the spool keeps
//! accepting WAL (foreground returns 0) while the S3 catalog stays
//! frozen. archive-push-queue-max eventually drops segments and ALSO
//! returns 0 to Postgres — so failed_count never increments and the
//! archive-push wrapper never sees a non-zero exit.
//!
//! Detection: every iteration, compare pg_stat_archiver.last_archived_wal
//! against the catalog max from `pgbackrest info --output=json` (parsed
//! with serde_json — earlier substring-match extraction had a silent
//! catch-all that collapsed unparseable output into "lag=0", missing real
//! wedges). When lag ≥ `WAL_LAG_GAP_THRESHOLD_SEGMENTS` (default 32 ≈
//! 512 MiB) the watcher enters the gap-recovery state machine described
//! under "Gap recovery" above.
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
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

const STATE_FILENAME: &str = ".pgbackrest_backup_state";
const GAP_MARKER_FILENAME: &str = ".pgbackrest_gap_pending";
const REPO_PATH_MARKER: &str = ".pgbackrest_repo_path";
const PGBACKREST_CONF_FILE: &str = "/etc/pgbackrest/pgbackrest.conf";
const PATRONI_LEADER_URL: &str = "http://localhost:8008/leader";
const PATRONI_CONFIG_URL: &str = "http://localhost:8008/config";
const PATRONI_REPO_PATH_CONFIG_KEY: &str = "pgbackrest_repo1_path";

/// Knobs read from env at startup. All durations in seconds.
struct WatcherConfig {
    poll_interval: u64,
    initial_poll_interval: u64,
    /// Cooldown between gap-recovery actions. After initial detection, the
    /// state machine waits this long for natural async recovery before
    /// kicking the async daemon. Each subsequent pkill cycle also waits
    /// this long before the next pkill. Catalog advance breaks out of the
    /// wait immediately.
    gap_recovery_backoff: u64,
    full_interval: u64,
    diff_interval: u64,
    heartbeat_disabled: bool,
    /// LSN-lag detection — see file header. 32 segments ≈ 512 MiB: far
    /// enough above the steady-state hand-off-vs-upload skew to avoid
    /// false positives, far enough below archive-push-queue-max (default
    /// 5 GiB / 320 segments) to leave headroom for the recovery state
    /// machine to act before the queue actually trips.
    lag_threshold_segments: u64,
}

impl WatcherConfig {
    fn from_env() -> Self {
        let full_hours = env_u64("WAL_BACKUP_FULL_INTERVAL_HOURS", 168);
        let diff_hours = env_u64("WAL_BACKUP_DIFF_INTERVAL_HOURS", 24);
        Self {
            poll_interval: env_u64("WAL_BACKUP_POLL_INTERVAL_SECONDS", 60),
            initial_poll_interval: env_u64("WAL_BACKUP_INITIAL_POLL_SECONDS", 5),
            gap_recovery_backoff: env_u64("WAL_BACKUP_GAP_RECOVERY_BACKOFF_SECONDS", 600),
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
            lag_threshold_segments: env_u64("WAL_LAG_GAP_THRESHOLD_SEGMENTS", 32),
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
    failed_count: i64,
    last_archived_epoch: i64,
    last_failed_epoch: i64,
    /// 24-char hex WAL filename of the most-recent segment Postgres handed
    /// off to the archive process. Empty when `pg_stat_archiver` reports
    /// NULL (cluster just started, never archived). Used by the LSN-lag
    /// probe to compare against `pgbackrest info`'s repo high-water.
    last_archived_wal: String,
    /// 24-char hex WAL filename of the most-recent failed archive attempt.
    /// Sticky until postgres restart — always check `last_failed_epoch >
    /// last_archived_epoch` before acting on it to confirm the failure is
    /// currently active rather than historical.
    last_failed_wal: String,
    /// `0x100000000 / wal_segment_size` — how many WAL segments fit per
    /// XLogId. Default 256 for the standard 16 MiB segsize. Postgres
    /// allows any power-of-2 between 1 MiB and 1 GiB at initdb (via
    /// `initdb --wal-segsize=N`), so hardcoding 256 would mis-scale lag
    /// by `(default / actual)` on non-default clusters. Carried alongside
    /// `failed_count` so every consumer sees a single coherent snapshot.
    segments_per_log_file: u64,
}

/// Default segments-per-XLogId for a 16 MiB wal_segment_size — used as a
/// safe pre-query placeholder before `refresh_archiver_stats` fills the
/// real value. 0x100000000 / 16777216 = 256.
const DEFAULT_SEGMENTS_PER_LOG_FILE: u64 = 256;

/// Spawn the watcher as a tokio task. Returns immediately. Bails (logs
/// and exits the task) if WAL_ARCHIVE_BUCKET is unset.
///
/// Supervisor: `run()` is wrapped in a respawn loop. Each respawn cycle
/// launches one `tokio::task::spawn(run())` task whose lifetime spans the
/// entire watcher main loop (all iterations); if any iteration inside
/// the task panics or returns Err, the task ends, the supervisor logs
/// the cause, and `tokio::time::sleep(5s)` later it spawns a fresh
/// task. The boundary is at *task lifetime* — not per iteration —
/// which gives panic isolation from the rest of patroni-runner (a
/// panic in `run` surfaces as `JoinError::is_panic()` on this side
/// rather than aborting the host process). State that needs to
/// persist across respawns lives on disk (`.pgbackrest_backup_state`
/// + `.pgbackrest_gap_pending`), so a fresh task picks up the
/// in-flight recovery state where the old one left off.
pub fn spawn(data_dir: String) {
    if env::var("WAL_ARCHIVE_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        return;
    }

    tokio::spawn(async move {
        loop {
            let dd = data_dir.clone();
            let h = tokio::task::spawn(async move { run(dd).await });
            match h.await {
                Ok(Ok(())) => {
                    warn!("pgbackrest-watcher: run loop returned cleanly — respawning in 5s")
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "pgbackrest-watcher: run loop errored — respawning in 5s")
                }
                Err(e) if e.is_panic() => {
                    warn!(panic = ?e, "pgbackrest-watcher: run loop panicked — respawning in 5s")
                }
                Err(e) => {
                    warn!(error = %e, "pgbackrest-watcher: join error — respawning in 5s")
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
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
        gap_recovery_backoff = config.gap_recovery_backoff,
        heartbeat_disabled = config.heartbeat_disabled,
        lag_threshold_segments = config.lag_threshold_segments,
        "pgbackrest-watcher: starting"
    );

    // HTTP client used to ask Patroni "are we the leader right now". 5s
    // timeout because the API is local and any latency above that
    // probably means Patroni is wedged — we skip backups in that case
    // anyway via the "not leader" path.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    // Mark that a startup diff is pending. Cleared on first successful backup
    // so it never fires twice per watcher spawn. Seals WAL gaps at the crash
    // boundary (pg_stat_archiver resets on restart, so lag-detection can't see
    // historical pre-crash gaps) without waiting for the periodic diff interval.
    let state_path = format!("{data_dir}/{STATE_FILENAME}");
    let _ = write_state_field(&state_path, "startup_diff_pending", "1");

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

    if finalize_pending_wal_regression_migration_if_needed(data_dir, client).await {
        return;
    }

    if let Err(e) = converge_repo_path_with_patroni_dcs(data_dir, client).await {
        warn!(error = %e, "pgbackrest-watcher: failed to converge repo path with Patroni DCS");
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

    // Gap-recovery state machine — detects WAL/catalog divergence and
    // drives the kick-and-diff sequence. Runs every iteration;
    // pgbackrest info is cheap enough that throttling isn't worth the
    // false-negative window the earlier throttled version introduced.
    gap_recovery_step(data_dir, config, client, &stats).await;

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
    // Combined query: pg_stat_archiver columns + wal_segment_size in 8 KiB
    // pages (pg_settings's PGC_INTERNAL unit for this GUC). Folding both
    // into a single round-trip keeps watcher iteration cheap.
    //
    // COALESCE WAL names to '-' so split_whitespace() doesn't collapse an
    // empty trailing column into the preceding one and corrupt the bind.
    // The sentinel is stripped below.
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
            "SELECT failed_count, \
             COALESCE(EXTRACT(EPOCH FROM last_archived_time)::bigint, 0), \
             COALESCE(EXTRACT(EPOCH FROM last_failed_time)::bigint, 0), \
             COALESCE(last_archived_wal, '-'), \
             COALESCE(last_failed_wal, '-'), \
             (SELECT setting::bigint FROM pg_settings WHERE name = 'wal_segment_size') \
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
    if parts.len() < 6 {
        anyhow::bail!("pg_stat_archiver malformed: {line}");
    }
    let sentinel_to_empty = |s: &str| {
        if s == "-" {
            String::new()
        } else {
            s.to_string()
        }
    };
    // wal_segment_size is reported as the number of 8 KiB pages per
    // segment. Convert to bytes, then to segments-per-XLogId. Fall back
    // to the default (256 = 16 MiB segsize) on any parse failure; that's
    // strictly better than panicking, and a wrong-by-power-of-2 scaling
    // gets the watcher firing on a slightly different threshold rather
    // than not at all.
    let segments_per_log_file = parts[5]
        .parse::<u64>()
        .ok()
        .filter(|&p| p > 0)
        .and_then(|pages_per_segment| {
            let bytes = pages_per_segment.checked_mul(8 * 1024)?;
            if bytes == 0 || 0x1_0000_0000u64 % bytes != 0 {
                return None;
            }
            Some(0x1_0000_0000u64 / bytes)
        })
        .unwrap_or(DEFAULT_SEGMENTS_PER_LOG_FILE);
    Ok(ArchiverStats {
        failed_count: parts[0].parse().unwrap_or(0),
        last_archived_epoch: parts[1].parse().unwrap_or(0),
        last_failed_epoch: parts[2].parse().unwrap_or(0),
        last_archived_wal: sentinel_to_empty(parts[3]),
        last_failed_wal: sentinel_to_empty(parts[4]),
        segments_per_log_file,
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

/// 24-char hex WAL filename → absolute segment count.
/// `segments_per_log_file` = `0x100000000 / wal_segment_size`; the caller
/// (always sourced from `ArchiverStats.segments_per_log_file`) carries
/// the cluster's actual GUC value rather than the 256 hardcode, so a
/// cluster initdb'd with `--wal-segsize=N` (legal between 1 MiB and 1
/// GiB, powers of 2) computes lag correctly. Returns None on malformed
/// input so callers short-circuit. Strict shape + hex check avoids
/// letting a stray non-hex character feed `u64::from_str_radix` and
/// surface a parse error to the watcher loop.
fn segment_to_number(wal: &str, segments_per_log_file: u64) -> Option<u64> {
    if wal.len() != 24 || !wal.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let log = u64::from_str_radix(&wal[8..16], 16).ok()?;
    let seg = u64::from_str_radix(&wal[16..24], 16).ok()?;
    Some(log * segments_per_log_file + seg)
}

/// Probe state passed to `gap_recovery_step` and returned for tests.
/// `catalog_max` is None when the catalog has no archive entries for the
/// current timeline yet (fresh stanza); `Err` from `probe_catalog_max`
/// is "pgbackrest info failed" — leave state alone.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CatalogProbe {
    catalog_max: Option<String>,
    lag: u64,
}

/// Parse `pgbackrest info --output=json` and extract the lex-max archive
/// segment whose first 8 hex chars match `tl_hex`. Returns None when the
/// timeline isn't in the catalog (legitimate "fresh stanza" case).
/// Bubbles up serde_json errors so the caller distinguishes "no entries"
/// from "couldn't parse" — the previous substring-based extractor
/// collapsed both into "lag=0" and silently masked real wedges.
fn parse_catalog_max(info_json: &str, tl_hex: &str) -> Result<Option<String>> {
    let v: serde_json::Value = serde_json::from_str(info_json)?;
    let stanzas = v
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("pgbackrest info JSON top-level not an array"))?;
    let mut best: Option<String> = None;
    for stanza in stanzas {
        let Some(archives) = stanza.get("archive").and_then(|a| a.as_array()) else {
            continue;
        };
        for archive in archives {
            let Some(max) = archive.get("max").and_then(|m| m.as_str()) else {
                continue;
            };
            if max.len() != 24 || !max.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            if &max[..8] != tl_hex {
                continue;
            }
            match &best {
                None => best = Some(max.to_string()),
                Some(prev) if max > prev.as_str() => best = Some(max.to_string()),
                _ => {}
            }
        }
    }
    Ok(best)
}

/// Run `pgbackrest info` and extract the catalog max for the current
/// timeline plus computed lag. Returns Err on transient probe failure
/// (`pgbackrest info` errored or stdout didn't parse) so the caller
/// leaves any in-flight state alone.
async fn probe_catalog_max(stats: &ArchiverStats) -> Result<CatalogProbe> {
    if stats.last_archived_wal.is_empty() {
        return Ok(CatalogProbe::default());
    }
    let handed_off = segment_to_number(&stats.last_archived_wal, stats.segments_per_log_file)
        .ok_or_else(|| {
            anyhow::anyhow!("malformed last_archived_wal: {}", stats.last_archived_wal)
        })?;

    let out = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("pgbackrest")
            .args(["--stanza=main", "--repo=1", "info", "--output=json"])
            .env_remove("PGHOST")
            .env_remove("PGPORT")
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("pgbackrest info timed out"))??;
    if !out.status.success() {
        anyhow::bail!(
            "pgbackrest info exited non-zero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let info_json = String::from_utf8_lossy(&out.stdout);
    let tl_hex = &stats.last_archived_wal[..8];
    let catalog_max = parse_catalog_max(&info_json, tl_hex)?;
    let lag = match catalog_max
        .as_deref()
        .and_then(|w| segment_to_number(w, stats.segments_per_log_file))
    {
        Some(n) => handed_off.saturating_sub(n),
        None => 0,
    };
    Ok(CatalogProbe { catalog_max, lag })
}

fn repo_path_marker(data_dir: &str) -> String {
    format!("{data_dir}/{REPO_PATH_MARKER}")
}

fn spool_status_dir(data_dir: &str) -> String {
    format!("{data_dir}/pgbackrest-spool/archive/main/out")
}

/// Rewrite repo1-path in pgbackrest.conf. The marker/env path is authoritative;
/// this is defense-in-depth for bare `docker exec pgbackrest info` diagnostics.
fn rewrite_pgbackrest_conf_path(data_dir: &str, path: &str) -> Result<()> {
    let conf_path = Path::new(PGBACKREST_CONF_FILE);
    if !conf_path.exists() {
        return Ok(());
    }
    let existing = fs::read_to_string(conf_path)?;
    let mut replaced = false;
    let mut out = String::new();
    for line in existing.lines() {
        if line.starts_with("repo1-path=") {
            out.push_str(&format!("repo1-path={path}\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(&format!("repo1-path={path}\n"));
    }

    let tmp = format!("{data_dir}/.pgbackrest_conf.{}", std::process::id());
    fs::write(&tmp, out)?;
    let copy_res = fs::copy(&tmp, conf_path);
    let _ = fs::remove_file(&tmp);
    copy_res?;
    Ok(())
}

/// Source-of-truth setter for the active archive path. Updates the marker
/// atomically (read by archive-push on every WAL), rewrites pgbackrest.conf for
/// bare-shell diagnostics, and updates this watcher's process env. Idempotent.
fn apply_active_path(data_dir: &str, path: &str) -> Result<()> {
    if path.is_empty() {
        anyhow::bail!("empty repo path");
    }
    let marker = repo_path_marker(data_dir);
    let tmp = format!("{marker}.{}", std::process::id());
    fs::write(&tmp, format!("{path}\n"))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o640))?;
    if let Err(e) = fs::rename(&tmp, &marker) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = rewrite_pgbackrest_conf_path(data_dir, path) {
        warn!(error = %e, "pgbackrest-watcher: apply_active_path: failed to rewrite repo1-path in pgbackrest.conf (marker + env are authoritative)");
    }
    env::set_var("PGBACKREST_REPO1_PATH", path);
    Ok(())
}

async fn patroni_dcs_repo_path(client: &reqwest::Client) -> Result<Option<String>> {
    let resp = client.get(PATRONI_CONFIG_URL).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Patroni /config GET returned {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    Ok(json
        .get(PATRONI_REPO_PATH_CONFIG_KEY)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned))
}

async fn patch_patroni_dcs_repo_path(client: &reqwest::Client, path: &str) -> Result<()> {
    let mut body = serde_json::Map::new();
    body.insert(
        PATRONI_REPO_PATH_CONFIG_KEY.to_string(),
        serde_json::Value::String(path.to_string()),
    );
    let resp = client.patch(PATRONI_CONFIG_URL).json(&body).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Patroni /config PATCH returned {}", resp.status());
    }
    Ok(())
}

/// HA path-drift guard: replicas inherit `.pgbackrest_repo_path` during base
/// backup, but later leader-side WAL_REGRESSION migrations do not propagate
/// through PG state. The leader broadcasts the active path through Patroni's
/// DCS, and each leader iteration adopts it before archive activity.
async fn reset_local_backup_state_for_new_archive_path(data_dir: &str) -> Result<()> {
    let state_path = format!("{data_dir}/{STATE_FILENAME}");
    let failed_anchor = refresh_archiver_stats()
        .await
        .map(|s| s.failed_count)
        .unwrap_or(0);
    write_state_field(
        &state_path,
        "last_full_failed_count",
        &failed_anchor.to_string(),
    )?;
    write_state_field(&state_path, "last_full_at", "")?;
    write_state_field(&state_path, "last_diff_at", "")?;
    write_state_field(&state_path, "last_lag_detected_at", "0")?;
    write_state_field(&state_path, "catalog_max_at_detection", "")?;
    write_state_field(&state_path, "last_force_recovery_at", "0")?;
    write_state_field(&state_path, "force_attempts", "0")?;
    let gap_marker = format!("{data_dir}/{GAP_MARKER_FILENAME}");
    let _ = fs::remove_file(&gap_marker);
    Ok(())
}

async fn converge_repo_path_with_patroni_dcs(
    data_dir: &str,
    client: &reqwest::Client,
) -> Result<()> {
    let state_path = format!("{data_dir}/{STATE_FILENAME}");
    if read_state_field(&state_path, "wal_regression_pending_new_path").is_some() {
        return Ok(());
    }

    let active = env::var("PGBACKREST_REPO1_PATH")
        .ok()
        .filter(|s| !s.is_empty());
    let dcs = patroni_dcs_repo_path(client).await?;
    match (active, dcs) {
        (Some(active), Some(dcs_path)) if active != dcs_path => {
            info!(active = %active, dcs_path = %dcs_path, "pgbackrest-watcher: adopting repo path from Patroni DCS");
            reset_local_backup_state_for_new_archive_path(data_dir).await?;
            apply_active_path(data_dir, &dcs_path)?;
        }
        (Some(active), None) => {
            patch_patroni_dcs_repo_path(client, &active).await?;
            info!(repo_path = %active, "pgbackrest-watcher: seeded repo path into Patroni DCS");
        }
        (None, Some(dcs_path)) => {
            info!(dcs_path = %dcs_path, "pgbackrest-watcher: adopting repo path from Patroni DCS");
            reset_local_backup_state_for_new_archive_path(data_dir).await?;
            apply_active_path(data_dir, &dcs_path)?;
        }
        _ => {}
    }
    Ok(())
}

fn write_state_field_required(state_path: &str, field: &str, value: &str) -> bool {
    if let Err(e) = write_state_field(state_path, field, value) {
        warn!(error = %e, field = %field, "pgbackrest-watcher: state-write failed; refusing unsafe archive-path migration step");
        return false;
    }
    true
}

async fn async_daemon_running() -> bool {
    matches!(
        Command::new("pgrep")
            .args(["-f", "archive-push:async"])
            .status()
            .await,
        Ok(s) if s.success()
    )
}

fn clean_spool_status_files(data_dir: &str) -> Result<()> {
    let dir = spool_status_dir(data_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.ends_with(".error") || name.ends_with(".ok") {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Finalizes a marker-flipped WAL_REGRESSION migration by forcing pgBackRest's
/// async daemon to re-read repo1-path, clearing stale async status files from
/// the old path, broadcasting the path through Patroni DCS, then clearing the
/// pending sentinel. If any step fails, the pending field remains and the next
/// iteration retries finalization rather than trusting stale `.ok/.error` files.
async fn finalize_wal_regression_migration(
    data_dir: &str,
    client: &reqwest::Client,
    path: &str,
) -> bool {
    info!("pgbackrest-watcher: wal-regression: kicking async daemon to pick up new repo1-path");
    kick_async_daemon().await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && async_daemon_running().await {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Escalate to SIGKILL if SIGTERM wasn't enough — a daemon still alive here
    // may write stale `.ok/.error` status files for the old repo path after the
    // new archive path is active. Matches the shell implementation's
    // kill-then-drain logic.
    if async_daemon_running().await {
        warn!("pgbackrest-watcher: wal-regression: async daemon did not exit on SIGTERM; sending SIGKILL");
        let _ = Command::new("pkill")
            .args(["-KILL", "-f", "archive-push:async"])
            .status()
            .await;
        let kill_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < kill_deadline && async_daemon_running().await {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if async_daemon_running().await {
            warn!("pgbackrest-watcher: wal-regression: async daemon still alive after SIGKILL; will retry finalization");
            return false;
        }
    }

    if let Err(e) = clean_spool_status_files(data_dir) {
        warn!(error = %e, "pgbackrest-watcher: wal-regression: failed to clean async status files; will retry finalization");
        return false;
    }
    if let Err(e) = patch_patroni_dcs_repo_path(client, path).await {
        warn!(error = %e, "pgbackrest-watcher: wal-regression: failed to publish repo path to Patroni DCS; will retry finalization");
        return false;
    }

    let gap_marker = format!("{data_dir}/{GAP_MARKER_FILENAME}");
    let _ = fs::remove_file(&gap_marker);
    let state_path = format!("{data_dir}/{STATE_FILENAME}");
    if !write_state_field_required(&state_path, "wal_regression_pending_new_path", "") {
        warn!("pgbackrest-watcher: wal-regression: failed to clear pending migration marker; will retry finalization");
        return false;
    }

    info!(path = %path, "pgbackrest-watcher: wal-regression: migration finalized; async status cache cleared");
    true
}

async fn finalize_pending_wal_regression_migration_if_needed(
    data_dir: &str,
    client: &reqwest::Client,
) -> bool {
    let state_path = format!("{data_dir}/{STATE_FILENAME}");
    let Some(pending) = read_state_field(&state_path, "wal_regression_pending_new_path") else {
        return false;
    };
    if env::var("PGBACKREST_REPO1_PATH").ok().as_deref() != Some(pending.as_str()) {
        return false;
    }
    info!(pending = %pending, "pgbackrest-watcher: wal-regression: finalizing pending archive-path migration");
    finalize_wal_regression_migration(data_dir, client, &pending).await;
    true
}

fn wal_has_async_archive_duplicate_error(
    data_dir: &str,
    wal: &str,
    segments_per_log_file: u64,
) -> bool {
    if segment_to_number(wal, segments_per_log_file).is_none() {
        return false;
    }
    let path = format!("{}/{}.error", spool_status_dir(data_dir), wal);
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.lines().next().map(ToOwned::to_owned))
        .as_deref()
        == Some("45")
}

/// Async-spool probe for ArchiveDuplicateError (exit 45). Catches quiet-DB
/// regressions where pgBackRest async wrote an error but foreground
/// archive_command has not re-run, so pg_stat_archiver is still NULL/stale.
fn probe_async_duplicate_error(
    data_dir: &str,
    catalog_max: &str,
    segments_per_log_file: u64,
) -> Option<String> {
    let dir_s = spool_status_dir(data_dir);
    let dir = Path::new(&dir_s);
    if !dir.is_dir() {
        return None;
    }
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.ends_with(".error"))
        })
        .collect();
    files.sort();

    let catalog_n = if catalog_max.is_empty() {
        None
    } else {
        segment_to_number(catalog_max, segments_per_log_file)
    };
    for path in files {
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(base) = name.strip_suffix(".error") else {
            continue;
        };
        if segment_to_number(base, segments_per_log_file).is_none() {
            continue;
        }
        let first_line = fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.lines().next().map(ToOwned::to_owned));
        if first_line.as_deref() != Some("45") {
            continue;
        }
        if !catalog_max.is_empty() {
            if &base[..8] != &catalog_max[..8] {
                continue;
            }
            let Some(c_n) = catalog_n else {
                continue;
            };
            let Some(d_n) = segment_to_number(base, segments_per_log_file) else {
                continue;
            };
            if d_n > c_n {
                continue;
            }
        }
        return Some(base.to_string());
    }
    None
}

async fn repo_max_for_wal(wal: &str, current: &str) -> Result<Option<String>> {
    if !current.is_empty() && current.len() == 24 && &current[..8] == &wal[..8] {
        return Ok(Some(current.to_string()));
    }
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        Command::new("pgbackrest")
            .args(["--stanza=main", "--repo=1", "info", "--output=json"])
            .env_remove("PGHOST")
            .env_remove("PGPORT")
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("pgbackrest info timed out"))??;
    if !out.status.success() {
        anyhow::bail!(
            "pgbackrest info exited non-zero: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let info_json = String::from_utf8_lossy(&out.stdout);
    parse_catalog_max(&info_json, &wal[..8])
}

/// Self-heals a WAL_REGRESSION condition by migrating archiving to a fresh S3
/// path suffix (cluster-SYSID-<epoch>). The old path and all backups remain in
/// S3; mono's PITR restore UI enumerates cluster-* histories so orphaned
/// backups remain selectable. Epoch suffixes avoid collisions after repeated
/// volume snapshot rollbacks to pre-self-heal PGDATA.
async fn migrate_to_new_archive_path(
    data_dir: &str,
    client: &reqwest::Client,
    stats: &ArchiverStats,
) -> bool {
    let state_path = format!("{data_dir}/{STATE_FILENAME}");
    let Ok(old_path) = env::var("PGBACKREST_REPO1_PATH") else {
        warn!("pgbackrest-watcher: wal-regression: PGBACKREST_REPO1_PATH unset (marker missing, no env override); cannot migrate");
        return false;
    };
    if old_path.is_empty() {
        warn!("pgbackrest-watcher: wal-regression: PGBACKREST_REPO1_PATH empty; cannot migrate");
        return false;
    }

    let orig_path = match read_state_field(&state_path, "wal_regression_orig_path") {
        Some(path) => path,
        None => {
            if !write_state_field_required(&state_path, "wal_regression_orig_path", &old_path) {
                return false;
            }
            old_path.clone()
        }
    };

    let new_path = match read_state_field(&state_path, "wal_regression_pending_new_path") {
        Some(path) => path,
        None => {
            let path = format!("{orig_path}-{}", now_epoch());
            if !write_state_field_required(&state_path, "wal_regression_pending_new_path", &path) {
                return false;
            }
            path
        }
    };

    if old_path == new_path {
        info!(new_path = %new_path, "pgbackrest-watcher: wal-regression: finalizing pending archive-path migration");
        return finalize_wal_regression_migration(data_dir, client, &new_path).await;
    }

    info!(old_path = %old_path, new_path = %new_path, "pgbackrest-watcher: wal-regression: migrating archive path; old backups preserved at former path");

    let failed_anchor = refresh_archiver_stats()
        .await
        .map(|s| s.failed_count)
        .unwrap_or(stats.failed_count);
    if !write_state_field_required(
        &state_path,
        "last_full_failed_count",
        &failed_anchor.to_string(),
    ) || !write_state_field_required(&state_path, "last_full_at", "")
        || !write_state_field_required(&state_path, "last_diff_at", "")
        || !write_state_field_required(&state_path, "last_lag_detected_at", "0")
        || !write_state_field_required(&state_path, "catalog_max_at_detection", "")
        || !write_state_field_required(&state_path, "last_force_recovery_at", "0")
        || !write_state_field_required(&state_path, "force_attempts", "0")
    {
        return false;
    }

    if let Err(e) = apply_active_path(data_dir, &new_path) {
        warn!(error = %e, new_path = %new_path, "pgbackrest-watcher: wal-regression: failed to apply new archive path; will retry");
        return false;
    }

    if !finalize_wal_regression_migration(data_dir, client, &new_path).await {
        return false;
    }

    info!(new_path = %new_path, "pgbackrest-watcher: wal-regression: state reset; next iteration will initialize stanza and take full backup");
    true
}

/// Returns true (and calls `migrate_to_new_archive_path`) when the data is
/// consistent with WAL_REGRESSION: `last_failed_wal` is at or before the S3
/// catalog max on the same timeline, and the failure is currently active
/// (`last_failed_epoch > last_archived_epoch`).
async fn check_wal_regression(
    data_dir: &str,
    client: &reqwest::Client,
    stats: &ArchiverStats,
    catalog_max: &str,
) -> bool {
    if stats.last_failed_wal.is_empty() || stats.last_failed_wal.len() != 24 {
        return false;
    }
    // Require the file-specific async status code 45 (ArchiveDuplicateError);
    // last_failed_wal <= catalog_max alone only proves the failure is at/before
    // the repo frontier, not that pgBackRest refused a different-checksum WAL.
    if !wal_has_async_archive_duplicate_error(
        data_dir,
        &stats.last_failed_wal,
        stats.segments_per_log_file,
    ) {
        return false;
    }
    // Guard: failure must be currently active (more recent than last success).
    // pg_stat_archiver.last_failed_wal is sticky until postgres restart.
    if stats.last_failed_epoch == 0 || stats.last_failed_epoch <= stats.last_archived_epoch {
        return false;
    }
    let catalog_max = match repo_max_for_wal(&stats.last_failed_wal, catalog_max).await {
        Ok(Some(s)) => s,
        Ok(None) => return false,
        Err(e) => {
            warn!(error = %e, "pgbackrest-watcher: wal-regression catalog probe failed");
            return false;
        }
    };
    if catalog_max.len() != 24 || &stats.last_failed_wal[..8] != &catalog_max[..8] {
        return false;
    }
    let Some(failed_n) = segment_to_number(&stats.last_failed_wal, stats.segments_per_log_file)
    else {
        return false;
    };
    let Some(repo_n) = segment_to_number(&catalog_max, stats.segments_per_log_file) else {
        return false;
    };
    if failed_n > repo_n {
        return false;
    }
    info!(
        failed_wal = %stats.last_failed_wal,
        catalog_max = %catalog_max,
        failed_count = stats.failed_count,
        "pgbackrest-watcher: wal-regression: detected (failed_wal <= catalog_max on same timeline) — self-healing"
    );
    migrate_to_new_archive_path(data_dir, client, stats).await;
    true
}

/// Clears all gap-recovery state (marker file + state fields). Called
/// after a successful diff (recovery confirmed) or full (re-anchors the
/// baseline). Re-reads pg_stat_archiver to fold any failed pushes during
/// the backup we just ran into the failed_count anchor — without this
/// the next iteration would see failed_count > last_full_failed_count
/// and immediately re-fire detection.
///
/// `fallback_failed_count` is what we anchor with when the post-backup
/// refresh errors (pg restart, brief unavailability right after a long
/// backup). Without a fallback we'd anchor at 0 and the next iteration
/// would see `stats.failed_count > 0` and immediately re-enter recovery
/// on a stale baseline. Callers pass the pre-backup `failed_count` from
/// the iteration's own stats.
async fn clear_gap_recovery_state(data_dir: &str, reason: &str, fallback_failed_count: i64) {
    let state_path = format!("{data_dir}/{STATE_FILENAME}");
    let gap_marker = format!("{data_dir}/{GAP_MARKER_FILENAME}");
    let failed_count = refresh_archiver_stats()
        .await
        .map(|s| s.failed_count)
        .unwrap_or(fallback_failed_count);
    let _ = write_state_field(
        &state_path,
        "last_full_failed_count",
        &failed_count.to_string(),
    );
    let _ = write_state_field(&state_path, "last_lag_detected_at", "0");
    let _ = write_state_field(&state_path, "catalog_max_at_detection", "");
    let _ = write_state_field(&state_path, "last_force_recovery_at", "0");
    let _ = write_state_field(&state_path, "force_attempts", "0");
    if Path::new(&gap_marker).exists() {
        let _ = fs::remove_file(&gap_marker);
        info!(reason = %reason, "pgbackrest-watcher: gap-recovery state cleared");
    }
}

/// Kicks the async daemon. Foreground archive-push respawns it on the
/// next WAL switch (heartbeat + archive_timeout=60 guarantees one within
/// ~60s). Crashed-daemon case: pkill is a no-op, respawn happens
/// regardless. Hung-daemon case: pkill removes the stuck process so the
/// next archive-push can spawn a fresh one. Spool is safe to disrupt —
/// pgBackRest re-uploads from pg_wal on respawn.
///
/// Target: the literal substring "archive-push:async" in the cmdline.
/// pgBackRest spawns the async daemon via
/// cfgExecParam(cfgCmdArchivePush, cfgCmdRoleAsync, ...) and
/// cfgParseCommandRoleName (src/config/parse.c) encodes the role with a
/// colon — argv[1] of the spawned process becomes "archive-push:async".
/// The foreground caller (runs as archive_command, exits in ~300ms) has
/// "archive-push" *without* a colon, so the colon disambiguates: pkill
/// matches the long-lived async daemon but never the foreground call.
/// Verify in a running container with `pgrep -af archive-push:async`.
async fn kick_async_daemon() {
    let _ = Command::new("pkill")
        .args(["-f", "archive-push:async"])
        .status()
        .await;
}

/// Recovery state machine. Replaces the old "wait for grace then take a
/// full" path with: detect → wait 10 min → pkill → wait 10 min → pkill →
/// … → (catalog advances) → take diff → clear. Repeats pkill every
/// backoff window during an extended upstream outage; the diff fires the
/// instant the catalog actually advances past the detection point, which
/// is the only conclusive proof async has resumed pushing to S3.
///
/// Called every iteration. Idempotent — re-entering the function while
/// already in recovery just advances the timers / inspects current
/// catalog max.
/// Snapshot of every input the gap-recovery decision depends on. All
/// fields are plain data — no async, no Result, no I/O. The orchestrator
/// (`gap_recovery_step`) does the I/O to collect this; `decide_gap_recovery`
/// is pure so it can be exhaustively unit-tested without filesystem or
/// subprocess mocks.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GapRecoveryInputs {
    now: i64,
    lag: u64,
    /// `""` when the catalog has no entries for the current timeline (fresh
    /// stanza, never archived). Non-empty values are 24-char hex WAL names.
    catalog_max: String,
    handoff_wal: String,
    failed_count: i64,
    marker_present: bool,
    /// `0` when no recovery cycle is in flight (or wrapper-touched marker
    /// the watcher hasn't seen yet).
    detected_at: i64,
    /// `""` when not yet back-filled.
    catalog_at_detection: String,
    /// `0` when never force-recovered this cycle.
    last_force_recovery_at: i64,
    force_attempts: u64,
    last_full_failed: i64,
    threshold_segments: u64,
    backoff_seconds: u64,
    /// `0x100000000 / wal_segment_size` — carried from `ArchiverStats`
    /// so the segment-arithmetic side-channel sees the cluster's actual
    /// segsize instead of the 256 hardcode. See `segment_to_number`.
    segments_per_log_file: u64,
}

/// What the orchestrator should do in this iteration. Exactly one variant
/// per state-machine transition; back-fill writes are their own variants
/// so the orchestrator never has to combine multiple side-effects in one
/// dispatch. Trade-off: a wrapper-touched-marker entry path takes up to
/// two extra iterations before kick logic engages (back-fill detected_at,
/// then back-fill catalog_at_detection). 60-120s of added latency before
/// the first kick is acceptable for the simplification.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GapRecoveryAction {
    /// Not in recovery; no entry condition met.
    NoOp,
    /// First detection — touch marker, write all four state fields,
    /// log entry banner. `catalog_at_detection` is the current catalog
    /// max (may be `""` if the timeline has no archive entries yet).
    Detect { catalog_at_detection: String },
    /// In recovery, `detected_at == 0` (wrapper touched the marker
    /// without the watcher having recorded its own detection time).
    /// Stamp `last_lag_detected_at = now`.
    BackFillDetectedAt,
    /// In recovery, `catalog_at_detection == ""` and `catalog_max != ""`.
    /// Stamp the baseline so the next iteration's advance check has
    /// something to compare against.
    BackFillCatalogAtDetection { value: String },
    /// Catalog has advanced past the detection point — proof the async
    /// daemon is pushing again. Run a diff to anchor the restore point.
    TakeRecoveryDiff,
    /// Backoff has elapsed since the last action. pkill the async
    /// daemon, bump attempts, stamp `last_force_recovery_at = now`.
    KickAsyncDaemon { attempt: u64 },
    /// In recovery; nothing to do this iteration.
    Wait,
}

/// Pure state-machine decision. No I/O, no async. Driven by a
/// `GapRecoveryInputs` snapshot the orchestrator builds; returns the
/// single action to dispatch this iteration. Every branch in the
/// production code path corresponds to one variant of
/// `GapRecoveryAction` and is unit-tested below.
fn decide_gap_recovery(inp: &GapRecoveryInputs) -> GapRecoveryAction {
    if !inp.marker_present {
        // Not in recovery. Two independent entry conditions:
        //   - LSN lag ≥ threshold (async wedge / queue-max-trip)
        //   - failed_count > anchor (foreground hard failure)
        let lag_trigger = inp.lag >= inp.threshold_segments;
        let failed_trigger = inp.failed_count > inp.last_full_failed;
        if lag_trigger || failed_trigger {
            return GapRecoveryAction::Detect {
                catalog_at_detection: inp.catalog_max.clone(),
            };
        }
        return GapRecoveryAction::NoOp;
    }

    // Marker present (we're in recovery). Back-fills run first so the
    // state file is normalised before the advance/kick logic.
    if inp.detected_at == 0 {
        return GapRecoveryAction::BackFillDetectedAt;
    }
    if inp.catalog_at_detection.is_empty() {
        if inp.catalog_max.is_empty() {
            // Stanza has no archive entries yet; nothing to anchor
            // against. The kick path is also gated on having a non-empty
            // baseline (no way to detect "catalog advanced" otherwise),
            // so just wait.
            return GapRecoveryAction::Wait;
        }
        return GapRecoveryAction::BackFillCatalogAtDetection {
            value: inp.catalog_max.clone(),
        };
    }

    // Advance check: catalog moved past the detection baseline → diff.
    let curr = segment_to_number(&inp.catalog_max, inp.segments_per_log_file);
    let det = segment_to_number(&inp.catalog_at_detection, inp.segments_per_log_file);
    let advanced = matches!((curr, det), (Some(c), Some(d)) if c > d);
    if advanced {
        return GapRecoveryAction::TakeRecoveryDiff;
    }

    // No advance. Backoff elapsed since the last action → kick.
    let last_action_at = inp.detected_at.max(inp.last_force_recovery_at);
    let since_action = inp.now.saturating_sub(last_action_at);
    if since_action >= inp.backoff_seconds as i64 {
        return GapRecoveryAction::KickAsyncDaemon {
            attempt: inp.force_attempts + 1,
        };
    }

    GapRecoveryAction::Wait
}

async fn gap_recovery_step(
    data_dir: &str,
    config: &WatcherConfig,
    client: &reqwest::Client,
    stats: &ArchiverStats,
) {
    let now = now_epoch();
    let state_path = format!("{data_dir}/{STATE_FILENAME}");
    let gap_marker = format!("{data_dir}/{GAP_MARKER_FILENAME}");

    let probe = match probe_catalog_max(stats).await {
        Ok(p) => {
            // Probe succeeded — reset consecutive-failure tracking.
            let _ = write_state_field(&state_path, "probe_fail_since", "0");
            let _ = write_state_field(&state_path, "probe_fail_wal_at_start", "");
            p
        }
        Err(e) => {
            // S3 blind-spot kick: probe_catalog_max requires S3 reads. When
            // S3 is completely unreachable (e.g. Tigris outage), every probe
            // times out — the lag-threshold path never fires and a hung async
            // worker is never killed even though the spool keeps accumulating
            // WAL behind a queue-max-trip.
            //
            // If the blackout exceeds gap_recovery_backoff AND
            // last_archived_wal has advanced (postgres is still handing WAL
            // to the spool), pkill the async daemon. The hung S3 connection
            // is torn down; archive_command respawns the async process on the
            // next WAL switch, which retries once S3 recovers. The clock
            // resets after each kick so we don't pkill every iteration.
            warn!(error = %e, "pgbackrest-watcher: pgbackrest info probe failed; leaving gap-recovery state unchanged");

            let probe_fail_since: i64 = read_state_field(&state_path, "probe_fail_since")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let probe_fail_wal: String =
                read_state_field(&state_path, "probe_fail_wal_at_start").unwrap_or_default();

            if probe_fail_since == 0 {
                let _ = write_state_field(&state_path, "probe_fail_since", &now.to_string());
                let _ = write_state_field(
                    &state_path,
                    "probe_fail_wal_at_start",
                    &stats.last_archived_wal,
                );
            } else {
                let blackout_s = now - probe_fail_since;
                let wal_advanced = !stats.last_archived_wal.is_empty()
                    && !probe_fail_wal.is_empty()
                    && stats.last_archived_wal != probe_fail_wal;
                if blackout_s >= config.gap_recovery_backoff as i64 && wal_advanced {
                    warn!(
                        blackout_s = blackout_s,
                        wal_at_start = %probe_fail_wal,
                        wal_now = %stats.last_archived_wal,
                        "pgbackrest-watcher: S3 unreachable while postgres keeps handing WAL — kicking async daemon (blind-spot kick)"
                    );
                    kick_async_daemon().await;
                    // Reset so the next kick fires after another full backoff.
                    let _ =
                        write_state_field(&state_path, "probe_fail_since", &now.to_string());
                    let _ = write_state_field(
                        &state_path,
                        "probe_fail_wal_at_start",
                        &stats.last_archived_wal,
                    );
                }
            }
            return;
        }
    };
    let catalog_max = probe.catalog_max.unwrap_or_default();

    // Async-spool WAL_REGRESSION probe runs before pg_stat-based detection.
    // pgBackRest async writes `<wal>.error` before foreground archive_command
    // has necessarily re-run, so pg_stat_archiver can still be NULL/stale on
    // quiet DBs. With empty catalog_max (common after postgres restart), exit
    // 45 is sufficient proof: the segment already exists remotely with a
    // different checksum.
    if let Some(dup_seg) =
        probe_async_duplicate_error(data_dir, &catalog_max, stats.segments_per_log_file)
    {
        info!(dup_seg = %dup_seg, catalog_max = %catalog_max, "pgbackrest-watcher: wal-regression: async spool ArchiveDuplicateError — self-healing");
        migrate_to_new_archive_path(data_dir, client, stats).await;
        return;
    }

    // WAL_REGRESSION is structural (archive path conflict after volume
    // rollback), not a transient async lag. Migrate to a new non-conflicting
    // repo path and return so the next iteration starts clean.
    if check_wal_regression(data_dir, client, stats, &catalog_max).await {
        return;
    }

    let inp = GapRecoveryInputs {
        now,
        lag: probe.lag,
        catalog_max,
        handoff_wal: stats.last_archived_wal.clone(),
        failed_count: stats.failed_count,
        marker_present: Path::new(&gap_marker).exists(),
        detected_at: read_state_field(&state_path, "last_lag_detected_at")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0),
        catalog_at_detection: read_state_field(&state_path, "catalog_max_at_detection")
            .unwrap_or_default(),
        last_force_recovery_at: read_state_field(&state_path, "last_force_recovery_at")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0),
        force_attempts: read_state_field(&state_path, "force_attempts")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
        last_full_failed: read_state_field(&state_path, "last_full_failed_count")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0),
        threshold_segments: config.lag_threshold_segments,
        backoff_seconds: config.gap_recovery_backoff,
        segments_per_log_file: stats.segments_per_log_file,
    };

    match decide_gap_recovery(&inp) {
        GapRecoveryAction::NoOp => {}

        GapRecoveryAction::Detect {
            catalog_at_detection,
        } => {
            if let Err(e) = fs::write(&gap_marker, "") {
                warn!(error = %e, "pgbackrest-watcher: failed to write gap marker");
                return;
            }
            let _ = write_state_field(&state_path, "last_lag_detected_at", &now.to_string());
            let _ = write_state_field(
                &state_path,
                "catalog_max_at_detection",
                &catalog_at_detection,
            );
            let _ = write_state_field(&state_path, "last_force_recovery_at", "0");
            let _ = write_state_field(&state_path, "force_attempts", "0");
            info!(
                handoff = %inp.handoff_wal,
                catalog_max = %inp.catalog_max,
                lag = inp.lag,
                threshold = inp.threshold_segments,
                failed_count = inp.failed_count,
                last_full_failed = inp.last_full_failed,
                backoff_seconds = inp.backoff_seconds,
                "pgbackrest-watcher: entering gap-recovery, first pkill after backoff if catalog hasn't advanced"
            );
        }

        GapRecoveryAction::BackFillDetectedAt => {
            let _ = write_state_field(&state_path, "last_lag_detected_at", &now.to_string());
        }

        GapRecoveryAction::BackFillCatalogAtDetection { value } => {
            let _ = write_state_field(&state_path, "catalog_max_at_detection", &value);
        }

        GapRecoveryAction::TakeRecoveryDiff => {
            info!(
                catalog_at_detection = %inp.catalog_at_detection,
                catalog_max = %inp.catalog_max,
                force_attempts = inp.force_attempts,
                "pgbackrest-watcher: gap-recovery — catalog advanced, taking diff to anchor restore point"
            );
            // Branch on run_backup's actual return — looking up
            // last_diff_at as a "did it succeed?" proxy would match an
            // unrelated periodic diff that happened to land seconds
            // earlier, and incorrectly clear state on a recovery diff
            // that actually failed. stats.failed_count is the
            // pre-backup fallback for clear_gap_recovery_state's
            // post-backup refresh.
            if run_backup(data_dir, Action::Diff, stats).await {
                clear_gap_recovery_state(
                    data_dir,
                    "cleared by gap-recovery diff",
                    stats.failed_count,
                )
                .await;
            } else {
                warn!("pgbackrest-watcher: gap-recovery diff failed; retry on next iteration");
            }
        }

        GapRecoveryAction::KickAsyncDaemon { attempt } => {
            let stuck_min = (now - inp.detected_at) / 60;
            info!(
                catalog_at_detection = %inp.catalog_at_detection,
                handoff = %inp.handoff_wal,
                lag = inp.lag,
                attempt,
                stuck_min,
                "pgbackrest-watcher: gap-recovery — catalog still frozen, pkill async daemon"
            );
            kick_async_daemon().await;
            let _ = write_state_field(&state_path, "last_force_recovery_at", &now.to_string());
            let _ = write_state_field(&state_path, "force_attempts", &attempt.to_string());
        }

        GapRecoveryAction::Wait => {
            // No log line during the wait — per-iteration tracing in the
            // surrounding watcher_iteration loop already surfaces enough
            // state; printing the same "still waiting" message every
            // minute for 10 minutes per backoff cycle is just spam.
        }
    }
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

    // WAL_REGRESSION migration in-flight: async daemon was killed and the
    // marker was flipped, but spool cleanup or DCS broadcast may not have
    // completed yet. Stale old-path .ok files would let archive_command
    // return success without uploading WAL to the new path, producing an
    // unrestorable backup. Block all backups until finalization clears the
    // field. finalize_pending_wal_regression_migration_if_needed runs at
    // the top of every watcher_iteration and retries until it succeeds.
    if read_state_field(&state_path, "wal_regression_pending_new_path").is_some() {
        return Action::None {
            reason: "wal_regression migration pending finalization".to_string(),
        };
    }

    // NEEDS_INITIAL_BACKUP — no full on record, take it now. pgbackrest
    // backup brackets the base in pg_backup_start/stop and waits for the
    // closing WAL to archive before declaring success, so a broken
    // archive_command fails the backup loudly instead of producing an
    // unrestorable base.
    if last_full.is_none() {
        return Action::Full;
    }
    let last_full = last_full.unwrap();

    // Gap-recovery state machine owns the .pgbackrest_gap_pending marker.
    // While the marker is present, decide_action stays silent —
    // gap_recovery_step already ran this iteration and either took a
    // diff, kicked the async daemon, or is waiting on the backoff.
    // Racing a periodic full on top of an in-flight recovery would burn
    // a full at the worst time (mid-outage).
    if Path::new(&gap_marker).exists() {
        return Action::None {
            reason: "gap-recovery in progress (state machine owns the marker)".to_string(),
        };
    }

    // Periodic full. full_interval=0 disables periodic fulls (gap +
    // initial still fire).
    if config.full_interval > 0 && now >= last_full + config.full_interval as i64 {
        return Action::Full;
    }

    // Startup diff — fires once per watcher spawn (startup_diff_pending is
    // written in run() before the loop and cleared in run_backup on success).
    // Ensures WAL lost at the crash boundary is sealed before the next periodic
    // diff would otherwise fire. Gap-marker check above already returned if gap
    // recovery is running; the startup diff fires on the first clean iteration
    // after gap recovery clears its own marker.
    if read_state_field(&state_path, "startup_diff_pending").as_deref() == Some("1") {
        return Action::Diff;
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
            "all gates clean (last_full={last_full}, last_diff={:?}, failed={}, last_full_failed={last_full_failed})",
            last_diff, stats.failed_count
        ),
    }
}

/// Run `pgbackrest backup --type=<type>` and apply the post-success
/// bookkeeping (state-file timestamps, gap-recovery state clear on a
/// full, PITR anchor emit). Returns `true` if the backup succeeded so
/// callers can branch on the actual exit code rather than guessing from
/// `last_diff_at` proxies that could match an unrelated periodic diff
/// completed seconds earlier.
async fn run_backup(data_dir: &str, action: Action, stats_pre: &ArchiverStats) -> bool {
    let backup_type = match action {
        Action::Full => "full",
        Action::Diff => "diff",
        Action::None { .. } => return false,
    };
    info!(backup_type = %backup_type, "pgbackrest-watcher: running backup");

    let mut res = Command::new("pgbackrest")
        .args(["--stanza=main", "backup", &format!("--type={backup_type}")])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .status()
        .await;

    // Exit 55 = FileMissingError: backup.info absent — stanza was never
    // initialized (bootstrap stanza-create failed or timed out on first
    // boot). Run stanza-create now and retry once; the watcher loop handles
    // subsequent retries.
    if res.as_ref().ok().and_then(|s| s.code()) == Some(55) {
        info!("pgbackrest-watcher: stanza not initialized (exit 55), running stanza-create then retrying");
        let sc = Command::new("pgbackrest")
            .args(["--stanza=main", "stanza-create"])
            .env_remove("PGHOST")
            .env_remove("PGPORT")
            .status()
            .await;
        match sc {
            Ok(s) if s.success() => info!("pgbackrest-watcher: stanza-create completed"),
            Ok(s) => warn!(status = ?s, "pgbackrest-watcher: stanza-create failed"),
            Err(e) => warn!(error = %e, "pgbackrest-watcher: stanza-create invocation failed"),
        }
        res = Command::new("pgbackrest")
            .args(["--stanza=main", "backup", &format!("--type={backup_type}")])
            .env_remove("PGHOST")
            .env_remove("PGPORT")
            .status()
            .await;
    }

    match res {
        Ok(s) if s.success() => {
            let now = now_epoch();
            let state_path = format!("{data_dir}/{STATE_FILENAME}");
            match backup_type {
                "full" => {
                    let _ = write_state_field(&state_path, "last_full_at", &now.to_string());
                    let _ = write_state_field(&state_path, "last_diff_at", &now.to_string());
                    // clear_gap_recovery_state refreshes pg_stat_archiver
                    // and writes last_full_failed_count itself — folds any
                    // failure-during-backup into the anchor so the next
                    // iteration doesn't re-fire detection. Pre-backup
                    // failed_count is the fallback if the post-backup
                    // refresh errors (pg restart, brief unavailability).
                    clear_gap_recovery_state(
                        data_dir,
                        "cleared by full backup",
                        stats_pre.failed_count,
                    )
                    .await;
                }
                "diff" => {
                    let _ = write_state_field(&state_path, "last_diff_at", &now.to_string());
                }
                _ => {}
            }
            // Consume the startup-diff flag so it never fires again this run,
            // regardless of what type of backup completed (full subsumes it;
            // diff fulfills it; gap-recovery diff also clears it so the next
            // iteration doesn't double-fire).
            let _ = write_state_field(&state_path, "startup_diff_pending", "0");
            info!(backup_type = %backup_type, "pgbackrest-watcher: backup completed");
            emit_pitr_anchor().await;
            true
        }
        Ok(s) => {
            warn!(status = ?s, backup_type = %backup_type, "pgbackrest-watcher: backup failed (will retry next poll)");
            false
        }
        Err(e) => {
            warn!(error = %e, "pgbackrest-watcher: backup invocation failed");
            false
        }
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
        Err(e) => {
            warn!(error = %e, "pgbackrest-watcher: pitr anchor invocation failed (non-fatal)")
        }
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

#[cfg(test)]
mod tests {
    use super::{
        decide_gap_recovery, parse_catalog_max, probe_async_duplicate_error, segment_to_number,
        wal_has_async_archive_duplicate_error, GapRecoveryAction, GapRecoveryInputs,
    };
    use std::fs;

    /// Baseline `GapRecoveryInputs` for tests — every field at its
    /// "nothing happening" default. Individual tests override only the
    /// fields they care about. Mirrors `WatcherConfig::from_env`
    /// defaults: threshold=32 segments, backoff=600s.
    fn base_inputs() -> GapRecoveryInputs {
        GapRecoveryInputs {
            now: 10_000,
            lag: 0,
            catalog_max: String::new(),
            handoff_wal: String::new(),
            failed_count: 0,
            marker_present: false,
            detected_at: 0,
            catalog_at_detection: String::new(),
            last_force_recovery_at: 0,
            force_attempts: 0,
            last_full_failed: 0,
            threshold_segments: 32,
            backoff_seconds: 600,
            segments_per_log_file: 256,
        }
    }

    #[test]
    fn segment_to_number_real_values() {
        // Real cache values (admin monitor warning detail). Lag must match
        // the segment math the monitor uses (713 segments for both).
        assert_eq!(
            segment_to_number("000000010000001D000000ED", 256),
            Some(7661)
        );
        assert_eq!(
            segment_to_number("000000010000001B00000024", 256),
            Some(6948)
        );
        assert_eq!(
            segment_to_number("000000010000001D000000ED", 256).unwrap()
                - segment_to_number("000000010000001B00000024", 256).unwrap(),
            713
        );
    }

    #[test]
    fn segment_to_number_rejects_malformed() {
        assert_eq!(segment_to_number("", 256), None);
        assert_eq!(segment_to_number("not24", 256), None);
        assert_eq!(segment_to_number("abcdefghijklmnopqrstuvwx", 256), None); // 24 non-hex
        assert_eq!(segment_to_number("000000010000001B0000002", 256), None); // 23 chars
        assert_eq!(segment_to_number("000000010000001B0000002__", 256), None); // 25 chars
    }

    #[test]
    fn segment_to_number_non_default_segsize() {
        // wal_segment_size = 32 MiB → segments_per_log_file = 128. A WAL
        // file name like 00000001 00000002 0000005A means "log 2, offset
        // 0x5A" = 2*128 + 0x5A under 32 MiB segsize (vs. 2*256 + 0x5A
        // = 602 under the 16 MiB hardcode).
        assert_eq!(
            segment_to_number("00000001000000020000005A", 128),
            Some(2 * 128 + 0x5A)
        );
        // wal_segment_size = 1 MiB → segments_per_log_file = 4096.
        assert_eq!(
            segment_to_number("00000001000000010000000A", 4096),
            Some(4096 + 0x0A)
        );
        // Two segments across an XLogId boundary scale linearly with
        // segments_per_log_file: same hex inputs, different divisor.
        let a16 = segment_to_number("000000010000000200000010", 256).unwrap();
        let b16 = segment_to_number("000000010000000100000010", 256).unwrap();
        assert_eq!(a16 - b16, 256);
        let a32 = segment_to_number("000000010000000200000010", 128).unwrap();
        let b32 = segment_to_number("000000010000000100000010", 128).unwrap();
        assert_eq!(a32 - b32, 128);
    }

    #[test]
    fn parse_catalog_max_picks_matching_timeline() {
        // Single-timeline case (the AlgoEd-shape input).
        let info = r#"[{"archive":[{"id":"18-1","min":"000000010000000000000001","max":"000000010000001B00000024"}],"backup":[]}]"#;
        assert_eq!(
            parse_catalog_max(info, "00000001").unwrap(),
            Some("000000010000001B00000024".to_string())
        );
        // Wrong timeline → None.
        assert_eq!(parse_catalog_max(info, "00000002").unwrap(), None);
    }

    #[test]
    fn parse_catalog_max_handles_multi_timeline() {
        // Multi-timeline post-failover catalog. Match the requested one.
        let info = r#"[{"archive":[
            {"id":"18-1","min":"000000010000000000000001","max":"000000010000001A00000010"},
            {"id":"18-2","min":"000000020000000000000005","max":"000000020000000200000050"}
        ],"backup":[]}]"#;
        assert_eq!(
            parse_catalog_max(info, "00000002").unwrap(),
            Some("000000020000000200000050".to_string())
        );
        assert_eq!(
            parse_catalog_max(info, "00000001").unwrap(),
            Some("000000010000001A00000010".to_string())
        );
    }

    #[test]
    fn parse_catalog_max_returns_none_when_timeline_absent() {
        let info = r#"[{"archive":[],"backup":[]}]"#;
        assert_eq!(parse_catalog_max(info, "00000001").unwrap(), None);
    }

    #[test]
    fn parse_catalog_max_errors_on_malformed_json() {
        // The bug this whole change fixes: previous text-match extractor
        // silently returned None ("treat as no entries") for unparseable
        // output, which the caller collapsed to lag=0 — masking real
        // wedges. parse_catalog_max now bubbles a serde_json error so the
        // caller logs "probe failed" and leaves state alone instead of
        // pretending everything's fine.
        assert!(parse_catalog_max("not json at all", "00000001").is_err());
        assert!(parse_catalog_max("{\"not\": \"an array\"}", "00000001").is_err());
    }

    #[test]
    fn parse_catalog_max_skips_null_max() {
        // pgBackRest can emit `"max": null` when a stanza exists but the
        // archive section has been wiped. Treat as "no entry for this
        // timeline" rather than a parse error.
        let info = r#"[{"archive":[{"id":"18-1","min":null,"max":null}],"backup":[]}]"#;
        assert_eq!(parse_catalog_max(info, "00000001").unwrap(), None);
    }

    #[test]
    fn parse_catalog_max_handles_pretty_printed_json() {
        // Review feedback: JSON output may include spaces/newlines. The old
        // substring matcher would silently return None here.
        let info = r#"[
          {
            "archive": [
              { "id": "18-1", "min": "000000010000000000000001", "max": "000000010000001B00000024" }
            ],
            "backup": []
          }
        ]"#;
        assert_eq!(
            parse_catalog_max(info, "00000001").unwrap(),
            Some("000000010000001B00000024".to_string())
        );
    }

    // ---- decide_gap_recovery: state-machine transitions ----
    //
    // The orchestrator (`gap_recovery_step`) is a thin dispatcher around
    // `decide_gap_recovery`. Every state-machine transition has its own
    // variant on `GapRecoveryAction`, and each variant has a test below
    // that pins the conditions that produce it. Future refactors should
    // change the rule set in `decide_gap_recovery` only; the
    // orchestrator's match arms are pure I/O.

    #[test]
    fn not_in_recovery_lag_below_threshold_is_noop() {
        let inp = GapRecoveryInputs {
            lag: 31,
            catalog_max: "000000010000000000000020".into(),
            ..base_inputs()
        };
        assert_eq!(decide_gap_recovery(&inp), GapRecoveryAction::NoOp);
    }

    #[test]
    fn not_in_recovery_lag_at_threshold_triggers_detect() {
        let inp = GapRecoveryInputs {
            lag: 32,
            catalog_max: "000000010000000000000020".into(),
            ..base_inputs()
        };
        assert_eq!(
            decide_gap_recovery(&inp),
            GapRecoveryAction::Detect {
                catalog_at_detection: "000000010000000000000020".into(),
            }
        );
    }

    #[test]
    fn not_in_recovery_failed_count_grew_triggers_detect_without_lag() {
        // Foreground hard-failure path: archive_command returns non-zero,
        // postgres's last_archived_wal never advances, so lag stays at 0
        // even though failed_count climbs.
        let inp = GapRecoveryInputs {
            lag: 0,
            failed_count: 13,
            last_full_failed: 0,
            ..base_inputs()
        };
        assert_eq!(
            decide_gap_recovery(&inp),
            GapRecoveryAction::Detect {
                catalog_at_detection: String::new(),
            }
        );
    }

    #[test]
    fn not_in_recovery_failed_count_equal_anchor_is_noop() {
        // Boundary: failed_count *equal* to the anchor isn't growth.
        // (Tests the `>` vs `>=` mistake that's easy to make.)
        let inp = GapRecoveryInputs {
            lag: 0,
            failed_count: 13,
            last_full_failed: 13,
            ..base_inputs()
        };
        assert_eq!(decide_gap_recovery(&inp), GapRecoveryAction::NoOp);
    }

    #[test]
    fn marker_present_missing_detected_at_back_fills_first() {
        // Wrapper touched the marker, watcher's first iteration sees it
        // and hasn't recorded its own detection time yet. Stamp
        // detected_at = now. Other fields irrelevant on this branch —
        // back-fill takes priority over everything else.
        let inp = GapRecoveryInputs {
            marker_present: true,
            detected_at: 0,
            catalog_at_detection: String::new(),
            catalog_max: "000000010000000000000050".into(),
            lag: 100,
            ..base_inputs()
        };
        assert_eq!(
            decide_gap_recovery(&inp),
            GapRecoveryAction::BackFillDetectedAt
        );
    }

    #[test]
    fn marker_present_empty_catalog_max_and_no_baseline_waits() {
        // Fresh stanza: marker is set, detected_at recorded, but the
        // catalog still has no archive entries for this timeline, so
        // there's nothing to anchor against. Just wait — the catalog
        // will populate once archiving starts, and the next iteration
        // will back-fill catalog_at_detection.
        let inp = GapRecoveryInputs {
            marker_present: true,
            detected_at: 9_000,
            catalog_at_detection: String::new(),
            catalog_max: String::new(),
            ..base_inputs()
        };
        assert_eq!(decide_gap_recovery(&inp), GapRecoveryAction::Wait);
    }

    #[test]
    fn marker_present_empty_baseline_with_non_empty_catalog_back_fills() {
        // Audit #3 fix codified: we only back-fill catalog_at_detection
        // once catalog_max is non-empty, so the baseline captures a real
        // segment to compare against on the next iteration.
        let inp = GapRecoveryInputs {
            marker_present: true,
            detected_at: 9_000,
            catalog_at_detection: String::new(),
            catalog_max: "000000010000000000000050".into(),
            ..base_inputs()
        };
        assert_eq!(
            decide_gap_recovery(&inp),
            GapRecoveryAction::BackFillCatalogAtDetection {
                value: "000000010000000000000050".into(),
            }
        );
    }

    #[test]
    fn marker_present_catalog_advanced_triggers_diff() {
        // The whole point of the state machine: catalog moved past the
        // detection baseline → async is pushing again → diff to anchor
        // the restore point.
        let inp = GapRecoveryInputs {
            marker_present: true,
            detected_at: 9_000,
            catalog_at_detection: "000000010000000000000050".into(),
            catalog_max: "000000010000000000000051".into(),
            ..base_inputs()
        };
        assert_eq!(
            decide_gap_recovery(&inp),
            GapRecoveryAction::TakeRecoveryDiff
        );
    }

    #[test]
    fn marker_present_catalog_equal_to_baseline_within_backoff_waits() {
        // No advance, backoff not yet elapsed. Don't kick.
        let inp = GapRecoveryInputs {
            now: 10_000,
            marker_present: true,
            detected_at: 9_700, // 300s ago (< 600s backoff)
            catalog_at_detection: "000000010000000000000050".into(),
            catalog_max: "000000010000000000000050".into(),
            ..base_inputs()
        };
        assert_eq!(decide_gap_recovery(&inp), GapRecoveryAction::Wait);
    }

    #[test]
    fn marker_present_catalog_equal_to_baseline_at_backoff_kicks_first_attempt() {
        // Backoff elapsed since the only prior action (detection) →
        // first pkill cycle.
        let inp = GapRecoveryInputs {
            now: 10_000,
            marker_present: true,
            detected_at: 9_400, // 600s ago (== backoff)
            last_force_recovery_at: 0,
            force_attempts: 0,
            catalog_at_detection: "000000010000000000000050".into(),
            catalog_max: "000000010000000000000050".into(),
            ..base_inputs()
        };
        assert_eq!(
            decide_gap_recovery(&inp),
            GapRecoveryAction::KickAsyncDaemon { attempt: 1 }
        );
    }

    #[test]
    fn marker_present_within_backoff_after_kick_waits() {
        // We've kicked once; backoff resets from `last_force_recovery_at`.
        let inp = GapRecoveryInputs {
            now: 10_000,
            marker_present: true,
            detected_at: 8_000,
            last_force_recovery_at: 9_700, // 300s ago (< 600s)
            force_attempts: 1,
            catalog_at_detection: "000000010000000000000050".into(),
            catalog_max: "000000010000000000000050".into(),
            ..base_inputs()
        };
        assert_eq!(decide_gap_recovery(&inp), GapRecoveryAction::Wait);
    }

    #[test]
    fn marker_present_at_backoff_after_kick_kicks_again_with_incremented_attempt() {
        // Extended outage: second pkill cycle, attempts bumped to 2.
        let inp = GapRecoveryInputs {
            now: 10_000,
            marker_present: true,
            detected_at: 8_000,
            last_force_recovery_at: 9_400, // 600s ago (== backoff)
            force_attempts: 1,
            catalog_at_detection: "000000010000000000000050".into(),
            catalog_max: "000000010000000000000050".into(),
            ..base_inputs()
        };
        assert_eq!(
            decide_gap_recovery(&inp),
            GapRecoveryAction::KickAsyncDaemon { attempt: 2 }
        );
    }

    #[test]
    fn marker_present_catalog_lower_than_baseline_does_not_diff() {
        // Edge case: pgbackrest info returned a catalog max that
        // parses to a lower segment number than catalog_at_detection
        // (could happen on a stanza rebuild or timeline rollback).
        // Don't treat that as "advance" — the diff would anchor against
        // a regressed baseline. Wait until the catalog catches up.
        let inp = GapRecoveryInputs {
            now: 10_000,
            marker_present: true,
            detected_at: 9_900,
            catalog_at_detection: "000000010000000000000051".into(),
            catalog_max: "000000010000000000000050".into(),
            ..base_inputs()
        };
        assert_eq!(decide_gap_recovery(&inp), GapRecoveryAction::Wait);
    }

    #[test]
    fn marker_present_advance_takes_priority_over_backoff_kick() {
        // If the catalog HAS advanced, even with backoff elapsed and
        // attempts pending, we diff (not kick). Advance is the
        // higher-signal event.
        let inp = GapRecoveryInputs {
            now: 10_000,
            marker_present: true,
            detected_at: 8_000, // 2000s ago, way past backoff
            last_force_recovery_at: 0,
            force_attempts: 0,
            catalog_at_detection: "000000010000000000000050".into(),
            catalog_max: "000000010000000000000051".into(),
            ..base_inputs()
        };
        assert_eq!(
            decide_gap_recovery(&inp),
            GapRecoveryAction::TakeRecoveryDiff
        );
    }

    #[test]
    fn probe_async_duplicate_error_filters_to_exit45_wal_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("pgbackrest-spool/archive/main/out");
        fs::create_dir_all(&out_dir).unwrap();

        // Backup-history files archive through the same path but must not
        // short-circuit the scan with a non-segment basename.
        fs::write(
            out_dir.join("000000010000000000000002.00000028.backup.error"),
            "45\nArchiveDuplicateError\n",
        )
        .unwrap();
        // Wrong exit code: not WAL_REGRESSION.
        fs::write(out_dir.join("000000010000000000000003.error"), "82\n").unwrap();
        // Valid exit-45 WAL segment at or before catalog max.
        fs::write(
            out_dir.join("000000010000000000000004.error"),
            "45\nArchiveDuplicateError\n",
        )
        .unwrap();

        assert_eq!(
            probe_async_duplicate_error(
                tmp.path().to_str().unwrap(),
                "000000010000000000000004",
                256,
            ),
            Some("000000010000000000000004".to_string())
        );
        assert!(wal_has_async_archive_duplicate_error(
            tmp.path().to_str().unwrap(),
            "000000010000000000000004",
            256,
        ));
        assert!(!wal_has_async_archive_duplicate_error(
            tmp.path().to_str().unwrap(),
            "000000010000000000000003",
            256,
        ));
    }
}
