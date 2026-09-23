//! WAL archiving (pgBackRest) setup shared by both boot modes of the image:
//! patroni-runner (Patroni HA) and postgres-wrapper's standalone branch.
//!
//! Everything here is driven by the tool-agnostic `WAL_ARCHIVE_*` env
//! contract: screen the bucket, translate it to pgBackRest-native
//! `PGBACKREST_REPO1_*`, size the WAL budgets against the volume, render
//! `/etc/pgbackrest/pgbackrest.conf`, and bootstrap the stanza once the
//! local Postgres is up and primary. None of it depends on Patroni.

use crate::pgbackrest::derive_pgbackrest_repo_path;
use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

/// The `archive_command` both boot modes install: the never-halt wrapper
/// around `pgbackrest archive-push` (see pgbackrest-archive-push-wrapper.sh).
pub const ARCHIVE_PUSH_COMMAND: &str = "/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p";

/// Default `archive_timeout` (seconds) when `POSTGRES_ARCHIVE_TIMEOUT` is
/// unset or not a positive integer.
pub const DEFAULT_ARCHIVE_TIMEOUT_SECS: i64 = 60;

/// Parse a `POSTGRES_ARCHIVE_TIMEOUT` value: a positive integer, otherwise
/// [`DEFAULT_ARCHIVE_TIMEOUT_SECS`].
pub fn parse_archive_timeout_secs(raw: Option<&str>) -> i64 {
    raw.and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_ARCHIVE_TIMEOUT_SECS)
}

/// True when `WAL_ARCHIVE_BUCKET` is set and non-empty in this process.
pub fn wal_archive_enabled() -> bool {
    env::var("WAL_ARCHIVE_BUCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .is_some()
}

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
pub fn translate_wal_env_to_pgbackrest() {
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
pub fn validate_wal_archive_bucket(volume_root: &str) {
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
pub fn detect_cpus() -> u32 {
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

pub fn clamp(v: i64, lo: i64, hi: i64) -> u32 {
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

pub fn env_or_clamp(var: &str, default: u32) -> u32 {
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
pub fn compute_volume_thresholds(volume_path: &str) -> (u32, u32) {
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

pub fn render_pgbackrest_conf(data_dir: &str, queue_max_mib: u32) -> Result<()> {
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
pub fn spawn_bootstrap_stanza_create() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_timeout_defaults_on_missing_or_non_positive_values() {
        assert_eq!(parse_archive_timeout_secs(None), 60);
        assert_eq!(parse_archive_timeout_secs(Some("")), 60);
        assert_eq!(parse_archive_timeout_secs(Some("0")), 60);
        assert_eq!(parse_archive_timeout_secs(Some("-5")), 60);
        assert_eq!(parse_archive_timeout_secs(Some("soon")), 60);
        assert_eq!(parse_archive_timeout_secs(Some("300")), 300);
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
                push_max: clamp(cpus / 8, 2, 8),
                get_max: clamp(cpus / 8, 2, 8),
                backup_max: 1,
                restore_max: 1,
                retention_full: 4,
                retention_diff: 14,
            });
            assert!(
                conf.contains(&format!(
                    "[global:archive-get]\nprocess-max={expected_get_max}\n"
                )),
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
}
