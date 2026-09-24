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
use postgres_patroni::bootstrap::{reconcile_pg_stat_statements, refresh_collation_versions};
use postgres_patroni::health_server::{self, HealthServerConfig};
use postgres_patroni::major_upgrade;
use postgres_patroni::patroni::etcd_preflight::{
    etcd_password_source_variable, probe_etcd_credential, rejection_message, EtcdAuthProbe,
    REJECTION_PREFIX,
};
use postgres_patroni::patroni::rest_preflight::{
    divergence_message, rest_credential_diverges, DIVERGENCE_PREFIX,
};
use postgres_patroni::patroni::{
    apply_credential_pin, credential_drift, credentials_from_env_requested,
    generate_patroni_config, reconcile_pgbackrest_archive_config, run_monitoring_loop,
    spawn_backup_watcher, spawn_self_heal_watcher, spawn_slot_recovery_watcher,
    update_pg_hba_for_replication, Config, RestapiAddressSource,
};
use postgres_patroni::pgbackrest::{derive_pgbackrest_repo_path, read_wal_level};
use postgres_patroni::wal_archive::{
    clamp, compute_volume_thresholds, detect_cpus, env_or_clamp, render_pgbackrest_conf,
    spawn_bootstrap_stanza_create, translate_wal_env_to_pgbackrest, validate_wal_archive_bucket,
};
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

#[derive(Serialize)]
struct EtcdAuthRequest<'a> {
    name: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct EtcdAuthResponse {
    #[serde(default)]
    token: Option<String>,
}

/// Token for etcd's HTTP gateway, or `None` when the cluster has not enabled
/// authentication (the request is then accepted without one). Any other
/// failure also yields `None`; the following request surfaces the real error.
async fn etcd_gateway_token(
    client: &reqwest::Client,
    host: &str,
    cred: Option<&postgres_patroni::patroni::Credential>,
) -> Option<String> {
    let cred = cred?;
    let url = format!("http://{}/v3/auth/authenticate", host.trim());
    let resp = client
        .post(&url)
        .json(&EtcdAuthRequest {
            name: &cred.username,
            password: &cred.password,
        })
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<EtcdAuthResponse>().await.ok()?.token
}

