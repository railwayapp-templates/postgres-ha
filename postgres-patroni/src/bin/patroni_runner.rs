//! Patroni runner - Wrapper to run Patroni with proper setup
//!
//! Generates Patroni configuration and starts Patroni.
//! Runs as PID 1 in container with built-in health monitoring.

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use common::init_logging;
use nix::sys::stat::{umask, Mode};
use postgres_patroni::health_server::{self, HealthServerConfig};
use postgres_patroni::patroni::{
    generate_patroni_config, reconcile_pgbackrest_archive_config, run_monitoring_loop,
    spawn_backup_watcher, update_pg_hba_for_replication, Config,
};
use postgres_patroni::pgbackrest::derive_pgbackrest_repo_path;
use postgres_patroni::{volume_root, Telemetry};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

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

fn env_or_clamp(var: &str, default: u32) -> u32 {
    env::var(var)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(default)
}

/// pg_wal drop ceiling (MiB) and pgBackRest archive-push spool ceiling (MiB),
/// both scaled DOWN from absolute defaults (500 / 5120) on small volumes —
/// never up. Hobby's 1 GiB volume can't carry a 5 GiB spool, and 500 MiB of
/// pg_wal is half that disk; on volumes ≥25 GiB the absolutes hold.
///
/// Ratios: wal-drop ~10% of volume (hard-failure pg_wal hostage), queue-max
/// ~50% of volume (transient-stall spool absorption). The ~10× spread between
/// the two budgets is preserved across all volume sizes — hard failures still
/// bail fast, transient stalls still absorb generously.
///
/// Floors: 64 MiB wal-drop (~4 WAL segments — one short stall), 128 MiB
/// queue-max (~8 segments). Below these archiving is effectively off and the
/// dashboard surfaces it via pg_stat_archiver.
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
        info!("pgbackrest: volume size unknown; using absolute thresholds wal-drop=500 MiB queue-max=5 GiB");
        return (500, 5 * 1024);
    }

    let wal_drop = (total_mib / 10).clamp(64, 500);
    let queue_max = (total_mib / 2).clamp(128, 5 * 1024);

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
/// `compute_volume_thresholds` (≤500 MiB pg_wal, ≤5 GiB spool on volumes
/// ≥25 GiB; scaled down proportionally below that).
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
/// serial WAL arrival + S3 PUT overhead; archive-get by serial replay
/// inside Postgres; backup leaves CPU for live DB; restore is unbounded
/// (DB is down) up to pgBackRest's plateau around 32 workers.
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
    let spool_dir = format!("{data_dir}/pgbackrest-spool");

    let cpus = detect_cpus().max(1) as i64;
    let push_max = env_or_clamp("PGBACKREST_ARCHIVE_PUSH_PROCESS_MAX", clamp(cpus / 8, 2, 8));
    let get_max = env_or_clamp("PGBACKREST_ARCHIVE_GET_PROCESS_MAX", 1);
    let backup_max = env_or_clamp("PGBACKREST_BACKUP_PROCESS_MAX", clamp(cpus / 4, 1, 16));
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

    let conf = format!(
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
    );

    fs::create_dir_all("/etc/pgbackrest").context("Failed to create /etc/pgbackrest")?;
    fs::write(conf_path, conf).context("Failed to write pgbackrest.conf")?;
    fs::set_permissions(conf_path, std::fs::Permissions::from_mode(0o640))
        .context("Failed to set pgbackrest.conf permissions")?;

    info!("pgbackrest: rendered {}", conf_path);
    Ok(())
}

