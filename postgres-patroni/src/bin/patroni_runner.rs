//! Patroni runner - Wrapper to run Patroni with proper setup
//!
//! Generates Patroni configuration and starts Patroni.
//! Runs as PID 1 in container with built-in health monitoring.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use common::init_logging;
use nix::sys::signal::{SigHandler, Signal};
use nix::sys::stat::{umask, Mode};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::{ForkResult, Pid};
use postgres_patroni::bootstrap::refresh_collation_versions;
use postgres_patroni::health_server::{self, HealthServerConfig};
use postgres_patroni::major_upgrade;
use postgres_patroni::patroni::{
    apply_credential_pin, credentials_from_env_requested, generate_patroni_config,
    reconcile_pgbackrest_archive_config, run_monitoring_loop, spawn_backup_watcher,
    spawn_self_heal_watcher, spawn_slot_recovery_watcher, update_pg_hba_for_replication, Config,
};
use postgres_patroni::pgbackrest::{derive_pgbackrest_repo_path, read_wal_level};
use postgres_patroni::{volume_root, Telemetry, TelemetryEvent};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;
use tokio::process::Command;
use tracing::{error, info, warn};

/// Translate the tool-agnostic `WAL_ARCHIVE_*` env contract into
/// pgBackRest's native `PGBACKREST_REPO1_S3_*` so pgBackRest reads them
/// natively and the rest of this binary (and the `archive_command`
/// wrapper Postgres invokes) can stay pgBackRest-shaped.
///
/// Backboard / frontend / template speak the WAL_ contract; the image
/// translates locally so swapping pgBackRest for another archiver is a
/// wrapper change rather than a cross-repo rewrite.
///
/// `WAL_RECOVER_FROM_*` is intentionally NOT translated to env vars.
/// pgBackRest's option resolution is command-line > env vars > config
/// file > defaults, so a global `PGBACKREST_REPO*_*` export silently
/// overrides any --config we pass during recovery. Instead the
/// recover-from credentials live ONLY in
/// `/etc/pgbackrest/pgbackrest-recovery-source.conf`, which is referenced
/// via --config exclusively for restore + archive-get during recovery.
/// This keeps archive-push, stanza-create, and backup against the
/// service's own bucket — they read the default pgbackrest.conf which
/// has only repo1 (the service's archive bucket). Mirrors postgres-ssl
/// PR #49.
fn translate_wal_env_to_pgbackrest() {
    let archive = env::var("WAL_ARCHIVE_BUCKET")
        .ok()
        .filter(|s| !s.is_empty());

    if archive.is_some() {
        export_repo("PGBACKREST_REPO1", "WAL_ARCHIVE");
    }
}

/// Reasons `validate_wal_archive_bucket` may reject `WAL_ARCHIVE_BUCKET`.
/// Each value is the sentinel-file payload that the admin monitor reads
/// to distinguish "never enabled" from one of these misconfigurations.
const VALIDATE_BUCKET_REASON_TEMPLATE_REF: &str = "unresolved-template-ref";
const VALIDATE_BUCKET_REASON_WHITESPACE: &str = "whitespace";
const VALIDATE_BUCKET_REASON_UUID_SHAPE: &str = "uuid-shape";

/// Screen `WAL_ARCHIVE_BUCKET` for known-bogus shapes before
/// `translate_wal_env_to_pgbackrest` exports it as `PGBACKREST_REPO1_S3_BUCKET`.
/// If invalid, unset the WAL_ARCHIVE_* env vars so every downstream gate
/// treats archiving as off, then drop a `.pgbackrest_invalid_bucket`
/// sentinel under the **volume root** (NOT PGDATA) so the admin
/// dashboard can surface the "PITR enabled but wired to junk" state
/// distinctly from "PITR never enabled" or "PITR misconfigured creds."
/// Mirrors postgres-ssl `wrapper.sh::validate_wal_archive_bucket`.
///
/// Path choice: PGDATA itself (`<volume_root>/pgdata`) gets wiped/
/// reinitialized by Patroni's bootstrap on the first boot of a fresh
/// volume — a sentinel inside it would silently disappear before the
/// dashboard could read it. The volume root is on the same persistent
/// volume but outside Patroni's reach, so the sentinel survives
/// initdb + bootstrap.
///
/// Caught shapes:
///   - contains `${{` or `}}` → unresolved Railway template ref leaked
///     by the resolver (most common misconfiguration cause)
///   - whitespace or control chars → typo or shell-escape mishap
///   - UUID 8-4-4-4-12 hex → almost certainly a raw bucket-id from a
///     tombstoned bucket; opt out via `WAL_ARCHIVE_BUCKET_ALLOW_UUID=1`
///     if you legitimately use a UUID-named bucket.
///
/// Sentinel cleanup: `clear_pgbackrest_state_if_disabled` removes the
/// sentinel on disable (WAL_ARCHIVE_BUCKET unset on next boot).
fn validate_wal_archive_bucket(volume_root: &str) {
    let marker = format!("{volume_root}/.pgbackrest_invalid_bucket");
    let val = match env::var("WAL_ARCHIVE_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(v) => v,
        None => {
            // Bucket unset → either never configured or already
            // intentionally disabled. Clear any stale sentinel so the
            // dashboard doesn't flag a now-disabled service.
            let _ = fs::remove_file(&marker);
            return;
        }
    };
    let invalid = if val.contains("${{") || val.contains("}}") {
        Some(VALIDATE_BUCKET_REASON_TEMPLATE_REF)
    } else if val.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some(VALIDATE_BUCKET_REASON_WHITESPACE)
    } else if env::var("WAL_ARCHIVE_BUCKET_ALLOW_UUID")
        .ok()
        .filter(|s| s == "1")
        .is_none()
        && is_uuid_shape(&val)
    {
        Some(VALIDATE_BUCKET_REASON_UUID_SHAPE)
    } else {
        None
    };

    let Some(reason) = invalid else {
        // Valid bucket name; remove any stale sentinel from a previous
        // boot's misconfiguration.
        let _ = fs::remove_file(&marker);
        return;
    };

    if reason == VALIDATE_BUCKET_REASON_UUID_SHAPE {
        warn!(
            value = %val,
            reason = %reason,
            "pgbackrest: WAL_ARCHIVE_BUCKET looks invalid (uuid-shape); refusing to enable archiving. \
             If this UUID is your legitimate bucket name, set WAL_ARCHIVE_BUCKET_ALLOW_UUID=1 to override."
        );
    } else {
        warn!(
            value = %val,
            reason = %reason,
            "pgbackrest: WAL_ARCHIVE_BUCKET looks invalid; refusing to enable archiving"
        );
    }
    // postgres_wrapper has already chowned volume_root to postgres:postgres
    // before exec'ing patroni-runner, so writes here succeed as the
    // postgres user. Log errors loudly — silent failures here mean the
    // dashboard wouldn't show the misconfiguration and operators would
    // think PITR was never enabled (rather than wired to junk).
    match fs::write(&marker, format!("{reason}\n")) {
        Ok(()) => info!(marker = %marker, "pgbackrest: invalid-bucket sentinel written"),
        Err(e) => {
            warn!(marker = %marker, error = %e, "pgbackrest: failed to write invalid-bucket sentinel")
        }
    }
    if let Err(e) = fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o640)) {
        warn!(marker = %marker, error = %e, "pgbackrest: failed to set sentinel permissions");
    }
    for key in [
        "WAL_ARCHIVE_BUCKET",
        "WAL_ARCHIVE_KEY",
        "WAL_ARCHIVE_SECRET",
        "WAL_ARCHIVE_REGION",
        "WAL_ARCHIVE_ENDPOINT",
    ] {
        env::remove_var(key);
    }
}

/// Returns `true` when `s` matches the literal 8-4-4-4-12 lowercase-hex
/// UUID shape (no version/variant nibble enforcement — Railway's bucket
/// ids are random enough that any uuid-shaped string is likely a leak).
fn is_uuid_shape(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    let dash_positions = [8usize, 13, 18, 23];
    for (i, b) in bytes.iter().enumerate() {
        if dash_positions.contains(&i) {
            if *b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() || b.is_ascii_uppercase() {
            return false;
        }
    }
    true
}

/// Copy a `WAL_<role>_*` quintuple onto a `PGBACKREST_<repo>_S3_*` quintuple
/// (plus the non-S3-prefixed `_PATH` knob). Path defaults to `/pgbackrest`
/// when unset, matching the wrapper.sh behavior in postgres-ssl.
fn export_repo(repo_prefix: &str, source_prefix: &str) {
    for (dst_suffix, src_suffix, default) in [
        ("S3_BUCKET", "BUCKET", None),
        ("S3_KEY", "KEY", None),
        ("S3_KEY_SECRET", "SECRET", None),
        ("S3_REGION", "REGION", None),
        ("S3_ENDPOINT", "ENDPOINT", None),
        ("PATH", "PATH", Some("/pgbackrest")),
    ] {
        let src = format!("{source_prefix}_{src_suffix}");
        let dst = format!("{repo_prefix}_{dst_suffix}");
        let value = env::var(&src)
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| default.map(String::from));
        if let Some(v) = value {
            env::set_var(&dst, v);
        }
    }
}