/// `POST /v3/kv/range` for one key on one etcd host, authenticating first when
/// a credential is configured.
async fn etcd_range(
    client: &reqwest::Client,
    host: &str,
    key_base64: &str,
    cred: Option<&postgres_patroni::patroni::Credential>,
) -> reqwest::Result<reqwest::Response> {
    let url = format!("http://{}/v3/kv/range", host.trim());
    let mut req = client.post(&url).json(&EtcdRangeRequest {
        key: key_base64.to_string(),
    });
    if let Some(token) = etcd_gateway_token(client, host, cred).await {
        req = req.header(reqwest::header::AUTHORIZATION, token);
    }
    req.send().await
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
            match etcd_range(&client, host, &key_base64, config.etcd_auth.as_ref()).await {
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
        let Ok(resp) = etcd_range(&client, host, &key_base64, config.etcd_auth.as_ref()).await
        else {
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
        context:
            "the leftover marker keeps the self-heal watcher standing down until it is removed"
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

/// The drifted-variable list as a telemetry field.
fn drifted_summary(drifted: &[&str]) -> String {
    if drifted.is_empty() {
        "none recorded".to_string()
    } else {
        drifted.join(", ")
    }
}

/// Patroni's own configuration loader gives `PATRONI_*` environment variables
/// priority over the config file, so it is not enough to render the pinned
/// credentials into patroni.yml: Patroni would read the drifted
/// `PATRONI_REPLICATION_PASSWORD` / `PATRONI_SUPERUSER_PASSWORD` straight out
/// of the environment, override the file, and write the drifted replication
/// password into its pgpass — which is what `primary_conninfo` authenticates
/// with. The replicas then fail to authenticate against a leader whose roles
/// still carry the pinned password, which is exactly the outage the pin
/// exists to prevent.
///
/// Hand Patroni the same values the config file already carries, so the two
/// sources agree whatever the variables say. `config` here is post-pin, so on
/// a fresh volume these are the variables themselves and this is a no-op.
///
/// The control-plane credential variables get the same treatment for the
/// opposite reason: the runner reads a blank (whitespace-only) value as unset
/// and renders patroni.yml accordingly, while Patroni's loader keeps any
/// non-empty string (`_get_auth`: `if value:`) and applies it over the file.
/// A blank `PATRONI_RESTAPI_PASSWORD` then reaches Patroni as
/// `restapi.authentication = {password: "  "}` with no username, and
/// `'{username}:{password}'.format(...)` raises KeyError before the API
/// starts; a blank `PATRONI_ETCD3_PASSWORD` replaces the etcd password the
/// file carries with spaces. Blank variables are removed from the child's
/// environment so both sides agree they are unset.
async fn start_patroni(config: &Config) -> Result<tokio::process::Child> {
    let mut command = Command::new("patroni");
    command
        .arg("/etc/patroni/patroni.yml")
        .env("PATRONI_REPLICATION_PASSWORD", &config.repl_pass)
        .env("PATRONI_SUPERUSER_PASSWORD", &config.superuser_pass);
    for var in blank_credential_vars(|name| env::var(name).ok()) {
        warn!(
            variable = var,
            "credential variable is blank; treated as unset and not passed to Patroni"
        );
        command.env_remove(var);
    }
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to start patroni")?;

    Ok(child)
}

/// Control-plane credential variables the runner treats as unset when blank
/// (see `patroni::config::resolve_restapi_auth` / `resolve_etcd_auth`) and
/// Patroni would treat as set.
const BLANK_IS_UNSET_CREDENTIAL_VARS: [&str; 4] = [
    "PATRONI_RESTAPI_USERNAME",
    "PATRONI_RESTAPI_PASSWORD",
    "PATRONI_ETCD3_USERNAME",
    "PATRONI_ETCD3_PASSWORD",
];

/// The credential variables whose value under `lookup` is blank, i.e. the ones
/// to withhold from Patroni's environment. Unset variables are not listed.
fn blank_credential_vars(lookup: impl Fn(&str) -> Option<String>) -> Vec<&'static str> {
    BLANK_IS_UNSET_CREDENTIAL_VARS
        .iter()
        .copied()
        .filter(|name| lookup(name).is_some_and(|value| value.trim().is_empty()))
        .collect()
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
    for sig in [
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGHUP,
    ] {
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
        if wait_until_local_primary(Duration::from_secs(600)).await {
            refresh_collation_versions();
        }
    });
}

/// Wait until the local postgres both answers connections AND reports
/// primary (`pg_is_in_recovery() = false`), or until `budget` elapses.
/// Returns false on timeout.
///
/// pg_isready alone accepts connections from a replica in hot_standby mode
/// too — the second loop waits for primary specifically, since the callers
/// run DDL (ALTER DATABASE / ALTER EXTENSION) that fails with a
/// read-only-transaction error on a standby. A permanent replica never
/// passes it and just times out at the deadline; it gets the callers' fixes
/// through WAL replication once the primary runs them, not by running them
/// itself.
async fn wait_until_local_primary(budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return false;
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

    loop {
        if tokio::time::Instant::now() >= deadline {
            return false;
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
            return true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Boot-time pg_stat_statements reconcile (see bootstrap::extensions for the
/// full rationale): pg_upgrade preserves the extension's SQL-level version
/// and nothing else ever updates it, so upgraded clusters drift behind what
/// the image ships while the dashboard's Stats tab queries one view shape
/// fleet-wide. Primary-only, same gating as spawn_collation_refresh; safe to
/// double-run with the on_role_change-triggered reconcile — it no-ops once
/// versions match.
fn spawn_extension_reconcile() {
    tokio::spawn(async move {
        if wait_until_local_primary(Duration::from_secs(600)).await {
            reconcile_pg_stat_statements();
        }
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
    // A degraded outcome keeps the boot going (see volume_lock) but must not
    // pass silently: without the lock this container has no overlap protection
    // at all, on a volume whose PGDATA a second postmaster may already be
    // writing to. Report it the way redis-ha and mysql-ha report theirs.
    let _runtime_volume_lock = match postgres_patroni::volume_lock::acquire_volume_runtime_lock(
        &volume_root,
        &postgres_patroni::pgdata(),
    )? {
        postgres_patroni::volume_lock::VolumeLockOutcome::Held(lock) => Some(lock),
        postgres_patroni::volume_lock::VolumeLockOutcome::SkippedForFirstInit => None,
        postgres_patroni::volume_lock::VolumeLockOutcome::FailedOpen { reason } => {
            let error = format!(
                "{reason}; booting WITHOUT the volume lock — if a previous container is still \
                 alive on this volume, two postmasters may now touch the same PGDATA. This also \
                 drops the interlock shared with the standalone postgres-ssl image, so a \
                 standalone<->HA conversion overlapping on this volume is unserialized."
            );
            error!("{error}");
            telemetry.send(TelemetryEvent::ComponentError {
                component: "volume-lock".to_string(),
                error,
                context: "startup".to_string(),
            });
            None
        }
    };

    // Shared, container-lifetime flock on the SAME lock file upgrade-job.sh
    // takes exclusively for its own run (see major_upgrade::
    // take_volume_upgrade_lock). Bound to `_upgrade_volume_lock` so it lives
    // for the rest of this function — i.e. for as long as this process is
    // running Patroni/Postgres against the volume — and releases automatically
    // on exit either way. Checked before the marker/version guards below: if
    // a job holds the volume right now, that refusal is more informative than
    // whatever the marker happens to say mid-job.
    let _upgrade_volume_lock =
        match major_upgrade::take_volume_upgrade_lock(&volume_root, &postgres_patroni::pgdata()) {
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
        rest_address = %config.restapi_connect_address,
        rest_address_source = ?config.restapi_address_source,
        "=== Patroni Runner ==="
    );
    if config.restapi_address_source == RestapiAddressSource::PrivateDomain {
        telemetry.send(TelemetryEvent::ComponentError {
            component: "patroni-runner".to_string(),
            error: format!(
                "no usable Railway container IPv6 on the interface; restapi.connect_address is {}",
                config.restapi_connect_address
            ),
            context: "other members resolve the private domain through Patroni's 600 s cache: switchover to this node can answer 412 for up to ten minutes after it redeploys"
                .to_string(),
        });
    }

    let bootstrap_marker = format!("{}/.patroni_bootstrap_complete", volume_root);

    // Credential pre-flights, ahead of the reseed handling below and of
    // anything else that touches the volume: a reseed boot probes etcd for the
    // leader before it wipes, and with a refused credential it would stop on
    // "not safe to wipe" instead of on the message that names the edited
    // variable. The drift list is a read of the credential pin (which the
    // reseed wipe removes along with the rest of pgdata); the pin itself is
    // applied further down, once the post-reseed state of the data directory
    // is known, and reports the same list.
    let credential_drift = credential_drift(&config, credentials_from_env_requested());

    // etcd's root password is fixed when the etcd entrypoint first enables
    // authentication; the credential this member presents is re-derived from
    // its variables on every boot. Ask etcd before Patroni does, so a member
    // whose password variable was edited after the cluster was created stops
    // with a message that names the variable and the fix, instead of Patroni's
    // bare "Etcd3 authentication failed". Only an explicit rejection stops the
    // boot: an etcd without authentication, or one that is unreachable right
    // now, is left to Patroni's own retry loop (see patroni::etcd_preflight).
    if let Some(cred) = config.etcd_auth.as_ref() {
        let probe = probe_etcd_credential(&config.etcd_hosts, cred, Duration::from_secs(3)).await;
        if probe == EtcdAuthProbe::Rejected {
            let message = rejection_message(
                &cred.username,
                etcd_password_source_variable(env::var("PATRONI_ETCD3_PASSWORD").ok().as_deref()),
                &credential_drift,
            );
            error!("{message}");
            telemetry.send(TelemetryEvent::ComponentError {
                component: "patroni-runner".to_string(),
                error: format!(
                    "{REJECTION_PREFIX}; drifted variables: {}",
                    drifted_summary(&credential_drift)
                ),
                context: "etcd credential pre-flight".to_string(),
            });
            anyhow::bail!("{REJECTION_PREFIX}; see the message above for the variable to restore");
        }
    }

    // The same edit seen from the REST API, for the cluster where etcd does
    // not check the password (authentication not enabled, or a dedicated
    // PATRONI_ETCD3_PASSWORD): the member would enforce and present the edited
    // REST password while its peers hold the original, and every call between
    // them — the failsafe pings included — would be refused both ways. Decided
    // from the pin's drift list and the variables alone (see
    // patroni::rest_preflight). `config` is pre-pin here, so its superuser
    // password is the variable's value.
    let restapi_password = config
        .restapi_auth
        .as_ref()
        .filter(|_| config.restapi_auth_enforced)
        .map(|cred| cred.password.as_str());
    if rest_credential_diverges(&credential_drift, restapi_password, &config.superuser_pass) {
        let message = divergence_message(&credential_drift);
        error!("{message}");
        telemetry.send(TelemetryEvent::ComponentError {
            component: "patroni-runner".to_string(),
            error: format!(
                "{DIVERGENCE_PREFIX}; drifted variables: {}",
                drifted_summary(&credential_drift)
            ),
            context: "REST credential pre-flight".to_string(),
        });
        anyhow::bail!("{DIVERGENCE_PREFIX}; see the message above for the variable to restore");
    }

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
    let child = start_patroni(&config).await?;

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

    // Same boot trigger for the pg_stat_statements version reconcile — and
    // the same on_role_change frequency-gap coverage on promotion.
    spawn_extension_reconcile();

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
        available_bytes, blank_credential_vars, build_pgbackrest_recovery_source_conf,
        build_pitr_managed_block, clear_clone_wipe_attempts, clone_wipe_ledger_path,
        configure_pitr_recovery, data_dir_nonempty, pgdata_is_dedicated_subdir,
        read_clone_wipe_attempts, record_clone_wipe_attempt, should_wipe_incomplete_clone,
        strip_pitr_managed_block, wipe_has_safe_clone_source, wipe_pgdata_contents, Config,
        RecoverySourceConfParams, RestapiAddressSource, MAX_CLONE_WIPE_ATTEMPTS,
        PITR_MANAGED_MARKER,
    };

    #[test]
    fn blank_credential_variables_are_withheld_from_patroni_set_ones_are_not() {
        let env = |name: &str| match name {
            "PATRONI_RESTAPI_PASSWORD" => Some("  ".to_string()),
            "PATRONI_ETCD3_PASSWORD" => Some(String::new()),
            "PATRONI_RESTAPI_USERNAME" => Some("ops".to_string()),
            // PATRONI_ETCD3_USERNAME unset
            _ => None,
        };
        assert_eq!(
            blank_credential_vars(env),
            vec!["PATRONI_RESTAPI_PASSWORD", "PATRONI_ETCD3_PASSWORD"]
        );
    }

    #[test]
    fn a_real_password_with_inner_whitespace_is_not_blank() {
        let env = |name: &str| (name == "PATRONI_RESTAPI_PASSWORD").then(|| " a b ".to_string());
        assert!(blank_credential_vars(env).is_empty());
        assert!(blank_credential_vars(|_| None).is_empty());
    }

    fn test_config(data_dir: &str) -> Config {
        Config {
            scope: "test-scope".into(),
            name: "test-node".into(),
            connect_address: "test-node".into(),
            restapi_connect_address: "test-node:8008".into(),
            restapi_address_source: RestapiAddressSource::PrivateDomain,
            etcd_hosts: "etcd-1:2379".into(),
            etcd_auth: None,
            restapi_auth: None,
            restapi_auth_enforced: false,
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
            failsafe_mode: true,
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
        assert_eq!(
            ledger,
            "/var/lib/postgresql/data/.railway_clone_wipe_attempts"
        );
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