/// Render `/etc/pgbackrest/pgbackrest-recovery-source.conf` when
/// `WAL_RECOVER_FROM_*` is set. This conf is used exclusively during
/// recovery: explicit `pgbackrest restore` (empty-volume restore in
/// future) and `restore_command` for archive-get. Has only the source
/// bucket as repo1 (numbering is per-config) so post-promote
/// archive-push from the fork's main pgbackrest.conf can never fan out
/// to source's read-only bucket and 403. Mirrors postgres-ssl PR #49.
fn render_pgbackrest_recovery_source_conf(data_dir: &str) -> Result<()> {
    let bucket = match env::var("WAL_RECOVER_FROM_BUCKET") {
        Ok(b) if !b.is_empty() => b,
        _ => return Ok(()),
    };
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

    let spool_dir = format!("{data_dir}/pgbackrest-spool");
    let conf = format!(
        "[global]\n\
         log-level-console=info\n\
         log-level-file=off\n\
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
         [main]\n\
         pg1-path={data_dir}\n\
         pg1-port=5432\n",
    );

    fs::create_dir_all("/etc/pgbackrest").context("Failed to create /etc/pgbackrest")?;
    let conf_path = "/etc/pgbackrest/pgbackrest-recovery-source.conf";
    fs::write(conf_path, conf).context("Failed to write pgbackrest-recovery-source.conf")?;
    fs::set_permissions(conf_path, std::fs::Permissions::from_mode(0o640))
        .context("Failed to set pgbackrest-recovery-source.conf permissions")?;
    info!("pgbackrest: rendered {}", conf_path);
    Ok(())
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
/// env vars and restart, and the next boot will re-stage cleanly.
///
/// Source-path divergence detection is gone: under the new-service restore
/// design, the restored cluster has its own bucket (`WAL_ARCHIVE_*`) and
/// reads from the source's bucket via the distinct `WAL_RECOVER_FROM_*`
/// repo, so no shared write path exists to corrupt.
fn configure_pitr_recovery(config: &Config) -> Result<()> {
    use std::io::Write;

    let data_dir = &config.data_dir;
    let staging = format!("{data_dir}/.pitr_staging");
    let done = format!("{data_dir}/.pitr_configured");
    let signal = format!("{data_dir}/recovery.signal");
    let pg_version = format!("{data_dir}/PG_VERSION");
    let restored_marker = format!("{data_dir}/.pgbackrest_restored");

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

    let auto_conf_path = format!("{data_dir}/postgresql.auto.conf");
    let addition = format!(
        "\n# managed by pgbackrest-recovery (patroni-runner)\n\
         restore_command = '{escaped_restore}'\n\
         {target_param} = '{escaped_target}'\n\
         recovery_target_action = 'promote'\n",
    );
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&auto_conf_path)
        .context("Failed to open postgresql.auto.conf")?;
    f.write_all(addition.as_bytes())
        .context("Failed to append recovery settings")?;

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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            if tokio::time::Instant::now() >= deadline {
                warn!("pgbackrest: timed out waiting for Postgres before stanza-create");
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
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_logging("patroni-runner");

    // Translate the WAL_* env contract into pgBackRest-native PGBACKREST_*
    // before anything reads either set. Done first so Config::from_env() and
    // every downstream invocation (patroni archive_command, the wrapper
    // script, stanza-create) see the same translated env.
    translate_wal_env_to_pgbackrest();

    // Capture health server config BEFORE clearing PG* env vars
    let health_config = HealthServerConfig::from_env();

    let telemetry = Telemetry::from_env("postgres-ha");
    let config = Config::from_env()?;

    info!(
        node = %config.name,
        address = %config.connect_address,
        "=== Patroni Runner ==="
    );

    let volume_root = volume_root();
    let bootstrap_marker = format!("{}/.patroni_bootstrap_complete", volume_root);

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

    // Prevent race condition during HA conversion:
    // When PATRONI_WAIT_FOR_LEADER=true, this replica waits for the primary to
    // establish leadership before starting. This prevents empty replicas from
    // winning the election and causing data loss during conversion.
    // Only used during conversion when postgres-1 has existing data to preserve.
    if config.wait_for_leader && !has_pg_control {
        wait_for_cluster_in_etcd(&config).await?;
    }

    // Generate and write Patroni config
    let patroni_config = generate_patroni_config(&config);
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
    // mounted volume. Both scale DOWN from absolute defaults (500 MiB /
    // 5 GiB) on small volumes — never up. WAL_DROP_THRESHOLD_MB is exported
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

    // Start health server for HAProxy health checks
    // This runs independently and queries PostgreSQL directly for primary/replica status
    let _health_handle = health_server::start(health_config).await?;

    // Start Patroni and run monitoring loop
    let child = start_patroni().await?;

    // Spawn a one-shot DCS reconcile task. This waits for Patroni's REST API
    // to come up, then PATCHes /config so DCS archive params match env-var
    // intent. Required because `bootstrap.dcs` only seeds DCS at first
    // cluster init; without this, env-var changes on existing clusters are
    // silently ignored by Patroni. Failure is logged but does not abort
    // patroni-runner — the monitoring loop must still run.
    {
        let reconcile_config = Config::from_env()?;
        tokio::spawn(async move {
            match reconcile_pgbackrest_archive_config(&reconcile_config).await {
                Ok(()) => {}
                Err(e) => warn!(error = %e, "DCS pgbackrest reconcile failed"),
            }
        });
    }

    // Auto-run pgbackrest stanza-create once Postgres is reachable. Without
    // this, the first WAL switch after enable would fail until a human
    // exec'd in and ran the command. Idempotent and safe to run from every
    // node — pgBackRest's stanza metadata is keyed on system_identifier,
    // which is identical across HA peers.
    spawn_bootstrap_stanza_create();

    // Spawn the leader-only backup watcher. Mirrors postgres-ssl
    // pgbackrest-backup-watcher.sh. Each iteration re-checks Patroni's
    // /leader API so replicas stay idle and a new leader takes over
    // within one poll cycle after failover. No-op when
    // WAL_ARCHIVE_BUCKET is unset.
    spawn_backup_watcher(config.data_dir.clone());

    run_monitoring_loop(&config, child, &telemetry).await
}