/// Detect the container's effective CPU allocation. Reads cgroup v2 cpu.max
/// first, then falls back to cgroup v1 cpu.cfs_quota_us, then to nproc.
/// Returns the integer ceiling of fractional quotas (0.5 vCPU → 1) so
/// process-max sizing is sane on the smallest tier.
fn detect_cpus() -> u32 {
    if let Ok(s) = fs::read_to_string("/sys/fs/cgroup/cpu.max") {
        let mut it = s.split_whitespace();
        if let (Some(q), Some(p)) = (it.next(), it.next()) {
            if q != "max" {
                if let (Ok(quota), Ok(period)) = (q.parse::<i64>(), p.parse::<i64>()) {
                    if quota > 0 && period > 0 {
                        return ((quota + period - 1) / period) as u32;
                    }
                }
            }
        }
    }
    let q = fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_quota_us")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());
    let p = fs::read_to_string("/sys/fs/cgroup/cpu/cpu.cfs_period_us")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok());
    if let (Some(quota), Some(period)) = (q, p) {
        if quota > 0 && period > 0 {
            return ((quota + period - 1) / period) as u32;
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

fn clamp(v: i64, lo: i64, hi: i64) -> u32 {
    v.clamp(lo, hi) as u32
}

/// Pure parse+validate for a `process-max` override: accepts only a positive
/// integer, falls back to `default` on anything missing/zero/malformed.
/// Split out from `env_or_clamp` (mirrors `resolve_basebackup_max_rate` in
/// config.rs) so the parsing logic is unit-testable without mutating process
/// env — `PGBACKREST_*_PROCESS_MAX` vars would otherwise race across tests
/// run in parallel within the same binary.
fn parse_process_max(raw: Option<&str>, default: u32) -> u32 {
    raw.and_then(|s| s.parse::<u32>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(default)
}

fn env_or_clamp(var: &str, default: u32) -> u32 {
    parse_process_max(env::var(var).ok().as_deref(), default)
}

/// pg_wal drop ceiling (MiB) and pgBackRest archive-push spool ceiling (MiB).
/// Both scale DOWN from the absolute default (5120) on small volumes — never
/// up. On volumes ≥10 GiB the absolute holds.
///
/// wal-drop == queue-max, deliberately identical (was ~10% of volume / 500
/// MiB cap, a ~10x smaller budget than queue-max — see 2026-07-01 Tigris
/// "sjc" incident: transient S3 500s/connection-resets are exactly the
/// failure pgBackRest's spool is designed to absorb generously, but the
/// wrapper's own smaller pg_wal check tripped first, silently dropping WAL
/// far short of the 5 GiB spool budget that should have covered the whole
/// outage). Only the two explicit no-recovery-possible errors (NoSuchBucket,
/// InvalidAccessKeyId, checked in pgbackrest-archive-push-wrapper.sh) bypass
/// this and drop immediately — everything else, hard failure or transient,
/// gets the full budget before we give up on it.
///
/// The wrapper checks pg_wal + spool against this value as ONE combined sum,
/// not pg_wal alone — identical caps on two independently-checked
/// directories would let a single outage hold up to ~2x this budget on disk.
///
/// Floor: 128 MiB (~8 WAL segments). Below this archiving is effectively off
/// and the dashboard surfaces it via pg_stat_archiver.
fn compute_volume_thresholds(volume_path: &str) -> (u32, u32) {
    use nix::sys::statvfs::statvfs;

    let total_mib = statvfs(Path::new(volume_path))
        .ok()
        .and_then(|s| {
            let total = (s.blocks() as u64).checked_mul(s.fragment_size() as u64)?;
            Some((total / (1024 * 1024)) as u32)
        })
        .unwrap_or(0);

    if total_mib == 0 {
        info!("pgbackrest: volume size unknown; using absolute threshold wal-drop=queue-max=5 GiB");
        return (5 * 1024, 5 * 1024);
    }

    let queue_max = (total_mib / 2).clamp(128, 5 * 1024);
    let wal_drop = queue_max;

    info!(
        volume_mib = total_mib,
        wal_drop_mib = wal_drop,
        queue_max_mib = queue_max,
        "pgbackrest: sized WAL thresholds from volume size"
    );

    (wal_drop, queue_max)
}

/// Request body for etcd v3 range API
#[derive(Serialize)]
struct EtcdRangeRequest {
    key: String,
}

/// Response from etcd v3 range API
#[derive(Deserialize)]
struct EtcdRangeResponse {
    #[serde(default)]
    kvs: Option<Vec<serde_json::Value>>,
}

/// Wait for the Patroni cluster to exist in etcd before starting.
/// This prevents replicas from racing with the primary during initial setup.
/// Only the primary (with existing data) should be allowed to initialize the cluster.
async fn wait_for_cluster_in_etcd(config: &Config) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("Failed to create HTTP client")?;

    // The key Patroni uses for leader lock: /service/{scope}/leader
    let leader_key = format!("/service/{}/leader", config.scope);
    let key_base64 = BASE64.encode(leader_key.as_bytes());

    // Parse etcd hosts - format is "host1:port1,host2:port2,..."
    let etcd_hosts: Vec<&str> = config.etcd_hosts.split(',').collect();

    let max_wait = Duration::from_secs(300); // 5 minute max wait
    let poll_interval = Duration::from_secs(2);
    let start = std::time::Instant::now();

    info!(
        scope = %config.scope,
        "Waiting for cluster to be initialized by primary before starting..."
    );

    loop {
        if start.elapsed() > max_wait {
            anyhow::bail!(
                "Timeout waiting for cluster '{}' to be initialized in etcd after {:?}",
                config.scope,
                max_wait
            );
        }

        // Try each etcd host until one succeeds
        for host in &etcd_hosts {
            let url = format!("http://{}/v3/kv/range", host.trim());
            let request = EtcdRangeRequest {
                key: key_base64.clone(),
            };

            match client.post(&url).json(&request).send().await {
                Ok(response) if response.status().is_success() => {
                    if let Ok(range_response) = response.json::<EtcdRangeResponse>().await {
                        // Check if we got any keys back (cluster exists and has a leader)
                        let has_leader = range_response
                            .kvs
                            .as_ref()
                            .map(|kvs| !kvs.is_empty())
                            .unwrap_or(false);

                        if has_leader {
                            info!(
                                scope = %config.scope,
                                elapsed = ?start.elapsed(),
                                "Cluster leader found, proceeding to start Patroni"
                            );
                            return Ok(());
                        }
                    }
                }
                Ok(response) => {
                    warn!(
                        host = %host,
                        status = %response.status(),
                        "etcd returned non-success status"
                    );
                }
                Err(e) => {
                    warn!(host = %host, error = %e, "Failed to connect to etcd");
                }
            }
        }

        info!(
            elapsed = ?start.elapsed(),
            "Cluster not yet initialized, waiting..."
        );
        tokio::time::sleep(poll_interval).await;
    }
}

/// True if `data_dir` exists and contains at least one entry.
fn data_dir_nonempty(data_dir: &str) -> bool {
    fs::read_dir(data_dir)
        .map(|mut it| it.next().is_some())
        .unwrap_or(false)
}

/// True when PGDATA is a dedicated subdirectory of the volume (the standard
/// `<volume_root>/pgdata` layout) rather than the volume root itself. The
/// interrupted-clone wipe is only safe in the former: at the volume root it
/// would also delete the sibling state we persist there (the bootstrap marker,
/// the invalid-bucket sentinel, TLS certs). Trailing-slash insensitive.
fn pgdata_is_dedicated_subdir(data_dir: &str, volume_root: &str) -> bool {
    data_dir.trim_end_matches('/') != volume_root.trim_end_matches('/')
}

/// The safety core shared by every destructive pgdata wipe in this binary
/// (the interrupted-clone wipe and the major-upgrade reseed wipe): pgdata must
/// be a dedicated subdir of the volume (not the volume root itself, whose
/// siblings — bootstrap marker, sentinels, TLS certs — a wipe would destroy),
/// and a DISTINCT member must hold the DCS leader lock. A `None` leader (no
/// lock / etcd unreachable) or a leader that is *us* (a stale lock) blocks the
/// wipe so we never destroy the only copy of the data.
fn wipe_has_safe_clone_source(
    pgdata_is_dedicated_subdir: bool,
    leader: Option<&str>,
    my_name: &str,
) -> bool {
    pgdata_is_dedicated_subdir && matches!(leader, Some(l) if l != my_name)
}

/// Pure safety predicate for the interrupted-clone wipe (unit-tested). Only
/// wipe when: pg_control is absent, the dir is non-empty, and
/// [`wipe_has_safe_clone_source`] holds (dedicated pgdata subdir + a distinct
/// member holding the DCS leader lock — a node missing pg_control cannot
/// itself be a healthy leader).
/// How many times the incomplete-clone wipe may fire on one volume before it
/// stops. The wipe exists to unwedge a clone that was interrupted once; a clone
/// that is interrupted every single time is not being unwedged by it, and each
/// extra pass destroys the partial data again for nothing. Three is enough to
/// ride out transient interruptions (a stacker blip, a leader failover mid-clone)
/// while still bounding the loop.
const MAX_CLONE_WIPE_ATTEMPTS: u32 = 3;

/// Ledger of incomplete-clone wipes, kept at the VOLUME ROOT rather than inside
/// pgdata — the wipe empties pgdata, so a counter stored in there would reset
/// itself on every pass and could never bound anything.
fn clone_wipe_ledger_path(volume_root: &str) -> String {
    format!("{}/.railway_clone_wipe_attempts", volume_root)
}

/// Wipes recorded so far. An unreadable or malformed ledger reads as 0: the gate
/// may only ever *block* a destructive action, so when in doubt it must not be
/// the thing that stops a legitimate first recovery.
fn read_clone_wipe_attempts(volume_root: &str) -> u32 {
    fs::read_to_string(clone_wipe_ledger_path(volume_root))
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// Best-effort: a ledger we cannot write is not a reason to refuse recovery, so
/// failures are logged and swallowed rather than propagated.
fn record_clone_wipe_attempt(volume_root: &str, attempts: u32) {
    let path = clone_wipe_ledger_path(volume_root);
    if let Err(e) = fs::write(&path, attempts.to_string()) {
        warn!(path = %path, error = %e, "Failed to record clone-wipe attempt; the wipe cap may not hold");
    }
}

/// Called once a complete clone is observed (pg_control present). The volume is
/// healthy, so the previous wipes did their job and must not count against a
/// future, unrelated interruption.
fn clear_clone_wipe_attempts(volume_root: &str) {
    let path = clone_wipe_ledger_path(volume_root);
    if Path::new(&path).exists() {
        if let Err(e) = fs::remove_file(&path) {
            warn!(path = %path, error = %e, "Failed to clear the clone-wipe ledger");
        } else {
            info!("Complete clone present — cleared the clone-wipe ledger");
        }
    }
}

/// Free bytes on the filesystem holding `path`, for diagnosis only — never a
/// gate. Estimating whether a clone "fits" would need the leader's on-disk size,
/// which this process cannot see before Patroni starts; the attempt ledger above
/// bounds the loop empirically instead, and this number tells the reader whether
/// capacity was why.
fn available_bytes(path: &str) -> Option<u64> {
    let stat = nix::sys::statvfs::statvfs(Path::new(path)).ok()?;
    Some(stat.blocks_available() as u64 * stat.fragment_size() as u64)
}

fn should_wipe_incomplete_clone(
    has_pg_control: bool,
    data_dir_nonempty: bool,
    pgdata_is_dedicated_subdir: bool,
    leader: Option<&str>,
    my_name: &str,
) -> bool {
    !has_pg_control
        && data_dir_nonempty
        && wipe_has_safe_clone_source(pgdata_is_dedicated_subdir, leader, my_name)
}

/// Read the current leader's member name from etcd (`/service/{scope}/leader`).
/// Returns None when no leader holds the lock or etcd is unreachable — both
/// block the destructive wipe. Best-effort across all etcd hosts.
async fn probe_cluster_leader(config: &Config) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?;
    let leader_key = format!("/service/{}/leader", config.scope);
    let key_base64 = BASE64.encode(leader_key.as_bytes());
    for host in config.etcd_hosts.split(',') {
        let url = format!("http://{}/v3/kv/range", host.trim());
        let request = EtcdRangeRequest {
            key: key_base64.clone(),
        };
        let Ok(resp) = client.post(&url).json(&request).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(range) = resp.json::<EtcdRangeResponse>().await else {
            continue;
        };
        // etcd v3 returns the value base64-encoded; Patroni stores the holding
        // member's name as the leader-key value.
        let value_b64 = range
            .kvs
            .as_ref()
            .and_then(|kvs| kvs.first())
            .and_then(|kv| kv.get("value"))
            .and_then(|v| v.as_str());
        if let Some(b64) = value_b64 {
            if let Ok(bytes) = BASE64.decode(b64) {
                if let Ok(name) = String::from_utf8(bytes) {
                    let name = name.trim().to_string();
                    if !name.is_empty() {
                        return Some(name);
                    }
                }
            }
        }
    }
    None
}

/// Remove every entry inside `data_dir` (the contents, not the mount point),
/// so Patroni sees an empty pgdata and performs a fresh pg_basebackup.
///
/// Symlinks are unlinked, never followed: `pg_tblspc/<oid>` and a relocated
/// `pg_wal` are symlinks pointing outside pgdata, and a recursive
/// `remove_dir_all` through one would delete data on another filesystem. We
/// only recurse into a *real* directory; for a symlink (even one targeting a
/// directory) we drop just the link. `DirEntry::file_type` does not traverse
/// symlinks, so the explicit `is_symlink` check below is belt-and-suspenders
/// to keep that invariant obvious and robust.
fn wipe_pgdata_contents(data_dir: &str) -> Result<()> {
    for entry in fs::read_dir(data_dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() && !file_type.is_symlink() {
            fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
        } else {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

/// A failed marker removal is more than a stale file: the self-heal watcher
/// treats any non-completed marker as an upgrade in flight and stands down
/// for as long as it exists, so a removal that fails in handle_reseed_marker
/// quietly disables self-healing until some future boot manages the unlink.
/// Boot still proceeds (the database is healthy; refusing would trade
/// degraded self-healing for downtime), but the state must announce itself:
/// an error log plus a telemetry event here, and the watcher's standdown
/// event carries the marker's phase and age so the two correlate in the
/// fleet view.
fn report_marker_removal_failure(telemetry: &Telemetry, e: &std::io::Error, when: &str) {
    error!(
        error = %e,
        "failed to remove the reseed marker {when}; the self-heal watcher stands down while it exists — if no upgrade is running, remove it manually to restore self-healing"
    );
    telemetry.send(TelemetryEvent::ComponentError {
        component: "patroni-runner".to_string(),
        error: format!("failed to remove the reseed marker {when}: {e}"),
        context: "the leftover marker keeps the self-heal watcher standing down until it is removed"
            .to_string(),
    });
}

/// Consume a `reseed` upgrade marker (see `major_upgrade`'s module doc).
///
/// The HA upgrade workflow writes this onto each replica's volume before it
/// pauses failover: after the leader is upgraded, the replica is repinned to
/// the new major and redeployed, and THIS boot is what rebuilds it. Streaming
/// replication cannot cross majors, so a cross-major data directory can never
/// rejoin the upgraded leader — it has to be wiped so Patroni takes a fresh
/// basebackup.
///
///   - on-disk PG_VERSION differs from the image's major → wipe pgdata
///     contents, but ONLY under the same safety predicate as the
///     incomplete-clone wipe (dedicated pgdata subdir + a DISTINCT member
///     holding the DCS leader lock — never wipe without a live clone source).
///     Without that predicate we bail with the marker left in place, so the
///     next boot retries once the leader is back.
///   - PG_VERSION matches (or either major is unknown — the version guard
///     abstains rather than guessing) → just delete the marker and boot
///     normally. This is the rollback shape: the workflow failed before the
///     replica was repinned and the old image boots its own data.
///
/// The marker is deleted AT WIPE TIME, not after the clone: an interrupted
/// clone then presents as ordinary incomplete-clone debris on the next boot
/// (non-empty pgdata, no pg_control, no marker) and the existing machinery
/// re-clones it, instead of this path re-running against a half-cloned dir.
/// The wipe itself is idempotent, so deleting the marker only after a
/// successful wipe means an interrupted WIPE simply retries next boot.
async fn handle_reseed_marker(
    config: &Config,
    volume_root: &str,
    image_major: Option<&str>,
    telemetry: &Telemetry,
) -> Result<()> {
    if !major_upgrade::reseed_requested(volume_root) {
        return Ok(());
    }

    let on_disk = major_upgrade::data_dir_major(&config.data_dir);
    let mismatch = matches!(
        (on_disk.as_deref(), image_major),
        (Some(disk), Some(image)) if disk != image
    );

    if !mismatch {
        info!(
            on_disk_major = ?on_disk,
            image_major = ?image_major,
            "reseed marker present but the data directory already matches the image (or a major is unknown) — consuming the marker and booting normally"
        );
        if let Err(e) = major_upgrade::remove_marker(volume_root) {
            report_marker_removal_failure(telemetry, &e, "on the matching-major boot");
        }
        return Ok(());
    }

    let dedicated = pgdata_is_dedicated_subdir(&config.data_dir, volume_root);
    let leader = if dedicated {
        probe_cluster_leader(config).await
    } else {
        None
    };
    if !wipe_has_safe_clone_source(dedicated, leader.as_deref(), &config.name) {
        // Marker deliberately left in place: the reseed is still owed, and the
        // next boot retries it — the leader may be reachable by then.
        anyhow::bail!(
            "A replica reseed was requested (marker phase: reseed) and the data directory holds \
             major {} against a {} image, but it is not safe to wipe without a live clone source \
             (need pgdata as a dedicated subdir and a DISTINCT member holding the DCS leader \
             lock; pgdata_is_dedicated_subdir={}, leader={:?}). Refusing to boot; the marker is \
             left in place so the next boot retries.",
            on_disk.as_deref().unwrap_or("?"),
            image_major.unwrap_or("?"),
            dedicated,
            leader,
        );
    }

    warn!(
        data_dir = %config.data_dir,
        on_disk_major = ?on_disk,
        image_major = ?image_major,
        leader = %leader.as_deref().unwrap_or("?"),
        "Reseed marker on a cross-major data directory — wiping pgdata so Patroni re-clones from the upgraded leader"
    );
    wipe_pgdata_contents(&config.data_dir)
        .context("Failed to wipe the data directory for the requested reseed")?;
    if let Err(e) = major_upgrade::remove_marker(volume_root) {
        // Data-safe (the next boot sees an empty pgdata — no PG_VERSION, no
        // mismatch — and takes the consume-and-boot branch above), but NOT
        // harmless: the leftover marker keeps the self-heal watcher standing
        // down until a boot manages the unlink, so it must announce itself.
        report_marker_removal_failure(telemetry, &e, "after the wipe");
    }
    telemetry.send(TelemetryEvent::MajorUpgradeReseedWiped {
        node: config.name.clone(),
        leader: leader.as_deref().unwrap_or("unknown").to_string(),
        from_major: on_disk.unwrap_or_else(|| "?".to_string()),
        to_major: image_major.unwrap_or("?").to_string(),
    });
    Ok(())
}

async fn start_patroni() -> Result<tokio::process::Child> {
    let child = Command::new("patroni")
        .arg("/etc/patroni/patroni.yml")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to start patroni")?;

    Ok(child)
}

/// Render `/etc/pgbackrest/pgbackrest.conf` with operator-policy defaults +
/// stanza definition. User-supplied options (S3 bucket, region, key, secret,
/// endpoint, repo path) are read by pgBackRest natively from `PGBACKREST_*`
/// env vars (which `translate_wal_env_to_pgbackrest` populated from the
/// `WAL_ARCHIVE_*` / `WAL_RECOVER_FROM_*` contract), so they don't need to
/// be in the conf file.
///
/// The `archive-async=y` + `archive-push-queue-max` combination is one of
/// the two thresholds that keep Postgres alive under archiving failure; the
/// other is `pgbackrest-archive-push-wrapper.sh`'s `WAL_DROP_THRESHOLD_MB`
/// on `pg_wal/`. Either tripping drops WAL and keeps the DB up at the cost
/// of a truncated PITR window. Both ceilings come from
/// `compute_volume_thresholds` and are now symmetric (≤5 GiB on volumes
/// ≥10 GiB; scaled down proportionally below that).
///
/// Spool lives under `$PGDATA/pgbackrest-spool` so segments staged but not
/// yet pushed to S3 survive container restarts on the Railway volume.
/// Note: the spool directory is NOT created here — pre-creating it would
/// dirty pgdata before Patroni's first bootstrap and make fresh replicas
/// refuse to clone with "data dir is not empty, but system ID is invalid".
/// `spawn_bootstrap_stanza_create` mkdirs it after `pg_isready` confirms
/// Postgres is up (i.e., pgdata has been initialized by Patroni's clone or
/// initdb), mirroring postgres-ssl's `/docker-entrypoint-initdb.d` ordering.
///
/// Per-command `process-max` is sized off cgroup-detected vCPU. Each
/// command has a different bottleneck shape: archive-push is gated by
/// serial WAL arrival + S3 PUT overhead; archive-get is sized like push —
/// with `archive-async=y` it prefetches segments into the spool ahead of
/// replay, and during bulk catch-up per-segment S3 GET latency dominates,
/// so parallel prefetch directly shortens catch-up (this conf serves the
/// standby archive fallback; staged replay reads the recovery-source
/// conf, which mirrors these settings); restore is unbounded (DB is down) up to
/// pgBackRest's plateau around 32 workers. Backup is capped at 2: volume
/// read throughput does not scale with vCPU, so extra readers only deepen
/// the volume's request queue — starving live queries and any member
/// mid-rewind or mid-clone that is reading from this node.
///
/// No-op when neither archive nor recover-from is configured. Otherwise
/// idempotent — rewritten on every boot.
///
/// Wipe pgBackRest filesystem state for any role that is no longer
/// configured. Runs before the conf renderers so disabled-then-re-enabled
/// clusters don't carry forward stale watcher state, gap markers, or
/// recovery-staging markers from a previous configuration.
///
/// State scoping mirrors postgres-ssl wrapper.sh's clear_pgbackrest_state_
/// if_disabled:
///   - WAL_ARCHIVE_*  unset → drop watcher state, gap marker, repo-path
///                            marker (the per-cluster archive prefix)
///   - WAL_RECOVER_FROM_* unset → drop PITR staging/done/restored markers
///   - both unset → also drop the pgbackrest.conf files (they carry S3
///                  credentials from the previous role; clearing them
///                  removes a stale-cred footgun for any manual pgbackrest
///                  invocation post-disable)
///
/// The async spool dir is left alone: per design it's a coordination
/// cache, not durable data, and bootstrap_pgbackrest_stanza recreates it
/// when archiving comes back.
fn clear_pgbackrest_state_if_disabled(data_dir: &str) {
    let archive_enabled = env::var("WAL_ARCHIVE_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some();
    let recover_enabled = env::var("WAL_RECOVER_FROM_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some();

    if archive_enabled && recover_enabled {
        return;
    }

    let rm = |path: String| {
        if Path::new(&path).exists() {
            match fs::remove_file(&path) {
                Ok(_) => info!(path = %path, "pgbackrest: cleared stale state file"),
                Err(e) => warn!(error = %e, path = %path, "pgbackrest: failed to clear state file"),
            }
        }
    };

    if !archive_enabled {
        rm(format!("{data_dir}/.pgbackrest_backup_state"));
        rm(format!("{data_dir}/.pgbackrest_gap_pending"));
        rm(format!("{data_dir}/.pgbackrest_repo_path"));
        // Stanza-create timeout sentinel is scoped to the configured
        // archive bucket; clear it too so the monitor doesn't surface
        // "stanza bootstrap timed out" against a service that's now
        // intentionally in "no archive" state.
        rm(format!("{data_dir}/.pgbackrest_stanza_create_timeout"));
        //
        // Intentionally NOT clearing .pgbackrest_invalid_bucket here:
        // validate_wal_archive_bucket unsets WAL_ARCHIVE_BUCKET on
        // rejection, which makes this function see archive_enabled=false
        // and would race-delete the sentinel the validator just wrote
        // (~20 ms apart). The validator handles the sentinel lifecycle
        // itself: writes on a true rejection, removes when the env var
        // is unset by the operator. Letting it own that file end-to-end
        // avoids the self-overwrite.
    }

    if !recover_enabled {
        rm(format!("{data_dir}/.pitr_staging"));
        rm(format!("{data_dir}/.pitr_configured"));
        rm(format!("{data_dir}/.pgbackrest_restored"));
    }

    if !archive_enabled && !recover_enabled {
        rm("/etc/pgbackrest/pgbackrest.conf".to_string());
        rm("/etc/pgbackrest/pgbackrest-recovery-source.conf".to_string());
    }
}

fn render_pgbackrest_conf(data_dir: &str, queue_max_mib: u32) -> Result<()> {
    if env::var("PGBACKREST_REPO1_S3_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        return Ok(());
    }

    let conf_path = "/etc/pgbackrest/pgbackrest.conf";

    let cpus = detect_cpus().max(1) as i64;
    let push_max = env_or_clamp("PGBACKREST_ARCHIVE_PUSH_PROCESS_MAX", clamp(cpus / 8, 2, 8));
    let get_max = env_or_clamp("PGBACKREST_ARCHIVE_GET_PROCESS_MAX", clamp(cpus / 8, 2, 8));
    let backup_max = env_or_clamp("PGBACKREST_BACKUP_PROCESS_MAX", clamp(cpus / 4, 1, 2));
    let restore_max = env_or_clamp("PGBACKREST_RESTORE_PROCESS_MAX", clamp(cpus, 1, 32));

    info!(
        cpus = cpus,
        push = push_max,
        get = get_max,
        backup = backup_max,
        restore = restore_max,
        "pgbackrest: detected vCPU and sized process-max"
    );

    // The default pgbackrest.conf only ever has repo1 (the service's own
    // archive bucket). Recovery (which needs read access to source's
    // bucket on a fork) uses a separate
    // /etc/pgbackrest/pgbackrest-recovery-source.conf, referenced via
    // --config in restore + restore_command. Mirrors postgres-ssl PR #49.

    let retention_full = env::var("WAL_BACKUP_RETENTION_FULL")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(4);
    let retention_diff = env::var("WAL_BACKUP_RETENTION_DIFF")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(14);

    let conf = build_pgbackrest_conf(&PgbackrestConfParams {
        data_dir,
        queue_max_mib,
        push_max,
        get_max,
        backup_max,
        restore_max,
        retention_full,
        retention_diff,
    });

    fs::create_dir_all("/etc/pgbackrest").context("Failed to create /etc/pgbackrest")?;
    fs::write(conf_path, conf).context("Failed to write pgbackrest.conf")?;
    fs::set_permissions(conf_path, std::fs::Permissions::from_mode(0o640))
        .context("Failed to set pgbackrest.conf permissions")?;

    info!("pgbackrest: rendered {}", conf_path);
    Ok(())
}

/// Inputs for [`build_pgbackrest_conf`], bundled so the builder doesn't need
/// eight positional args (`clippy::too_many_arguments`) — every field maps
/// 1:1 to a line in the rendered conf, so a struct with named fields also
/// reads better at the call site than a wall of positional numbers.
#[derive(Clone, Copy)]
struct PgbackrestConfParams<'a> {
    data_dir: &'a str,
    queue_max_mib: u32,
    push_max: u32,
    get_max: u32,
    backup_max: u32,
    restore_max: u32,
    retention_full: u32,
    retention_diff: u32,
}

/// Pure conf-string builder for the main `pgbackrest.conf`, split out of
/// `render_pgbackrest_conf` so the process-max sizing (and its regression:
/// `archive-get` used to default to a flat `1`, now `clamp(cpus/8, 2, 8)`
/// like `archive-push`) is unit-testable without writing to the real
/// `/etc/pgbackrest` path.
fn build_pgbackrest_conf(params: &PgbackrestConfParams) -> String {
    let PgbackrestConfParams {
        data_dir,
        queue_max_mib,
        push_max,
        get_max,
        backup_max,
        restore_max,
        retention_full,
        retention_diff,
    } = *params;
    let spool_dir = format!("{data_dir}/pgbackrest-spool");
    format!(
        "[global]\n\
         repo1-type=s3\n\
         repo1-retention-full={retention_full}\n\
         repo1-retention-diff={retention_diff}\n\
         log-level-console=info\n\
         log-level-file=off\n\
         archive-async=y\n\
         archive-push-queue-max={queue_max_mib}MiB\n\
         archive-get-queue-max=1GiB\n\
         spool-path={spool_dir}\n\
         compress-type=zst\n\
         compress-level=3\n\
         start-fast=y\n\
         \n\
         [global:archive-push]\n\
         process-max={push_max}\n\
         \n\
         [global:archive-get]\n\
         process-max={get_max}\n\
         \n\
         [global:backup]\n\
         process-max={backup_max}\n\
         \n\
         [global:restore]\n\
         process-max={restore_max}\n\
         \n\
         [main]\n\
         pg1-path={data_dir}\n\
         pg1-port=5432\n",
    )
}

/// Render `/etc/pgbackrest/pgbackrest-recovery-source.conf` when
/// `WAL_RECOVER_FROM_*` is set. This conf is used exclusively during
/// recovery: explicit `pgbackrest restore` (empty-volume restore in
/// future) and `restore_command` for archive-get. Has only the source
/// bucket as repo1 (numbering is per-config) so post-promote
/// archive-push from the fork's main pgbackrest.conf can never fan out
/// to source's read-only bucket and 403. Mirrors postgres-ssl PR #49.
///
/// `--config` REPLACES the main pgbackrest.conf, so the async parallel
/// archive-get settings must be repeated here or staged replay silently
/// runs sync with process-max=1 — bulk WAL replay to a PITR target is
/// exactly the workload where per-segment S3 GET latency dominates.
/// Sizing and env override mirror `render_pgbackrest_conf`.
fn render_pgbackrest_recovery_source_conf(data_dir: &str) -> Result<()> {
    let bucket = match env::var("WAL_RECOVER_FROM_BUCKET") {
        Ok(b) if !b.is_empty() => b,
        _ => return Ok(()),
    };
    // Skip when this volume has already completed recovery — the source
    // bucket's read credentials are no longer needed for archive-get on
    // a long-promoted cluster (archive_command uses the main pgbackrest
    // conf, not the recovery-source conf). Rewriting on every boot leaks
    // credentials onto disk for no functional benefit. Mirrors
    // postgres-ssl wrapper.sh's L6 gate.
    if Path::new(&format!("{data_dir}/.pgbackrest_restored")).exists()
        || Path::new(&format!("{data_dir}/.pitr_configured")).exists()
    {
        return Ok(());
    }
    let key = env::var("WAL_RECOVER_FROM_KEY").unwrap_or_default();
    let secret = env::var("WAL_RECOVER_FROM_SECRET").unwrap_or_default();
    let region = env::var("WAL_RECOVER_FROM_REGION").unwrap_or_default();
    let endpoint = env::var("WAL_RECOVER_FROM_ENDPOINT").unwrap_or_default();
    let uri_style = env::var("WAL_RECOVER_FROM_S3_URI_STYLE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "path".to_string());
    let path = env::var("WAL_RECOVER_FROM_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/pgbackrest".to_string());

    let cpus = detect_cpus().max(1) as i64;
    let get_max = env_or_clamp("PGBACKREST_ARCHIVE_GET_PROCESS_MAX", clamp(cpus / 8, 2, 8));

    let conf = build_pgbackrest_recovery_source_conf(&RecoverySourceConfParams {
        data_dir,
        bucket: &bucket,
        key: &key,
        secret: &secret,
        region: &region,
        endpoint: &endpoint,
        uri_style: &uri_style,
        path: &path,
        get_max,
    });

    fs::create_dir_all("/etc/pgbackrest").context("Failed to create /etc/pgbackrest")?;
    let conf_path = "/etc/pgbackrest/pgbackrest-recovery-source.conf";
    fs::write(conf_path, conf).context("Failed to write pgbackrest-recovery-source.conf")?;
    fs::set_permissions(conf_path, std::fs::Permissions::from_mode(0o640))
        .context("Failed to set pgbackrest-recovery-source.conf permissions")?;
    info!("pgbackrest: rendered {}", conf_path);
    Ok(())
}

/// Inputs for [`build_pgbackrest_recovery_source_conf`], bundled for the same
/// reason as [`PgbackrestConfParams`] — avoids a nine-positional-argument
/// signature (`clippy::too_many_arguments`) where several args share the
/// `&str` type and are easy to transpose by position (`region` vs.
/// `endpoint` vs. `uri_style`) but not by name.
#[derive(Clone, Copy)]
struct RecoverySourceConfParams<'a> {
    data_dir: &'a str,
    bucket: &'a str,
    key: &'a str,
    secret: &'a str,
    region: &'a str,
    endpoint: &'a str,
    uri_style: &'a str,
    path: &'a str,
    get_max: u32,
}

/// Pure conf-string builder for `pgbackrest-recovery-source.conf`, split out
/// of `render_pgbackrest_recovery_source_conf` for unit testing. This PR's
/// regression to guard: `--config` REPLACES the main conf wholesale, so
/// without `archive-async=y` + `archive-get-queue-max` + the
/// `[global:archive-get]` process-max block repeated here, staged PITR replay
/// silently runs sync at process-max=1 even when the main conf is tuned for
/// parallel prefetch.
fn build_pgbackrest_recovery_source_conf(params: &RecoverySourceConfParams) -> String {
    let RecoverySourceConfParams {
        data_dir,
        bucket,
        key,
        secret,
        region,
        endpoint,
        uri_style,
        path,
        get_max,
    } = *params;
    let spool_dir = format!("{data_dir}/pgbackrest-spool");
    format!(
        "[global]\n\
         log-level-console=info\n\
         log-level-file=off\n\
         archive-async=y\n\
         archive-get-queue-max=1GiB\n\
         spool-path={spool_dir}\n\
         repo1-type=s3\n\
         repo1-s3-bucket={bucket}\n\
         repo1-s3-key={key}\n\
         repo1-s3-key-secret={secret}\n\
         repo1-s3-region={region}\n\
         repo1-s3-endpoint={endpoint}\n\
         repo1-s3-uri-style={uri_style}\n\
         repo1-path={path}\n\
         \n\
         [global:archive-get]\n\
         process-max={get_max}\n\
         \n\
         [main]\n\
         pg1-path={data_dir}\n\
         pg1-port=5432\n",
    )
}

/// Marker comment that opens the recovery block this binary manages in
/// postgresql.auto.conf. [`build_pitr_managed_block`] writes it and
/// [`strip_pitr_managed_block`] removes it — the two are deliberately
/// symmetric so the block has at most one live copy and is gone once a
/// restore completes.
const PITR_MANAGED_MARKER: &str = "# managed by pgbackrest-recovery (patroni-runner)";

/// GUC names that belong to the managed block. Stripping removes every one
/// of these that follows the marker — including BOTH recovery-target types,
/// so a target-type change across retries (time → xid) can never leave the
/// stale GUC behind (Postgres refuses to start with "multiple recovery
/// targets specified", an unrecoverable boot-loop).
const PITR_MANAGED_KEYS: [&str; 4] = [
    "restore_command",
    "recovery_target_time",
    "recovery_target_xid",
    "recovery_target_action",
];

/// Render the managed recovery block, leading separator included. Inputs are
/// already single-quote-escaped by the caller.
fn build_pitr_managed_block(
    target_param: &str,
    escaped_target: &str,
    escaped_restore: &str,
) -> String {
    format!(
        "\n{PITR_MANAGED_MARKER}\n\
         restore_command = '{escaped_restore}'\n\
         {target_param} = '{escaped_target}'\n\
         recovery_target_action = 'promote'\n",
    )
}

/// Remove every managed recovery block from `contents`: each marker line,
/// the managed-key lines that follow it, and the blank separator line the
/// writer put before it. Handles multiple accumulated copies (the raw-append
/// bug this symmetry fixes left duplicates on already-deployed volumes) and
/// blocks whose target type differs from the current one. Everything else —
/// user-set GUCs included — passes through untouched.
fn strip_pitr_managed_block(contents: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut lines = contents.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() == PITR_MANAGED_MARKER {
            if out.last().is_some_and(|l| l.trim().is_empty()) {
                out.pop();
            }
            while lines.peek().is_some_and(|l| {
                let l = l.trim_start();
                PITR_MANAGED_KEYS.iter().any(|key| l.starts_with(key))
            }) {
                lines.next();
            }
            continue;
        }
        out.push(line);
    }
    let mut result = out.join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    result
}

/// Strip the managed recovery block from the file at `auto_conf_path`.
/// No-op when the file or the marker is absent.
fn strip_pitr_managed_block_from_file(auto_conf_path: &str) -> std::io::Result<()> {
    if !Path::new(auto_conf_path).exists() {
        return Ok(());
    }
    let contents = fs::read_to_string(auto_conf_path)?;
    if !contents.contains(PITR_MANAGED_MARKER) {
        return Ok(());
    }
    fs::write(auto_conf_path, strip_pitr_managed_block(&contents))
}

/// Stage PITR replay before Patroni starts Postgres.
///
/// When `POSTGRES_RECOVERY_TARGET_TIME` (or `_XID`) is set, writes
/// `recovery.signal` + recovery settings to postgresql.auto.conf so Postgres
/// enters archive recovery on boot, replays WAL from `repo1` (the source
/// bucket via the `WAL_RECOVER_FROM_*` translation) to the target, then
/// promotes.
///
/// `recovery_target_xid` wins over `recovery_target_time` when both are set
/// because it's the only target type postgres can match exactly on an idle
/// source. `recovery_target_time` requires postgres to observe a WAL record
/// with timestamp > target before declaring "target reached" and firing
/// `recovery_target_action=promote`; on an idle DB no such record exists, so
/// recovery FATALs and the cluster either crash-loops or hangs in
/// hot_standby read-only mode. `recovery_target_xid` matches an exact
/// transaction ID — applying the target xid's commit is unambiguously
/// "target reached." The picker (mono's createServiceFromPITR mutation) sets
/// `_XID` when it clamped target down to `lastCommittedTxnAt`. Mirrors
/// postgres-ssl PR #63.
///
/// Two filesystem stamps coordinate "exactly once per successful promote":
///   - `.pitr_staging`: written when we hand recovery off to Postgres. Means
///     a replay attempt is in flight or last attempt didn't promote yet.
///   - `.pitr_configured`: written on the boot AFTER Postgres consumes
///     `recovery.signal` (which Postgres removes only on successful
///     promote). Means PITR is done and must not run again on this volume.
///     Once set, subsequent boots skip recovery even if
///     `POSTGRES_RECOVERY_TARGET_TIME` is changed. To re-run PITR with a
///     different target the operator must restore from a fresh snapshot
///     (or, advanced: rm the marker).
///
/// A failed replay (bad target, missing WAL, bad creds) leaves
/// `.pitr_staging` behind WITHOUT `.pitr_configured` — the operator can fix
/// env vars and restart, and the next boot will re-stage cleanly. Re-staging
/// REPLACES the managed recovery block in postgresql.auto.conf rather than
/// appending a second copy, and the done-marker stamp strips it entirely, so
/// the block has at most one live copy and no residue outlives a successful
/// promote (see [`build_pitr_managed_block`] / [`strip_pitr_managed_block`]).
///
/// Source-path divergence detection is gone: under the new-service restore
/// design, the restored cluster has its own bucket (`WAL_ARCHIVE_*`) and
/// reads from the source's bucket via the distinct `WAL_RECOVER_FROM_*`
/// repo, so no shared write path exists to corrupt.
fn configure_pitr_recovery(config: &Config) -> Result<()> {
    let data_dir = &config.data_dir;
    let staging = format!("{data_dir}/.pitr_staging");
    let done = format!("{data_dir}/.pitr_configured");
    let signal = format!("{data_dir}/recovery.signal");
    let pg_version = format!("{data_dir}/PG_VERSION");
    let restored_marker = format!("{data_dir}/.pgbackrest_restored");
    let auto_conf_path = format!("{data_dir}/postgresql.auto.conf");

    // Pick the recovery target type. xid wins over time when both are set —
    // see fn-doc above. Caller already gated on at least one being Some.
    let (target_param, target_value) = if let Some(xid) = config.pitr_target_xid.as_deref() {
        ("recovery_target_xid", xid)
    } else if let Some(time) = config.pitr_target_time.as_deref() {
        ("recovery_target_time", time)
    } else {
        return Ok(());
    };

    // Log restore-gate state up front so post-mortems on "why did/didn't
    // PITR run" don't require guessing. Mirrors postgres-ssl PR #57.
    info!(
        wal_recover_from_bucket = config.wal_recover_from_bucket.is_some(),
        postgres_recovery_target_time = ?config.pitr_target_time,
        postgres_recovery_target_xid = ?config.pitr_target_xid,
        pg_version_present = Path::new(&pg_version).exists(),
        restored_marker_present = Path::new(&restored_marker).exists(),
        pitr_staging_present = Path::new(&staging).exists(),
        pitr_configured_present = Path::new(&done).exists(),
        pgdata_path = %data_dir,
        "pgbackrest: restore-gate state"
    );

    // Without WAL_RECOVER_FROM_BUCKET the recovery-source conf never gets
    // rendered (render_pgbackrest_recovery_source_conf early-returns when
    // the bucket env is unset), so the staged restore_command would
    // archive-get FATAL at boot. Mirrors postgres-ssl wrapper.sh's
    // `[ -z "$WAL_RECOVER_FROM_BUCKET" ] && return 0` guard.
    if config.wal_recover_from_bucket.is_none() {
        info!("pgbackrest: WAL_RECOVER_FROM_BUCKET unset — skipping recovery staging");
        return Ok(());
    }

    if Path::new(&done).exists() {
        return Ok(());
    }

    // Postgres removes recovery.signal on successful promote. If staging is
    // present and the signal is gone, replay completed on a prior boot and
    // we just need to stamp the done marker.
    if Path::new(&staging).exists() && !Path::new(&signal).exists() {
        let _ = fs::remove_file(&staging);
        fs::write(&done, "").context("Failed to write PITR done marker")?;
        // The managed recovery block has done its job — strip it so no
        // restore_command / recovery_target residue outlives the promote.
        // Best-effort (the restore IS complete either way), but loud: a
        // leftover block would be re-stripped on the next staging, and until
        // then it sits inert in postgresql.auto.conf.
        if let Err(e) = strip_pitr_managed_block_from_file(&auto_conf_path) {
            warn!(
                error = %e,
                path = %auto_conf_path,
                "pgbackrest: failed to strip the managed recovery block after promote"
            );
        }
        info!("pgbackrest: previous PITR replay completed; marker written");
        return Ok(());
    }

    // Recovery uses the dedicated recovery-source conf (only contains the
    // source bucket as its repo1) so archive-get during replay can never
    // touch the service's own bucket. Post-promote archive_command reads
    // /etc/pgbackrest/pgbackrest.conf which has only the service's repo1.
    // Mirrors postgres-ssl PR #49.
    let restore_cmd = "pgbackrest --config=/etc/pgbackrest/pgbackrest-recovery-source.conf --stanza=main archive-get %f %p";
    let escaped_target = target_value.replace('\'', "''");
    let escaped_restore = restore_cmd.replace('\'', "''");

    // Strip any previous managed block before appending the fresh one. This
    // path runs on EVERY boot while recovery.signal persists (a failed
    // replay leaves both the signal and the block behind), so a raw append
    // accumulated duplicates — and a target-type change across retries
    // (time → xid) left BOTH recovery_target GUCs set, which Postgres
    // refuses outright ("multiple recovery targets specified"): an
    // unrecoverable boot-loop. Lifecycle: at most one live copy, removed on
    // completion (see the done-marker branch above).
    let existing = if Path::new(&auto_conf_path).exists() {
        fs::read_to_string(&auto_conf_path).context("Failed to read postgresql.auto.conf")?
    } else {
        String::new()
    };
    let mut new_contents = strip_pitr_managed_block(&existing);
    new_contents.push_str(&build_pitr_managed_block(
        target_param,
        &escaped_target,
        &escaped_restore,
    ));
    fs::write(&auto_conf_path, new_contents).context("Failed to write recovery settings")?;

    // The recovery-source conf runs archive-get in async mode, which needs
    // the spool dir. `spawn_bootstrap_stanza_create` only mkdirs it after
    // pg_isready — too late for replay, whose archive-gets start first.
    // Safe here: pgdata is populated (postgresql.auto.conf was just written
    // into it), so Patroni's empty-dir bootstrap gate is not in play.
    let spool_dir = format!("{data_dir}/pgbackrest-spool");
    fs::create_dir_all(&spool_dir).context("Failed to create pgbackrest spool dir")?;
    fs::set_permissions(&spool_dir, std::fs::Permissions::from_mode(0o750))
        .context("Failed to set pgbackrest spool dir permissions")?;

    fs::File::create(&signal).context("Failed to create recovery.signal")?;
    fs::write(&staging, "").context("Failed to write PITR staging marker")?;

    info!(target_param = %target_param, target = %target_value, "pgbackrest PITR replay staged");
    Ok(())
}

/// Post-Postgres-ready pgBackRest setup: mkdir the spool dir, then run
/// stanza-create. Forks a background poller so patroni-runner can stay on
/// its existing exec path.
///
/// Spool creation is deferred to here (rather than inside
/// `render_pgbackrest_conf` at boot) because pre-creating
/// `$PGDATA/pgbackrest-spool` would dirty pgdata before Patroni's first
/// bootstrap and trip its "data dir is not empty, but system ID is
/// invalid" gate on fresh replicas. Mirrors postgres-ssl, where the spool
/// is mkdir'd by an init script under `/docker-entrypoint-initdb.d` —
/// upstream's docker-entrypoint runs those only after `initdb` populates
/// pgdata. By the time `pg_isready` succeeds, Patroni has clone+started
/// Postgres, so adding a sibling subdir is harmless. Idempotent on
/// subsequent boots.
///
/// stanza-create is idempotent: a matching stanza in the repo is a no-op;
/// a mismatch errors loudly. Skipped in dual-repo mode (restored cluster
/// with PITR re-enabled) — pgBackRest's stanza-create operates against
/// all configured repos and we don't want to touch the source's. Spool
/// creation still happens in dual-repo mode because the restored cluster
/// will start archiving its own WAL once promoted.
fn spawn_bootstrap_stanza_create() {
    if env::var("WAL_ARCHIVE_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        return;
    }

    let data_dir = env::var("PGDATA").unwrap_or_else(|_| "/var/lib/postgresql/data".to_string());
    let dual_repo_mode = env::var("WAL_RECOVER_FROM_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some();

    tokio::spawn(async move {
        let timeout_sentinel = format!("{data_dir}/.pgbackrest_stanza_create_timeout");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            if tokio::time::Instant::now() >= deadline {
                warn!("pgbackrest: timed out waiting for Postgres before stanza-create");
                // Drop a sentinel so the monitor can distinguish
                // "stanza bootstrap timed out" from "archiving never
                // enabled" — the latter has archive_command unset; the
                // former has archive_command set but no stanza in the
                // bucket. Cleared on success below and by
                // clear_pgbackrest_state_if_disabled on archive disable.
                let _ = fs::write(&timeout_sentinel, "pg_isready-timeout\n");
                let _ =
                    fs::set_permissions(&timeout_sentinel, std::fs::Permissions::from_mode(0o640));
                return;
            }
            let probe = tokio::process::Command::new("pg_isready")
                .args(["-h", "127.0.0.1", "-p", "5432", "-U", "postgres", "-q"])
                .status()
                .await;
            if matches!(probe, Ok(s) if s.success()) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // pgdata is now populated; safe to add the spool subdir.
        let spool_dir = format!("{data_dir}/pgbackrest-spool");
        match fs::create_dir_all(&spool_dir) {
            Ok(()) => {
                if let Err(e) =
                    fs::set_permissions(&spool_dir, std::fs::Permissions::from_mode(0o750))
                {
                    warn!(error = %e, "pgbackrest: failed to set spool permissions");
                } else {
                    info!(spool_dir = %spool_dir, "pgbackrest: spool dir ready");
                }
            }
            Err(e) => warn!(error = %e, "pgbackrest: failed to create spool dir"),
        }

        if dual_repo_mode {
            warn!(
                "pgbackrest: skipping stanza-create — both WAL_RECOVER_FROM_* and \
                 WAL_ARCHIVE_* are set; clear the recover-from vars then restart"
            );
            return;
        }

        // Wait for this node to become primary before running stanza-create.
        // pg_isready above only confirms Postgres accepts connections — a
        // replica in hot_standby mode passes that check while still being
        // in recovery. pgBackRest stanza-create connects to the local
        // Postgres instance and fails with error 056 ("unable to find
        // primary cluster") if it finds a standby. We must wait here for
        // pg_is_in_recovery() to return false (i.e., Patroni has promoted
        // this node) before proceeding. On permanent replicas this loop
        // never exits and the task exits at the deadline — replicas don't
        // own the stanza.
        loop {
            if tokio::time::Instant::now() >= deadline {
                warn!("pgbackrest: timed out waiting for primary promotion before stanza-create");
                // Same sentinel as the pg_isready timeout above —
                // surfaces "stanza bootstrap did not run" to the monitor
                // regardless of which deadline branch fired. Replicas
                // legitimately reach this on every boot; they don't have
                // WAL_ARCHIVE_BUCKET=set without also being primary in
                // production, so the sentinel reflects an actual
                // misconfiguration rather than normal HA topology.
                let _ = fs::write(&timeout_sentinel, "promotion-timeout\n");
                let _ =
                    fs::set_permissions(&timeout_sentinel, std::fs::Permissions::from_mode(0o640));
                return;
            }
            let out = tokio::process::Command::new("psql")
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
                .await;
            match out {
                Ok(o) if o.status.success() => {
                    if String::from_utf8_lossy(&o.stdout).trim() == "f" {
                        break;
                    }
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // Re-derive the repo path now that pg_control is on disk. This is
        // the canonical first chance to do it on a fresh-cluster path.
        let repo_path = derive_pgbackrest_repo_path(&data_dir);
        env::set_var("PGBACKREST_REPO1_PATH", &repo_path);
        info!(repo_path = %repo_path, "pgbackrest: using per-cluster repo1-path");

        // PGHOST/PGPORT must not leak into pgbackrest's libpq calls — a
        // customer-supplied PGHOST=${{ Postgres.RAILWAY_PRIVATE_DOMAIN }}
        // would point libpq at the privnet domain and time out
        // (`unable to find primary cluster`). The parent already cleared
        // these before forking this task, but a Command-level remove is
        // belt-and-suspenders so future refactors can't reintroduce the
        // leak. Mirrors postgres-ssl PR #51.
        //
        // Call pgbackrest directly (no `gosu postgres` wrapper). In ssl,
        // wrapper.sh runs as root and gosu drops to postgres; in HA,
        // postgres-wrapper already dropped to postgres before exec'ing
        // patroni-runner, so we're non-root here — gosu's setgroups(0)
        // fails with EPERM ("error: failed switching to 'postgres'") and
        // stanza-create never completes, breaking archive-push.
        loop {
            let out = tokio::process::Command::new("pgbackrest")
                .args(["--stanza=main", "stanza-create"])
                .env_remove("PGHOST")
                .env_remove("PGPORT")
                .status()
                .await;
            match out {
                Ok(s) if s.success() => {
                    info!("pgbackrest: stanza-create completed");
                    // Clear the timeout sentinel — a successful
                    // stanza-create either arrived inside the deadline
                    // (no sentinel) or after a previous boot's timeout
                    // (stale sentinel from disk). Either way, the
                    // current state is "stanza present"; the dashboard
                    // should treat the timeout as resolved.
                    let _ = fs::remove_file(&timeout_sentinel);
                    break;
                }
                Ok(s) => {
                    warn!(status = ?s, "pgbackrest: stanza-create failed, retrying in 30s");
                }
                Err(e) => {
                    warn!(error = %e, "pgbackrest: stanza-create invocation failed, retrying in 30s");
                }
            }
            tokio::time::sleep(Duration::from_secs(30)).await;

            // Re-check leadership before the next attempt. If this node was
            // demoted after passing the promotion gate above, exit cleanly —
            // the backup watcher (already leader-gated) will run stanza-create
            // via exit-55 recovery once this node is promoted again.
            let recovery_check = tokio::process::Command::new("psql")
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
                .await;
            let still_primary = matches!(
                recovery_check,
                Ok(ref o) if o.status.success()
                    && String::from_utf8_lossy(&o.stdout).trim() == "f"
            );
            if !still_primary {
                info!(
                    "pgbackrest: node is no longer primary, stopping stanza-create bootstrap \
                     (watcher will recover on next promotion)"
                );
                return;
            }
        }
    });
}

/// PID of the real patroni-runner process, for the mini-init's
/// signal-forwarding handler. Written exactly once, before the handlers
/// are installed.
static MINI_INIT_CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn mini_init_forward(sig: nix::libc::c_int) {
    let pid = MINI_INIT_CHILD.load(Ordering::Relaxed);
    if pid > 0 {
        // Async-signal-safe: raw kill only.
        unsafe { nix::libc::kill(pid, sig) };
    }
}

/// Run as a minimal init: fork, let the child continue as the real
/// program, and keep the parent behind as the process the kernel hands
/// orphans to — reaping them forever and forwarding terminal signals to
/// the child.
///
/// This process is PID 1 in the container (postgres-wrapper and gosu both
/// exec into us), and PID 1 inherits every orphaned descendant. Reaping
/// them is not cosmetic: Patroni's stop-timeout path
/// (`PostmasterProcess.signal_kill`) SIGKILLs the postmaster BEFORE its
/// children, so those children die as orphans — and then Patroni blocks in
/// `psutil.wait_procs(children + [self])`, which has no timeout and polls
/// for the PIDs to vanish. An unreaped zombie's PID never vanishes, so the
/// async task thread that issued the stop (e.g. a reinitialize) hangs
/// forever, taking Patroni's single task slot with it: every later
/// `/reinitialize` logs "Cancelling long running task", then blocks on
/// `_finish_event.wait()` for a thread that can never finish. Observed
/// end-state: "reinitialize in progress" for the rest of the container's
/// life, immune to postgres dying and to re-issued reinitializes alike.
///
/// The fork-parent design (rather than a `waitpid(-1)` loop inside the
/// tokio process) is what makes blanket reaping safe: the parent has
/// exactly one direct child of its own, so `waitpid(-1)` can only ever
/// collect that child or orphans — never the exit status of a subprocess
/// the real program is still `wait()`ing on.
///
/// Returns in the CHILD; the parent never returns (it exits with the
/// child's status once the child dies).
fn run_as_mini_init() -> Result<()> {
    // Belt-and-suspenders for any future entrypoint shuffle that puts a
    // non-reaping process above us: subreaper routes our subtree's orphans
    // here even when we are not PID 1. (prctl is Linux-only; the image is.)
    #[cfg(target_os = "linux")]
    let _ = nix::sys::prctl::set_child_subreaper(true);

    let child = match unsafe { nix::unistd::fork() }.context("mini-init fork failed")? {
        ForkResult::Child => return Ok(()),
        ForkResult::Parent { child } => child,
    };

    MINI_INIT_CHILD.store(child.as_raw(), Ordering::Relaxed);
    for sig in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGQUIT, Signal::SIGHUP] {
        // SAFETY: handler is async-signal-safe (atomic load + kill).
        unsafe {
            let _ = nix::sys::signal::signal(sig, SigHandler::Handler(mini_init_forward));
        }
    }

    loop {
        match waitpid(Pid::from_raw(-1), None) {
            Ok(WaitStatus::Exited(pid, code)) if pid == child => std::process::exit(code),
            Ok(WaitStatus::Signaled(pid, sig, _)) if pid == child => {
                std::process::exit(128 + sig as i32)
            }
            // An orphan reaped — the entire point of standing here.
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => {}
            // No children at all: the real child is gone without us seeing
            // its status (should not happen; don't spin on it).
            Err(nix::errno::Errno::ECHILD) => std::process::exit(0),
            Err(e) => {
                eprintln!("mini-init: waitpid failed: {e}; exiting");
                std::process::exit(1);
            }
        }
    }
}

fn main() -> Result<()> {
    // Must run before the tokio runtime exists: fork() and threads don't mix.
    run_as_mini_init()?;

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the tokio runtime")?
        .block_on(async_main())
}

/// Refresh collation versions once this node is confirmed primary. Patroni's
/// on_role_change callback only fires on an actual role *transition* — a
/// plain container restart that leaves the same node as primary (exactly
/// what a routine image redeploy does) never fires it at all, confirmed
/// empirically: instrumenting on-role-change and doing a bare `docker
/// restart` on a standing primary produced zero invocations. Any image bump
/// (a minor version is enough — new glibc, same on-disk collation stamp)
/// would otherwise leave the mismatch WARNING noisy indefinitely, until
/// whatever next real failover happens to occur. This covers that gap the
/// same way spawn_bootstrap_stanza_create covers the analogous first-boot
/// gap for pgbackrest's repo-path marker (see the comment on that call
/// site) — unconditional here, since collation refresh has no
/// WAL_ARCHIVE_BUCKET gate to piggyback on.
///
/// Safe to double-run with an on_role_change-triggered refresh (e.g. right
/// after an actual promotion): refresh_collation_versions's SQL already
/// no-ops per-database once nothing is mismatched.
fn spawn_collation_refresh() {
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            let probe = tokio::process::Command::new("pg_isready")
                .args(["-h", "127.0.0.1", "-p", "5432", "-U", "postgres", "-q"])
                .status()
                .await;
            if matches!(probe, Ok(s) if s.success()) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // pg_isready above accepts connections from a replica in
        // hot_standby mode too — wait for primary specifically, since
        // ALTER DATABASE fails with a read-only-transaction error on a
        // standby. A permanent replica never passes this and just times
        // out at the deadline; it gets the fix through WAL replication
        // once the primary refreshes, not by running this itself.
        loop {
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            let out = tokio::process::Command::new("psql")
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
                .await;
            let is_primary = matches!(
                out,
                Ok(ref o) if o.status.success()
                    && String::from_utf8_lossy(&o.stdout).trim() == "f"
            );
            if is_primary {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        refresh_collation_versions();
    });
}

async fn async_main() -> Result<()> {
    let _guard = init_logging("patroni-runner");

    let telemetry = Telemetry::from_env("postgres-ha");

    // Major-upgrade boot guard, before ANYTHING touches the volume or the data
    // directory — including the bucket validator's sentinel write below and the
    // pg_hba adoption patch further down. Fail-stop by design: a marker that is
    // not `completed` means the upgrade workflow owns this volume and the data
    // directory may be absent or half-promoted, and a major mismatch means the
    // tag was changed without upgrading the data files. Either one would
    // otherwise reach the incomplete-clone wipe, or Patroni's own bootstrap,
    // and lose data. A `reseed` marker passes this guard on purpose — see
    // handle_reseed_marker, which resolves it right after config is built.
    // The image's major comes from the installed server tree (see
    // major_upgrade::image_major) — baked into the image, so a stray
    // user-set PG_MAJOR service variable can't refuse a healthy boot.
    // Unknown means the version guard abstains rather than guessing.
    let volume_root = volume_root();

    // At most one node container runs against this volume at a time: wait
    // for a previous container to release it before anything below touches
    // the data directory (see volume_lock for the overlap rationale, and why
    // the lock file is shared with the standalone postgres-ssl image). Bound
    // for the rest of this function, same lifetime idiom as the upgrade lock
    // below. Fail-stop on timeout — the restart policy retries the boot.
    let _runtime_volume_lock = postgres_patroni::volume_lock::acquire_volume_runtime_lock(
        &volume_root,
        &postgres_patroni::pgdata(),
    )?;

    // Shared, container-lifetime flock on the SAME lock file upgrade-job.sh
    // takes exclusively for its own run (see major_upgrade::
    // take_volume_upgrade_lock). Bound to `_upgrade_volume_lock` so it lives
    // for the rest of this function — i.e. for as long as this process is
    // running Patroni/Postgres against the volume — and releases automatically
    // on exit either way. Checked before the marker/version guards below: if
    // a job holds the volume right now, that refusal is more informative than
    // whatever the marker happens to say mid-job.
    let _upgrade_volume_lock = match major_upgrade::take_volume_upgrade_lock(
        &volume_root,
        &postgres_patroni::pgdata(),
    ) {
        Ok(lock) => lock,
        Err(reason) => {
            telemetry.send(TelemetryEvent::MajorUpgradeBootRefused {
                node: env::var("PATRONI_NAME").unwrap_or_else(|_| "unknown".to_string()),
                reason: reason.clone(),
            });
            anyhow::bail!("{reason}");
        }
    };

    let image_major = major_upgrade::image_major();
    if let Some(reason) = major_upgrade::boot_refusal_reason(
        &volume_root,
        &postgres_patroni::pgdata(),
        image_major.as_deref(),
    ) {
        telemetry.send(TelemetryEvent::MajorUpgradeBootRefused {
            node: env::var("PATRONI_NAME").unwrap_or_else(|_| "unknown".to_string()),
            reason: reason.clone(),
        });
        anyhow::bail!("{reason}");
    }

    // Screen WAL_ARCHIVE_BUCKET shape before translation so a junk value
    // (unresolved Railway template ref, raw bucket-id UUID, whitespace)
    // doesn't get exported as PGBACKREST_REPO1_S3_BUCKET — pgBackRest
    // would then hard-fail every archive_command and the
    // archive-push-wrapper's pg_wal threshold would eventually trip,
    // creating a real PITR gap from what is actually an upstream wiring
    // bug. Mirrors postgres-ssl PR #57 (validate_wal_archive_bucket).
    // Pass volume_root (not PGDATA) so the sentinel survives Patroni's
    // bootstrap wipe of /pgdata on fresh volumes.
    validate_wal_archive_bucket(&volume_root);

    // Translate the WAL_* env contract into pgBackRest-native PGBACKREST_*
    // before anything reads either set. Done first so Config::from_env() and
    // every downstream invocation (patroni archive_command, the wrapper
    // script, stanza-create) see the same translated env.
    translate_wal_env_to_pgbackrest();

    // Capture health server config BEFORE clearing PG* env vars
    let health_config = HealthServerConfig::from_env();

    let mut config = Config::from_env()?;

    info!(
        node = %config.name,
        address = %config.connect_address,
        "=== Patroni Runner ==="
    );

    let bootstrap_marker = format!("{}/.patroni_bootstrap_complete", volume_root);

    // Consume a reseed marker before anything else reads or patches the data
    // directory: a cross-major pgdata is wiped here (so the adoption patch and
    // the pg_control checks below see the post-wipe state), and a matching one
    // just sheds the marker.
    handle_reseed_marker(&config, &volume_root, image_major.as_deref(), &telemetry).await?;

    // Handle data adoption from vanilla PostgreSQL
    if config.adopt_existing_data {
        update_pg_hba_for_replication(&config)?;
    }

    let pg_control_path = format!("{}/global/pg_control", config.data_dir);
    let has_pg_control = Path::new(&pg_control_path).exists();
    let has_marker = Path::new(&bootstrap_marker).exists();

    if config.adopt_existing_data && has_pg_control && !has_marker {
        info!("PATRONI_ADOPT_EXISTING_DATA=true - migrating from vanilla PostgreSQL");
        fs::write(&bootstrap_marker, "").context("Failed to create bootstrap marker")?;
    } else if has_pg_control && has_marker {
        info!("Found valid data with bootstrap marker");
    } else if has_pg_control {
        info!("Found pg_control but NO bootstrap marker - stale data");
    } else {
        info!("No PostgreSQL data found");
    }

    // On a volume that already carries a bootstrapped cluster, the passwords
    // its roles were created with win over the variables: Patroni never
    // re-syncs role passwords, so rendering drifted variables into patroni.yml
    // would only break replication/rewind against the leader (see
    // patroni::credential_pin). Must run before generate_patroni_config and
    // before anything else reads config.*_pass.
    let has_cluster_data = has_pg_control && has_marker;
    let pin_outcome = apply_credential_pin(
        &mut config,
        has_cluster_data,
        credentials_from_env_requested(),
        &telemetry,
    );
    info!(outcome = ?pin_outcome, "credential pin reconciled");

    // Recover the debris of an interrupted clone. A non-empty data directory
    // with NO pg_control is what a pg_basebackup killed mid-stream leaves
    // behind (it writes global/pg_control LAST). Patroni refuses such a dir
    // ("data dir is not empty, but system ID is invalid; consider doing
    // reinitialize") and never re-clones it, so the replica is wedged
    // permanently. Wipe it here — BEFORE Patroni starts, so there is never an
    // in-progress clone to destroy — but only when a DIFFERENT member holds
    // the leader lock. That proves a clone source exists AND guarantees we
    // never wipe the primary's own data (a node missing pg_control cannot
    // itself be a healthy leader). Without a distinct leader we leave the dir
    // for manual recovery rather than risk wiping the only copy.
    if has_pg_control {
        // A complete clone is on disk, so whatever interrupted the previous ones is
        // over. Clearing here (rather than never) keeps the cap scoped to one
        // episode: a future, unrelated interruption gets its own full budget.
        clear_clone_wipe_attempts(&volume_root);
    }

    if !has_pg_control && data_dir_nonempty(&config.data_dir) {
        // Redundant with the boot guard above, deliberately: this is the call
        // that destroys data, so it states its own precondition rather than
        // inheriting one from fifty lines earlier. A missing pg_control during
        // an upgrade window is the EXPECTED mid-swap state, not clone debris.
        if major_upgrade::upgrade_in_flight(&volume_root) {
            anyhow::bail!(
                "Refusing to wipe {} — a major version upgrade is in progress on this volume. \
                 A missing pg_control is expected mid-upgrade and is not clone debris.",
                config.data_dir
            );
        }
        let dedicated = pgdata_is_dedicated_subdir(&config.data_dir, &volume_root);
        // Skip the etcd probe entirely when pgdata is the volume root — we
        // won't wipe regardless, so there's no point asking who the leader is.
        let leader = if dedicated {
            probe_cluster_leader(&config).await
        } else {
            None
        };
        let prior_wipes = read_clone_wipe_attempts(&volume_root);
        let safe_to_wipe = should_wipe_incomplete_clone(
            has_pg_control,
            true,
            dedicated,
            leader.as_deref(),
            &config.name,
        );
        if safe_to_wipe && prior_wipes >= MAX_CLONE_WIPE_ATTEMPTS {
            // Every wipe so far has been followed by another incomplete clone, so
            // the interruption is not transient and wiping again would destroy the
            // partial data for nothing — while also re-arming the same loop. The
            // usual cause is capacity (a replica volume smaller than the primary,
            // or one that has filled up), which no amount of re-cloning fixes.
            // Leave the directory intact and make the reason legible instead.
            let free = available_bytes(&volume_root);
            warn!(
                data_dir = %config.data_dir,
                attempts = prior_wipes,
                available_bytes = ?free,
                "Incomplete clone detected, but {} previous wipes already failed to complete a clone — refusing to wipe again (usually a volume too small for the primary, or full)",
                prior_wipes
            );
            telemetry.send(TelemetryEvent::IncompleteCloneWipeCapped {
                node: config.name.clone(),
                attempts: prior_wipes,
                available_bytes: free,
            });
        } else if safe_to_wipe {
            warn!(
                data_dir = %config.data_dir,
                leader = %leader.as_deref().unwrap_or("?"),
                attempt = prior_wipes + 1,
                max_attempts = MAX_CLONE_WIPE_ATTEMPTS,
                "Incomplete clone detected (non-empty pgdata, missing pg_control) — wiping so Patroni re-clones from the leader"
            );
            // Record BEFORE the destructive call: a wipe that starts and then dies
            // (OOM, SIGKILL, the container going away mid-remove) still consumed an
            // attempt, and a ledger written afterwards would miss exactly the
            // crash-looping case the cap exists to bound.
            record_clone_wipe_attempt(&volume_root, prior_wipes + 1);
            wipe_pgdata_contents(&config.data_dir)
                .context("Failed to wipe incomplete-clone data directory")?;
            // Surface the recovery so the fleet monitor can see it fire (and spot
            // a wipe→reclone→wipe loop, e.g. a replica volume too small for the
            // primary). Without a telemetry event the self-heal is invisible in prod.
            telemetry.send(TelemetryEvent::IncompleteCloneWiped {
                node: config.name.clone(),
                leader: leader.as_deref().unwrap_or("unknown").to_string(),
            });
        } else {
            warn!(
                data_dir = %config.data_dir,
                volume_root = %volume_root,
                pgdata_is_dedicated_subdir = dedicated,
                leader = ?leader,
                "Incomplete clone detected (non-empty pgdata, missing pg_control) but not safe to wipe (need a distinct leader AND pgdata as a dedicated subdir) — leaving intact for manual recovery"
            );
        }
    }

    // Prevent race condition during HA conversion:
    // When PATRONI_WAIT_FOR_LEADER=true, this replica waits for the primary to
    // establish leadership before starting. This prevents empty replicas from
    // winning the election and causing data loss during conversion.
    // Only used during conversion when postgres-1 has existing data to preserve.
    if config.wait_for_leader && !has_pg_control {
        wait_for_cluster_in_etcd(&config).await?;
    }

    // Preserve logical replication across HA conversion. If the adopted
    // cluster was already running `wal_level=logical` (e.g. a Fivetran/CDC
    // pipeline replicating off the standalone DB), keep it rather than
    // downgrading to `replica` — `replica` disables logical decoding and
    // silently breaks the customer's existing replication slots. New clusters
    // (no pg_control yet) and non-logical clusters stay on the HA default of
    // `replica`, so we never tax clusters that don't need it. bootstrap.dcs
    // parameters only seed at first cluster init, so this is decided on the
    // bootstrapping primary; replicas inherit wal_level from the DCS.
    let wal_level = match read_wal_level(&config.data_dir).as_deref() {
        Some("logical") => "logical",
        _ => "replica",
    };
    if wal_level == "logical" {
        info!("Adopted cluster has wal_level=logical; preserving it in Patroni bootstrap config");
    }

    // Generate and write Patroni config
    let patroni_config = generate_patroni_config(&config, wal_level);
    fs::create_dir_all("/etc/patroni").context("Failed to create /etc/patroni directory")?;
    fs::write("/etc/patroni/patroni.yml", &patroni_config)
        .context("Failed to write patroni.yml")?;

    info!(
        scope = %config.scope,
        etcd = %config.etcd_hosts,
        "Starting Patroni"
    );

    // Prepare data directory
    fs::create_dir_all(&config.data_dir).context("Failed to create data directory")?;
    fs::set_permissions(&config.data_dir, std::fs::Permissions::from_mode(0o700))
        .context("Failed to set data directory permissions")?;

    // Wipe stale pgBackRest filesystem state for any role no longer
    // configured. Must run before the conf renderers so a disable→re-enable
    // with the same volume doesn't carry forward yesterday's last_full_at
    // (suppressing NEEDS_INITIAL_BACKUP) or recovery markers from a prior
    // restore.
    clear_pgbackrest_state_if_disabled(&config.data_dir);

    // Size archive-push-queue-max and pg_wal drop ceiling against the
    // mounted volume. Both scale DOWN from the same absolute default
    // (5 GiB) on small volumes — never up. WAL_DROP_THRESHOLD_MB is exported
    // here because the bash archive_command wrapper reads it from env;
    // patroni → postgres → archive_command inherits.
    let (wal_drop_mib, queue_max_mib) = compute_volume_thresholds(&volume_root);
    if env::var("WAL_DROP_THRESHOLD_MB")
        .ok()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        env::set_var("WAL_DROP_THRESHOLD_MB", wal_drop_mib.to_string());
    }

    // Render /etc/pgbackrest/pgbackrest.conf when archiving is enabled
    // (WAL_ARCHIVE_BUCKET set). Has only repo1 = the service's own bucket.
    render_pgbackrest_conf(&config.data_dir, queue_max_mib)?;

    // Synchronously write the per-cluster repo-path marker if pg_control
    // is on disk and archive is enabled. Without this, an existing-volume
    // first-enable-after-image-upgrade can race: Patroni starts postgres,
    // archive_command fires on the first WAL switch (≤archive_timeout=60s),
    // and the archive-push wrapper reads no marker → uses the default
    // repo1-path (bucket root, no cluster-<sysid> sub-prefix). The
    // spawn_bootstrap_stanza_create task can't write the marker until
    // pg_isready + promotion check succeed, which is later. on_role_change
    // covers post-promotion writes; this covers the first-boot path before
    // any promotion event. Fresh-init is handled separately (initdb hook).
    if env::var("WAL_ARCHIVE_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some()
        && Path::new(&format!("{}/global/pg_control", config.data_dir)).exists()
        && !Path::new(&format!("{}/.pgbackrest_repo_path", config.data_dir)).exists()
    {
        let repo_path = derive_pgbackrest_repo_path(&config.data_dir);
        info!(
            repo_path = %repo_path,
            "pgbackrest: pre-Patroni repo-path marker rendered (existing-volume first-enable path)"
        );
    }

    // Render /etc/pgbackrest/pgbackrest-recovery-source.conf when
    // WAL_RECOVER_FROM_BUCKET is set. Has only the source bucket as
    // repo1 (per-config numbering). Used by restore_command (archive-get
    // during PITR replay) and by future explicit `pgbackrest restore`.
    // Isolated from the main conf so archive-push, stanza-create, and
    // backup never fan out to source's read-only bucket.
    render_pgbackrest_recovery_source_conf(&config.data_dir)?;

    // Stage pgBackRest PITR replay if requested. No-op unless
    // POSTGRES_RECOVERY_TARGET_TIME or POSTGRES_RECOVERY_TARGET_XID is set.
    // Must run before Patroni starts Postgres so the signal file and
    // recovery settings are in place. The function logs restore-gate state
    // unconditionally and gates the actual staging on
    // WAL_RECOVER_FROM_BUCKET internally — operators see why staging was
    // skipped via the log even when no bucket is configured.
    if config.pitr_target_time.is_some() || config.pitr_target_xid.is_some() {
        configure_pitr_recovery(&config)?;
    }

    // Clear PostgreSQL environment variables to avoid conflicts
    env::remove_var("PGPASSWORD");
    env::remove_var("PGUSER");
    env::remove_var("PGHOST");
    env::remove_var("PGPORT");
    env::remove_var("PGDATABASE");

    // Set umask so pg_basebackup creates files with correct permissions (0600/0700)
    // Without this, container environments may create files too permissive for PostgreSQL
    umask(Mode::from_bits_truncate(0o077));

    // Start health server for HAProxy health checks, supervised — HAProxy
    // builds BOTH backends from its /primary and /replica answers, so it must
    // outlive any single bind/serve failure or panic instead of dying with
    // its (previously discarded) task handle.
    // It runs independently and queries PostgreSQL directly for primary/replica status
    health_server::spawn(health_config, telemetry.clone());

    // Start Patroni and run monitoring loop
    let child = start_patroni().await?;

    // Spawn a DCS reconcile task that retries indefinitely with exponential
    // backoff until it succeeds. This waits for Patroni's REST API to come up,
    // then PATCHes /config so DCS archive params match env-var intent.
    // Required because `bootstrap.dcs` only seeds DCS at first cluster init;
    // without this, env-var changes on existing clusters are silently ignored
    // by Patroni.
    //
    // Retries indefinitely (like stanza-create) so a transient etcd CAS
    // failure or Patroni startup race during bulk deployments cannot
    // permanently leave archive_mode unset. Does not abort patroni-runner.
    {
        let mut reconcile_config = Config::from_env()?;
        // Same pinned credentials as the main config (see credential_pin) —
        // a second from_env() would resurrect drifted variables.
        reconcile_config.superuser_pass = config.superuser_pass.clone();
        reconcile_config.repl_pass = config.repl_pass.clone();
        reconcile_config.app_pass = config.app_pass.clone();
        let reconcile_telemetry = telemetry.clone();
        tokio::spawn(async move {
            let mut delay = Duration::from_secs(10);
            loop {
                match reconcile_pgbackrest_archive_config(&reconcile_config, &reconcile_telemetry)
                    .await
                {
                    Ok(()) => return,
                    Err(e) => {
                        warn!(
                            delay_secs = delay.as_secs(),
                            error = %e,
                            "DCS pgbackrest reconcile failed, retrying"
                        );
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_secs(120));
                    }
                }
            }
        });
    }

    // Auto-run pgbackrest stanza-create once Postgres is reachable. Without
    // this, the first WAL switch after enable would fail until a human
    // exec'd in and ran the command. Idempotent and safe to run from every
    // node — pgBackRest's stanza metadata is keyed on system_identifier,
    // which is identical across HA peers.
    spawn_bootstrap_stanza_create();

    // Closes the on_role_change frequency gap described on
    // spawn_collation_refresh — runs unconditionally, not just when
    // archiving is enabled.
    spawn_collation_refresh();

    // Spawn the leader-only backup watcher. Mirrors postgres-ssl
    // pgbackrest-backup-watcher.sh. Each iteration re-checks Patroni's
    // /leader API so replicas stay idle and a new leader takes over
    // within one poll cycle after failover. No-op when
    // WAL_ARCHIVE_BUCKET is unset.
    spawn_backup_watcher(config.data_dir.clone());

    // Spawn the replica-only self-heal watcher. Polls Patroni REST for
    // postmaster_start_time and POSTs /reinitialize when a replica is
    // crash-looping in a state Patroni's built-in recovery doesn't
    // catch (notably WAL-too-old after demoted-leader pg_rewind). No-op
    // on leaders. Honors SELF_HEAL_DISABLED=1 as a kill switch.
    spawn_self_heal_watcher(volume_root.clone(), telemetry.clone());

    // Spawn the slot-recovery watcher. On the leader it recreates replication
    // slots PostgreSQL invalidated when they breached max_slot_wal_keep_size
    // (Patroni 4.1.0 neither notices nor repairs these) — without that, the
    // disk-fill cap would trade a loud leader PANIC for a silently
    // un-streamable, un-promotable replica. On every node it keeps the cap
    // sized to the node's own live free space via ALTER SYSTEM (local param
    // outranks DCS, so DCS patches can't do it — see slot_recovery.rs). Honors
    // SLOT_RECOVERY_DISABLED=1 as a kill switch.
    spawn_slot_recovery_watcher(volume_root.clone());

    run_monitoring_loop(&config, child, &telemetry).await
}

#[cfg(test)]
mod tests {
    use super::{
        build_pgbackrest_conf, build_pgbackrest_recovery_source_conf, build_pitr_managed_block,
        configure_pitr_recovery, data_dir_nonempty, is_uuid_shape, parse_process_max,
        available_bytes, clear_clone_wipe_attempts, clone_wipe_ledger_path,
        pgdata_is_dedicated_subdir, read_clone_wipe_attempts, record_clone_wipe_attempt,
        should_wipe_incomplete_clone, strip_pitr_managed_block, wipe_has_safe_clone_source,
        wipe_pgdata_contents, Config, PgbackrestConfParams, MAX_CLONE_WIPE_ATTEMPTS,
        RecoverySourceConfParams, PITR_MANAGED_MARKER,
    };

    fn test_config(data_dir: &str) -> Config {
        Config {
            scope: "test-scope".into(),
            name: "test-node".into(),
            connect_address: "test-node".into(),
            etcd_hosts: "etcd-1:2379".into(),
            superuser: "postgres".into(),
            superuser_pass: "pw".into(),
            repl_user: "repl".into(),
            repl_pass: "pw".into(),
            app_user: "app".into(),
            app_pass: "pw".into(),
            app_db: "app".into(),
            data_dir: data_dir.into(),
            certs_dir: "/certs".into(),
            ttl: "30".into(),
            loop_wait: "10".into(),
            retry_timeout: "10".into(),
            health_check_interval: 5,
            health_check_timeout: 3,
            max_failures: 3,
            startup_grace_period: 30,
            max_startup_timeout: 1800,
            adopt_existing_data: false,
            wait_for_leader: false,
            synchronous_mode: false,
            wal_archive_bucket: None,
            wal_recover_from_bucket: None,
            pitr_target_time: None,
            pitr_target_xid: None,
            archive_timeout_secs: 60,
            basebackup_max_rate: "20M".into(),
            max_slot_wal_keep_size: "512000MB".into(),
        }
    }

    #[test]
    fn parse_process_max_uses_default_when_unset_or_invalid() {
        assert_eq!(parse_process_max(None, 4), 4);
        assert_eq!(parse_process_max(Some(""), 4), 4);
        assert_eq!(parse_process_max(Some("0"), 4), 4);
        assert_eq!(parse_process_max(Some("-1"), 4), 4);
        assert_eq!(parse_process_max(Some("not-a-number"), 4), 4);
    }

    #[test]
    fn parse_process_max_accepts_positive_override() {
        assert_eq!(parse_process_max(Some("16"), 4), 16);
        assert_eq!(parse_process_max(Some("1"), 4), 1);
    }

    #[test]
    fn archive_get_process_max_default_scales_with_cpus_not_flat_one() {
        // Regression: archive-get used to hard-default to 1 (WAL replay
        // assumed serial); it now sizes like archive-push,
        // clamp(cpus/8, 2, 8), because archive-async prefetch parallelizes
        // bulk catch-up. Exercise the same clamp() the real call site uses
        // across the cpu range operators actually see.
        for (cpus, expected_get_max) in [(1i64, 2u32), (8, 2), (16, 2), (64, 8), (256, 8)] {
            let conf = build_pgbackrest_conf(&PgbackrestConfParams {
                data_dir: "/pgdata",
                queue_max_mib: 5120,
                push_max: super::clamp(cpus / 8, 2, 8),
                get_max: super::clamp(cpus / 8, 2, 8),
                backup_max: 1,
                restore_max: 1,
                retention_full: 4,
                retention_diff: 14,
            });
            assert!(
                conf.contains(&format!("[global:archive-get]\nprocess-max={expected_get_max}\n")),
                "cpus={cpus}: expected archive-get process-max={expected_get_max} in:\n{conf}"
            );
        }
    }

    #[test]
    fn pgbackrest_conf_renders_all_five_process_max_sections() {
        let conf = build_pgbackrest_conf(&PgbackrestConfParams {
            data_dir: "/pgdata",
            queue_max_mib: 5120,
            push_max: 8,
            get_max: 4,
            backup_max: 2,
            restore_max: 32,
            retention_full: 4,
            retention_diff: 14,
        });
        assert!(conf.contains("[global:archive-push]\nprocess-max=8\n"));
        assert!(conf.contains("[global:archive-get]\nprocess-max=4\n"));
        assert!(conf.contains("[global:backup]\nprocess-max=2\n"));
        assert!(conf.contains("[global:restore]\nprocess-max=32\n"));
        assert!(conf.contains("repo1-retention-full=4\n"));
        assert!(conf.contains("repo1-retention-diff=14\n"));
    }

    #[test]
    fn recovery_source_conf_runs_archive_get_in_async_parallel_mode() {
        // Regression: --config REPLACES the main conf wholesale, so without
        // these three lines repeated here, staged PITR replay silently ran
        // sync archive-get at process-max=1 regardless of how the main conf
        // was tuned — exactly the bulk-catch-up workload parallel prefetch
        // targets.
        let conf = build_pgbackrest_recovery_source_conf(&RecoverySourceConfParams {
            data_dir: "/pgdata",
            bucket: "source-bucket",
            key: "key",
            secret: "secret",
            region: "us-east-1",
            endpoint: "fly.storage.tigris.dev",
            uri_style: "path",
            path: "/pgbackrest",
            get_max: 6,
        });
        assert!(conf.contains("archive-async=y\n"));
        assert!(conf.contains("archive-get-queue-max=1GiB\n"));
        assert!(conf.contains("[global:archive-get]\nprocess-max=6\n"));
        assert!(conf.contains("repo1-s3-bucket=source-bucket\n"));
        assert!(conf.contains("spool-path=/pgdata/pgbackrest-spool\n"));
    }

    #[test]
    fn configure_pitr_recovery_creates_spool_dir_with_restricted_perms() {
        let dir = std::env::temp_dir().join(format!("pitr_spool_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut config = test_config(dir.to_str().unwrap());
        config.wal_recover_from_bucket = Some("source-bucket".into());
        config.pitr_target_time = Some("2026-01-01T00:00:00Z".into());

        configure_pitr_recovery(&config).unwrap();

        let spool_dir = dir.join("pgbackrest-spool");
        assert!(spool_dir.is_dir(), "spool dir must exist after staging");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&spool_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o750, "spool dir must be 0750, got {mode:o}");
        }
        assert!(dir.join("recovery.signal").exists());
        assert!(dir.join(".pitr_staging").exists());
        let auto_conf = std::fs::read_to_string(dir.join("postgresql.auto.conf")).unwrap();
        assert!(auto_conf.contains("restore_command"));
        assert!(auto_conf.contains("recovery_target_time"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn managed_block_write_and_strip_are_symmetric() {
        let user_content = "shared_buffers = '128MB'\nwork_mem = '4MB'\n";
        let block = build_pitr_managed_block("recovery_target_time", "2026-01-01", "cmd");
        let combined = format!("{user_content}{block}");
        assert_eq!(strip_pitr_managed_block(&combined), user_content);
        // No marker → contents pass through untouched.
        assert_eq!(strip_pitr_managed_block(user_content), user_content);
        // Nothing but the block → empty file, not a stray blank line.
        assert_eq!(strip_pitr_managed_block(&block), "");
    }

    #[test]
    fn strip_removes_accumulated_duplicates_and_both_target_types() {
        // The raw-append bug left volumes in the wild with several copies,
        // possibly of DIFFERENT target types. One strip must clear them all —
        // a surviving stale recovery_target_time next to a fresh
        // recovery_target_xid makes Postgres refuse to start.
        let mut contents = String::from("max_connections = 100\n");
        contents.push_str(&build_pitr_managed_block(
            "recovery_target_time",
            "2026-01-01T00:00:00Z",
            "cmd",
        ));
        contents.push_str(&build_pitr_managed_block(
            "recovery_target_xid",
            "12345",
            "cmd",
        ));
        contents.push_str(&build_pitr_managed_block(
            "recovery_target_time",
            "2026-02-02T00:00:00Z",
            "cmd",
        ));

        let stripped = strip_pitr_managed_block(&contents);
        assert_eq!(stripped, "max_connections = 100\n");
    }

    #[test]
    fn strip_leaves_user_owned_recovery_settings_alone() {
        // Only lines FOLLOWING our marker are managed; a user's own
        // restore_command elsewhere in the file must survive.
        let contents = format!(
            "restore_command = 'cp /archive/%f %p'\n{}",
            build_pitr_managed_block("recovery_target_time", "2026-01-01", "cmd")
        );
        assert_eq!(
            strip_pitr_managed_block(&contents),
            "restore_command = 'cp /archive/%f %p'\n"
        );
    }

    #[test]
    fn restaging_replaces_the_managed_block_instead_of_accumulating() {
        let dir = std::env::temp_dir().join(format!("pitr_restage_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut config = test_config(dir.to_str().unwrap());
        config.wal_recover_from_bucket = Some("source-bucket".into());
        config.pitr_target_time = Some("2026-01-01T00:00:00Z".into());

        // First boot stages; the replay "fails" (recovery.signal persists),
        // and the retry boot comes back with the target CHANGED to xid —
        // the picker clamped it down to lastCommittedTxnAt.
        configure_pitr_recovery(&config).unwrap();
        config.pitr_target_xid = Some("987654".into());
        configure_pitr_recovery(&config).unwrap();

        let auto_conf = std::fs::read_to_string(dir.join("postgresql.auto.conf")).unwrap();
        assert_eq!(
            auto_conf.matches(PITR_MANAGED_MARKER).count(),
            1,
            "exactly one managed block must survive a re-stage:\n{auto_conf}"
        );
        assert!(auto_conf.contains("recovery_target_xid = '987654'"));
        assert!(
            !auto_conf.contains("recovery_target_time"),
            "the stale target type must be gone — postgres refuses multiple recovery targets:\n{auto_conf}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completed_replay_stamps_done_and_strips_the_managed_block() {
        let dir = std::env::temp_dir().join(format!("pitr_done_strip_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut config = test_config(dir.to_str().unwrap());
        config.wal_recover_from_bucket = Some("source-bucket".into());
        config.pitr_target_time = Some("2026-01-01T00:00:00Z".into());

        // Stage, then simulate a successful replay: Postgres consumes
        // recovery.signal on promote.
        configure_pitr_recovery(&config).unwrap();
        std::fs::remove_file(dir.join("recovery.signal")).unwrap();
        configure_pitr_recovery(&config).unwrap();

        assert!(dir.join(".pitr_configured").exists());
        assert!(!dir.join(".pitr_staging").exists());
        let auto_conf = std::fs::read_to_string(dir.join("postgresql.auto.conf")).unwrap();
        assert!(
            !auto_conf.contains(PITR_MANAGED_MARKER) && !auto_conf.contains("restore_command"),
            "no recovery residue may outlive a successful promote:\n{auto_conf}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configure_pitr_recovery_skips_staging_without_recover_from_bucket() {
        // Guard mirrored from postgres-ssl wrapper.sh: without
        // WAL_RECOVER_FROM_BUCKET the recovery-source conf never renders, so
        // staging recovery.signal here would archive-get FATAL at boot.
        let dir = std::env::temp_dir().join(format!("pitr_spool_skip_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut config = test_config(dir.to_str().unwrap());
        config.pitr_target_time = Some("2026-01-01T00:00:00Z".into());
        // wal_recover_from_bucket left None.

        configure_pitr_recovery(&config).unwrap();

        assert!(!dir.join("pgbackrest-spool").exists());
        assert!(!dir.join("recovery.signal").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wipe_only_with_pg_control_absent_nonempty_and_distinct_leader() {
        // The Comtrack case: no pg_control, non-empty dir, dedicated pgdata, a
        // different leader.
        assert!(should_wipe_incomplete_clone(
            false,
            true,
            true,
            Some("postgres-1"),
            "postgres-3"
        ));
        // pg_control present → valid (or foreign) dir, never our problem here.
        assert!(!should_wipe_incomplete_clone(
            true,
            true,
            true,
            Some("postgres-1"),
            "postgres-3"
        ));
        // Empty dir → nothing to wipe (fresh volume, Patroni will clone).
        assert!(!should_wipe_incomplete_clone(
            false,
            false,
            true,
            Some("postgres-1"),
            "postgres-3"
        ));
        // pgdata IS the volume root → wiping would nuke the bootstrap marker /
        // certs, so refuse even with a distinct leader.
        assert!(!should_wipe_incomplete_clone(
            false,
            true,
            false,
            Some("postgres-1"),
            "postgres-3"
        ));
        // No leader / etcd unreachable → no clone source, don't destroy the copy.
        assert!(!should_wipe_incomplete_clone(
            false,
            true,
            true,
            None,
            "postgres-3"
        ));
        // Leader is us (stale lock) → never wipe our own dir.
        assert!(!should_wipe_incomplete_clone(
            false,
            true,
            true,
            Some("postgres-3"),
            "postgres-3"
        ));
    }

    // The reseed wipe shares this predicate with the incomplete-clone wipe:
    // both destroy pgdata, so both demand a dedicated subdir AND a distinct
    // member holding the DCS leader lock — a live clone source.
    #[test]
    fn reseed_wipe_safety_requires_distinct_leader_and_dedicated_pgdata() {
        // Safe: dedicated pgdata, someone ELSE holds the leader lock.
        assert!(wipe_has_safe_clone_source(
            true,
            Some("postgres-1"),
            "postgres-3"
        ));
        // No leader / etcd unreachable → no clone source, never wipe.
        assert!(!wipe_has_safe_clone_source(true, None, "postgres-3"));
        // The lock names US (stale lock) → never wipe our own data.
        assert!(!wipe_has_safe_clone_source(
            true,
            Some("postgres-3"),
            "postgres-3"
        ));
        // pgdata IS the volume root → wiping would take out the sibling state
        // (bootstrap marker, sentinels, certs), so refuse even with a leader.
        assert!(!wipe_has_safe_clone_source(
            false,
            Some("postgres-1"),
            "postgres-3"
        ));
    }

    #[test]
    fn pgdata_dedicated_subdir_detection() {
        // Standard layout: pgdata is a subdir of the volume root.
        assert!(pgdata_is_dedicated_subdir(
            "/var/lib/postgresql/data/pgdata",
            "/var/lib/postgresql/data"
        ));
        // Non-standard: PGDATA points straight at the volume root.
        assert!(!pgdata_is_dedicated_subdir(
            "/var/lib/postgresql/data",
            "/var/lib/postgresql/data"
        ));
        // Trailing-slash insensitive.
        assert!(!pgdata_is_dedicated_subdir(
            "/var/lib/postgresql/data/",
            "/var/lib/postgresql/data"
        ));
    }

    #[test]
    fn data_dir_nonempty_and_wipe_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wipe_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("base/1")).unwrap();
        std::fs::write(dir.join("PG_VERSION"), b"17").unwrap();
        std::fs::write(dir.join("base/1/relfile"), vec![0u8; 16]).unwrap();
        let p = dir.to_str().unwrap();

        assert!(data_dir_nonempty(p));
        wipe_pgdata_contents(p).unwrap();
        // The mount point survives; only its contents are gone.
        assert!(dir.exists());
        assert!(!data_dir_nonempty(p));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn wipe_unlinks_symlinks_without_following_them() {
        // pg_tblspc / relocated pg_wal are symlinks out of pgdata; the wipe
        // must drop the link, never recurse into and delete the target.
        let base = std::env::temp_dir().join(format!("wipe_symlink_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let pgdata = base.join("pgdata");
        let external = base.join("external_tablespace");
        std::fs::create_dir_all(&pgdata).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("keepme"), b"data").unwrap();
        std::os::unix::fs::symlink(&external, pgdata.join("pg_tblspc_link")).unwrap();
        std::fs::write(pgdata.join("PG_VERSION"), b"17").unwrap();

        wipe_pgdata_contents(pgdata.to_str().unwrap()).unwrap();

        // pgdata emptied, but the symlink target and its file survive.
        assert!(!data_dir_nonempty(pgdata.to_str().unwrap()));
        assert!(external.join("keepme").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn data_dir_nonempty_false_for_missing_or_empty() {
        assert!(!data_dir_nonempty("/nonexistent/pgdata/xyz"));
        let dir = std::env::temp_dir().join(format!("empty_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!data_dir_nonempty(dir.to_str().unwrap()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_uuid_shape_accepts_canonical() {
        assert!(is_uuid_shape("121ccc45-0912-457e-8dc0-76625fe644bb"));
        assert!(is_uuid_shape("00000000-0000-0000-0000-000000000000"));
        assert!(is_uuid_shape("deadbeef-cafe-babe-c0de-feedfacef00d"));
    }

    #[test]
    fn is_uuid_shape_rejects_uppercase_hex() {
        // Bucket-id leaks are always lowercase. Reject uppercase so an
        // operator's intentionally-mixed-case bucket like
        // "Acme-2024-08" doesn't trip the validator just because of
        // its hex character set.
        assert!(!is_uuid_shape("121CCC45-0912-457E-8DC0-76625FE644BB"));
    }

    #[test]
    fn is_uuid_shape_rejects_wrong_length() {
        assert!(!is_uuid_shape(""));
        assert!(!is_uuid_shape("121ccc45-0912-457e-8dc0-76625fe644b")); // 35
        assert!(!is_uuid_shape("121ccc45-0912-457e-8dc0-76625fe644bbb")); // 37
    }

    #[test]
    fn is_uuid_shape_rejects_non_hex() {
        assert!(!is_uuid_shape("121ccc45-0912-457e-8dc0-76625fe644bg")); // g
        assert!(!is_uuid_shape("121ccc45-0912-457e-8dc0-76625fe644b!"));
    }

    #[test]
    fn is_uuid_shape_rejects_misplaced_dashes() {
        // All four dashes must be at positions 8, 13, 18, 23.
        assert!(!is_uuid_shape("121ccc450-912-457e-8dc0-76625fe644bb"));
        assert!(!is_uuid_shape("121ccc45-0912-457e-8dc076-625fe644bb"));
        assert!(!is_uuid_shape("121ccc4509124-57e-8dc0-76625fe644bb"));
    }

    #[test]
    fn is_uuid_shape_rejects_plausible_bucket_names() {
        // Real-world bucket names that the validator must NOT reject.
        assert!(!is_uuid_shape("pgbackrest"));
        assert!(!is_uuid_shape("railway-pgbackrest-prod"));
        assert!(!is_uuid_shape("my-bucket-with-dashes"));
        // Looks UUID-ish but isn't 8-4-4-4-12.
        assert!(!is_uuid_shape("121ccc45-0912-457e-8dc0"));
    }

    #[test]
    fn clone_wipe_ledger_counts_up_and_clears() {
        let vol = tempfile::tempdir().unwrap();
        let root = vol.path().to_str().unwrap();

        // No ledger yet: a first wipe must always be allowed.
        assert_eq!(read_clone_wipe_attempts(root), 0);

        record_clone_wipe_attempt(root, 1);
        assert_eq!(read_clone_wipe_attempts(root), 1);
        record_clone_wipe_attempt(root, 3);
        assert_eq!(read_clone_wipe_attempts(root), 3);
        assert!(read_clone_wipe_attempts(root) >= MAX_CLONE_WIPE_ATTEMPTS);

        clear_clone_wipe_attempts(root);
        assert_eq!(read_clone_wipe_attempts(root), 0);
        // Clearing an already-absent ledger must not panic or recreate it.
        clear_clone_wipe_attempts(root);
        assert!(!std::path::Path::new(&clone_wipe_ledger_path(root)).exists());
    }

    #[test]
    fn clone_wipe_ledger_lives_outside_pgdata() {
        // The wipe empties pgdata, so a ledger stored in there would reset itself
        // every pass and could never bound the loop.
        let ledger = clone_wipe_ledger_path("/var/lib/postgresql/data");
        assert_eq!(ledger, "/var/lib/postgresql/data/.railway_clone_wipe_attempts");
        assert!(!ledger.contains("/pgdata/"));
    }

    #[test]
    fn unreadable_ledger_reads_as_zero_so_it_never_blocks_a_first_recovery() {
        let vol = tempfile::tempdir().unwrap();
        let root = vol.path().to_str().unwrap();
        // A corrupt/garbage ledger must fail OPEN: the gate may only ever block a
        // destructive action, never be the reason a legitimate recovery is skipped.
        std::fs::write(clone_wipe_ledger_path(root), "not-a-number").unwrap();
        assert_eq!(read_clone_wipe_attempts(root), 0);
        std::fs::write(clone_wipe_ledger_path(root), "").unwrap();
        assert_eq!(read_clone_wipe_attempts(root), 0);
    }

    #[test]
    fn available_bytes_reads_a_real_filesystem_and_tolerates_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let free = available_bytes(dir.path().to_str().unwrap());
        assert!(free.is_some(), "statvfs should succeed on a real temp dir");
        assert!(free.unwrap() > 0);
        // Diagnosis only — a path we cannot stat must return None, never panic,
        // because it is reported alongside the cap, not used to gate it.
        assert!(available_bytes("/nonexistent-path-for-test").is_none());
    }

}
