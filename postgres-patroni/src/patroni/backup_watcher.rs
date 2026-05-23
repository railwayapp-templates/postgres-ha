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
    archived_count: i64,
    failed_count: i64,
    /// 24-char hex WAL filename of the most-recent segment Postgres handed
    /// off to the archive process. Empty when `pg_stat_archiver` reports
    /// NULL (cluster just started, never archived). Used by the LSN-lag
    /// probe to compare against `pgbackrest info`'s repo high-water.
    last_archived_wal: String,
}

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

async fn watcher_iteration(
    data_dir: &str,
    config: &WatcherConfig,
    client: &reqwest::Client,
) {
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

    // Gap-recovery state machine — detects WAL/catalog divergence and
    // drives the kick-and-diff sequence. Runs every iteration;
    // pgbackrest info is cheap enough that throttling isn't worth the
    // false-negative window the earlier throttled version introduced.
    gap_recovery_step(data_dir, config, &stats).await;

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
    // COALESCE last_archived_wal to '-' so split_whitespace() doesn't
    // collapse an empty trailing column into the preceding one and corrupt
    // the bind. The sentinel is stripped below.
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
             COALESCE(EXTRACT(EPOCH FROM last_failed_time)::bigint, 0), \
             COALESCE(last_archived_wal, '-') \
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
    if parts.len() < 5 {
        anyhow::bail!("pg_stat_archiver malformed: {line}");
    }
    let wal = parts[4];
    let last_archived_wal = if wal == "-" {
        String::new()
    } else {
        wal.to_string()
    };
    Ok(ArchiverStats {
        archived_count: parts[0].parse().unwrap_or(0),
        failed_count: parts[1].parse().unwrap_or(0),
        last_archived_wal,
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

/// 24-char hex WAL filename → absolute segment count (256 segments per
/// log file at the default 16 MiB wal_segment_size). Returns None on
/// malformed input so callers short-circuit. Strict shape + hex check
/// avoids letting a stray non-hex character feed `u64::from_str_radix`
/// and surface a parse error to the watcher loop.
fn segment_to_number(wal: &str) -> Option<u64> {
    if wal.len() != 24 || !wal.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let log = u64::from_str_radix(&wal[8..16], 16).ok()?;
    let seg = u64::from_str_radix(&wal[16..24], 16).ok()?;
    Some(log * 256 + seg)
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
    let stanzas = v.as_array().ok_or_else(|| {
        anyhow::anyhow!("pgbackrest info JSON top-level not an array")
    })?;
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
    let handed_off = segment_to_number(&stats.last_archived_wal)
        .ok_or_else(|| anyhow::anyhow!("malformed last_archived_wal: {}", stats.last_archived_wal))?;

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
    let lag = match catalog_max.as_deref().and_then(segment_to_number) {
        Some(n) => handed_off.saturating_sub(n),
        None => 0,
    };
    Ok(CatalogProbe { catalog_max, lag })
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
    let curr = segment_to_number(&inp.catalog_max);
    let det = segment_to_number(&inp.catalog_at_detection);
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
    stats: &ArchiverStats,
) {
    let now = now_epoch();
    let state_path = format!("{data_dir}/{STATE_FILENAME}");
    let gap_marker = format!("{data_dir}/{GAP_MARKER_FILENAME}");

    let probe = match probe_catalog_max(stats).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "pgbackrest-watcher: pgbackrest info probe failed; leaving gap-recovery state unchanged");
            return;
        }
    };

    let inp = GapRecoveryInputs {
        now,
        lag: probe.lag,
        catalog_max: probe.catalog_max.unwrap_or_default(),
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
    };

    match decide_gap_recovery(&inp) {
        GapRecoveryAction::NoOp => {}

        GapRecoveryAction::Detect { catalog_at_detection } => {
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

    // NEEDS_INITIAL_BACKUP — no full on record, take it now. PR #59
    // dropped the "archived_count > 0" gate: pgbackrest backup brackets
    // pg_backup_start/stop and waits for the closing WAL to archive
    // before declaring success, so a broken archive_command fails the
    // backup loudly instead of producing an unrestorable base.
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
                    clear_gap_recovery_state(data_dir, "cleared by full backup", stats_pre.failed_count).await;
                }
                "diff" => {
                    let _ = write_state_field(&state_path, "last_diff_at", &now.to_string());
                }
                _ => {}
            }
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

#[cfg(test)]
mod tests {
    use super::{
        decide_gap_recovery, parse_catalog_max, segment_to_number, GapRecoveryAction,
        GapRecoveryInputs,
    };

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
        }
    }

    #[test]
    fn segment_to_number_real_values() {
        // Real cache values (admin monitor warning detail). Lag must match
        // the segment math the monitor uses (713 segments for both).
        assert_eq!(segment_to_number("000000010000001D000000ED"), Some(7661));
        assert_eq!(segment_to_number("000000010000001B00000024"), Some(6948));
        assert_eq!(
            segment_to_number("000000010000001D000000ED").unwrap()
                - segment_to_number("000000010000001B00000024").unwrap(),
            713
        );
    }

    #[test]
    fn segment_to_number_rejects_malformed() {
        assert_eq!(segment_to_number(""), None);
        assert_eq!(segment_to_number("not24"), None);
        assert_eq!(segment_to_number("abcdefghijklmnopqrstuvwx"), None); // 24 non-hex
        assert_eq!(segment_to_number("000000010000001B0000002"), None); // 23 chars
        assert_eq!(segment_to_number("000000010000001B0000002__"), None); // 25 chars
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
}
