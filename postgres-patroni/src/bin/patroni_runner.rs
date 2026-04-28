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
    update_pg_hba_for_replication, Config,
};
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
/// env vars, so they don't need to be in the conf file.
///
/// The `archive-async=y` + `archive-push-queue-max=5GiB` combination is what
/// keeps Postgres alive under sustained S3 stalls — when the queue trips,
/// pgBackRest drops WAL and reports success to Postgres rather than letting
/// `pg_wal` fill the data volume.
///
/// Always rendered, regardless of whether archiving is currently enabled.
/// Reasoning: during a disable transition, DCS may still hold
/// `archive_mode=on` until the reconcile task patches it out and Postgres
/// restarts. If the conf file were missing, `archive_command` would fail
/// synchronously on every WAL switch and `pg_wal` would grow unbounded.
/// With the conf present, pgbackrest enqueues to the spool, the async
/// daemon fails the S3 push (no creds), and `archive-push-queue-max`
/// trips at 5 GiB — DB stays up at the cost of a truncated PITR window.
fn render_pgbackrest_conf(data_dir: &str) -> Result<()> {
    let conf_path = "/etc/pgbackrest/pgbackrest.conf";

    // repo1-retention-* is intentionally omitted: this image never runs
    // `pgbackrest backup`/`expire`, so those knobs would be no-ops anyway.
    // WAL retention is enforced server-side by the bucket's lifecycle policy.
    let process_max = env::var("PGBACKREST_PROCESS_MAX")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "2".to_string());

    let conf = format!(
        "[global]\n\
         repo1-type=s3\n\
         log-level-console=info\n\
         log-level-file=off\n\
         archive-async=y\n\
         archive-push-queue-max=5GiB\n\
         archive-get-queue-max=1GiB\n\
         spool-path=/var/lib/postgresql/pgbackrest-spool\n\
         process-max={process_max}\n\
         compress-type=zst\n\
         compress-level=3\n\
         start-fast=y\n\
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

/// Path of the sentinel that records which `repo1-path` this volume's WAL has
/// been pushed to. Recovery refuses to stage if the current
/// `PGBACKREST_REPO1_PATH` still equals this — the operator must change it so
/// post-promote `archive-push` lands in a different prefix and can't corrupt
/// the source's ongoing WAL chain.
fn source_path_sentinel(data_dir: &str) -> String {
    format!("{}/.pgbackrest_source_path", data_dir)
}

/// Refresh the sentinel to track the currently-configured `repo1-path`. Runs
/// on every boot when archiving is enabled so the recorded value matches the
/// path Postgres is actually pushing to right now.
fn stamp_source_repo_path(data_dir: &str, current_path: &str) -> Result<()> {
    let sentinel = source_path_sentinel(data_dir);
    if let Ok(existing) = fs::read_to_string(&sentinel) {
        if existing == current_path {
            return Ok(());
        }
    }
    fs::write(&sentinel, current_path).context("Failed to write source-path sentinel")?;
    info!(path = %current_path, "pgbackrest: stamped source repo path");
    Ok(())
}

/// Stage PITR replay before Patroni starts Postgres.
///
/// When `POSTGRES_RECOVERY_TARGET_TIME` is set and we haven't already staged
/// recovery on this volume (sentinel `$PGDATA/.pitr_configured`), this writes
/// `recovery.signal` + recovery settings to postgresql.auto.conf so Postgres
/// enters archive recovery on boot, replays WAL from the pgBackRest repo to
/// the target timestamp, then promotes.
///
/// Postgres removes `recovery.signal` automatically on successful promote.
/// The sentinel blocks re-trigger on subsequent restarts — repeat PITR is
/// expected to run on a fresh volume, not rerun on already-recovered data.
///
/// Repo-path divergence is enforced two ways:
///   1. Read path: `PGBACKREST_RECOVERY_REPO1_PATH` names where `archive-get`
///      pulls WAL from during replay (the source's path). Baked into
///      `restore_command` via `--repo1-path=...`.
///   2. Write path: `PGBACKREST_REPO1_PATH` must NOT equal the stamped source
///      path. After promote, `archive_command` pushes to that path; if it's
///      still the source's, the new timeline corrupts the source's ongoing
///      WAL chain. Refusing here surfaces the misconfig before Patroni starts.
fn configure_pitr_recovery(config: &Config, target_time: &str) -> Result<()> {
    use std::io::Write;

    let data_dir = &config.data_dir;
    let marker_path = format!("{}/.pitr_configured", data_dir);
    if Path::new(&marker_path).exists() {
        info!(
            target = %target_time,
            "PITR recovery already staged on this volume — skipping",
        );
        return Ok(());
    }

    // Refuse to stage when the post-promote write path still matches the
    // source's stamped path — recovered timeline would otherwise overwrite
    // the source's WAL prefix.
    let sentinel = source_path_sentinel(data_dir);
    if let Ok(stamped) = fs::read_to_string(&sentinel) {
        let current_write = config.pgbackrest_repo1_path.as_deref().unwrap_or("");
        if stamped == current_write {
            anyhow::bail!(
                "pgbackrest: REFUSING to stage PITR — PGBACKREST_REPO1_PATH ('{}') matches the source's stamped repo path. \
After promote, archive_command would push the recovered timeline back into the source's repo and corrupt its WAL chain. \
Set PGBACKREST_REPO1_PATH to a NEW prefix for the recovered cluster's writes, and PGBACKREST_RECOVERY_REPO1_PATH='{}' so archive-get can still read source WAL during replay.",
                current_write,
                stamped,
            );
        }
    }

    let restore_cmd = match config.pgbackrest_recovery_repo1_path.as_deref() {
        Some(p) => format!(
            "pgbackrest --stanza=main --repo1-path={} archive-get %f %p",
            p,
        ),
        None => {
            warn!(
                "PGBACKREST_RECOVERY_REPO1_PATH unset; archive-get will use \
                 PGBACKREST_REPO1_PATH. Set the recovery-read path explicitly \
                 to avoid coupling read and write paths."
            );
            "pgbackrest --stanza=main archive-get %f %p".to_string()
        }
    };

    // Write the marker before mutating Postgres state so a crash mid-setup
    // doesn't leave us in a loop re-triggering replay on the next boot.
    fs::write(&marker_path, "").context("Failed to create PITR marker")?;

    let signal_path = format!("{}/recovery.signal", data_dir);
    fs::File::create(&signal_path).context("Failed to create recovery.signal")?;

    let auto_conf_path = format!("{}/postgresql.auto.conf", data_dir);
    let addition = format!(
        "\n# managed by pgbackrest-recovery (patroni-runner)\n\
         restore_command = '{}'\n\
         recovery_target_time = '{}'\n\
         recovery_target_action = 'promote'\n",
        restore_cmd, target_time,
    );
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&auto_conf_path)
        .context("Failed to open postgresql.auto.conf")?;
    f.write_all(addition.as_bytes())
        .context("Failed to append recovery settings")?;

    info!(target = %target_time, "pgbackrest PITR replay staged");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_logging("patroni-runner");

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
    fs::write("/etc/patroni/patroni.yml", &patroni_config).context("Failed to write patroni.yml")?;

    info!(
        scope = %config.scope,
        etcd = %config.etcd_hosts,
        "Starting Patroni"
    );

    // Prepare data directory
    fs::create_dir_all(&config.data_dir).context("Failed to create data directory")?;
    fs::set_permissions(&config.data_dir, std::fs::Permissions::from_mode(0o700))
        .context("Failed to set data directory permissions")?;

    // Render /etc/pgbackrest/pgbackrest.conf unconditionally. The conf is
    // operator policy (queue-max, async, spool-path, stanza definition) and
    // is always safe to have in place: pgbackrest only does anything when
    // Postgres invokes archive_command, and that is gated by archive_mode in
    // DCS, not by the presence of this file. Keeping it always-present means
    // archive_command can never fail synchronously due to a missing conf —
    // worst case, queue-max trips and Postgres keeps running.
    render_pgbackrest_conf(&config.data_dir)?;

    // Stage pgBackRest PITR replay if requested. No-op unless
    // POSTGRES_RECOVERY_TARGET_TIME is set. Must run before Patroni starts
    // Postgres so the signal file and recovery settings are in place. Reads
    // the source-path sentinel before stamp_source_repo_path overwrites it,
    // so the path-divergence check sees the original source value.
    if let Some(target_time) = &config.pitr_target_time {
        configure_pitr_recovery(&config, target_time)?;
    }

    // Track the post-recovery (or steady-state) write path on disk so a
    // future PITR-restored snapshot of this volume can detect whether the
    // operator pivoted PGBACKREST_REPO1_PATH before staging recovery.
    if config.pgbackrest_s3_bucket.is_some() {
        let current_write = config.pgbackrest_repo1_path.as_deref().unwrap_or("");
        stamp_source_repo_path(&config.data_dir, current_write)?;
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

    run_monitoring_loop(&config, child, &telemetry).await
}
