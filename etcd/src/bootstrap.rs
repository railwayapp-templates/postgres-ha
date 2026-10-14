//! Bootstrap logic for etcd cluster
//!
//! Handles leader election, recovery detection, and cluster initialization.

use crate::cluster::{
    add_as_learner, add_self_to_cluster, check_cluster_health, clear_directory, find_self,
    get_member_list, get_voting_member_endpoint, has_local_data, is_name_collision,
    member_sharing_my_name, promote_self, refuse_name_collision, remove_stale_self,
    shared_name_message, PromoteOutcome, SelfIdentity,
};
use crate::config::{get_leader_endpoint, parse_initial_cluster, peer_to_client_url, Config};
use anyhow::{anyhow, Result};
use common::{etcd_http_health, etcd_http_user_add, etcdctl, Telemetry, TelemetryEvent};
use std::io::ErrorKind;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::time::sleep;
use tracing::{info, warn};

/// Only a definite NotFound is "no marker". `Path::exists()` returns false
/// on EACCES/EIO, which would treat a member we cannot inspect as
/// incomplete bootstrap and wipe it. Any other metadata error is present.
pub(crate) fn bootstrap_marker_present(path: &str) -> bool {
    match std::fs::metadata(path) {
        Ok(_) => true,
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => {
            warn!(
                error = %e,
                path,
                "could not stat bootstrap marker; treating as present"
            );
            true
        }
    }
}

/// Check if any other peer has a healthy cluster (for recovery detection)
pub async fn check_existing_cluster(
    initial_cluster: &str,
    my_name: &str,
) -> Result<Option<String>> {
    info!("Checking for existing cluster on other peers...");

    let cluster = parse_initial_cluster(initial_cluster)?;
    for (name, peer_url) in cluster.iter() {
        if name == my_name {
            continue;
        }

        let client_endpoint = peer_to_client_url(peer_url);
        info!(peer = %name, endpoint = %client_endpoint, "Checking peer");

        if etcd_http_health(&client_endpoint).await? {
            info!(peer = %name, "Found healthy cluster");
            return Ok(Some(client_endpoint));
        }
    }

    Ok(None)
}

/// Wait for leader or any healthy peer
pub async fn wait_for_any_healthy_peer(
    config: &Config,
    preferred_leader: &str,
) -> Result<(String, String)> {
    let cluster = parse_initial_cluster(&config.initial_cluster)?;

    info!(leader = %preferred_leader, "Waiting for bootstrap leader or any healthy peer");

    let start = std::time::Instant::now();
    while start.elapsed() < config.peer_wait_timeout {
        // Try preferred leader first
        if let Some(endpoint) = get_leader_endpoint(&config.initial_cluster, preferred_leader)? {
            if etcd_http_health(&endpoint).await? {
                info!(leader = %preferred_leader, "Leader is healthy");
                return Ok((preferred_leader.to_string(), endpoint));
            }
            info!(leader = %preferred_leader, "Leader health check failed");
        }

        // Try any other peer
        for (name, peer_url) in cluster.iter() {
            if name == &config.etcd_name || name == preferred_leader {
                continue;
            }

            let client_endpoint = peer_to_client_url(peer_url);
            if etcd_http_health(&client_endpoint).await? {
                info!(peer = %name, "Found healthy peer");
                return Ok((name.clone(), client_endpoint));
            }
            info!(peer = %name, "Peer health check failed");
        }

        info!(
            elapsed = ?start.elapsed(),
            timeout = ?config.peer_wait_timeout,
            "No healthy peers yet"
        );

        sleep(config.peer_check_interval).await;
    }

    Err(anyhow!("Timeout waiting for any healthy peer"))
}

/// Clean stale data on startup (only if no bootstrap marker)
pub async fn clean_stale_data(config: &Config, telemetry: &Telemetry) -> Result<()> {
    let data_path = Path::new(&config.data_dir);
    if !data_path.exists() {
        return Ok(());
    }

    let has_data = has_local_data(&config.data_dir).await?;
    let marker_exists = bootstrap_marker_present(&config.bootstrap_marker());

    if has_data && !marker_exists {
        info!("Found stale data from incomplete bootstrap - cleaning");
        match clear_directory(data_path).await {
            Ok(()) => {
                telemetry.send(TelemetryEvent::EtcdDataCleared {
                    node: config.etcd_name.clone(),
                    reason: "stale data from incomplete bootstrap".to_string(),
                });
                info!("Data directory cleaned");
            }
            Err(e) => {
                telemetry.send(TelemetryEvent::ComponentError {
                    component: "etcd".to_string(),
                    error: e.to_string(),
                    context: "clearing stale data on startup".to_string(),
                });
                return Err(e);
            }
        }
    } else if has_data {
        info!("Found data with bootstrap marker - preserving");
    }

    Ok(())
}

const MAX_PROMOTION_RETRIES: u32 = 180; // 15 minutes at 5 second intervals

/// Pace of promotion attempts once `MAX_PROMOTION_RETRIES` have failed on a
/// member that is not allowed to exit for them (see [`after_failed_promotion`]).
const SLOW_PROMOTION_RETRY: Duration = Duration::from_secs(60);

/// What to do after a failed promotion attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromotionRetry {
    /// Try again on the next cycle.
    Now,
    /// Try again after `SLOW_PROMOTION_RETRY`.
    Slowly,
    /// Exit so the restart wipes the half-joined data dir and joins again.
    Exit,
}

/// A node that joined as a learner in this run has no bootstrap marker yet:
/// when promotion keeps failing, exiting lets the restart wipe the half-joined
/// data dir and join again. A node that completed bootstrap earlier (a restart
/// with the marker present) is never wiped for this; it keeps retrying, once
/// a minute after the cap, so a learner left behind by an older image is
/// promoted as soon as etcd accepts it.
pub(crate) fn after_failed_promotion(joined_as_learner: bool, attempts: u32) -> PromotionRetry {
    if attempts < MAX_PROMOTION_RETRIES {
        PromotionRetry::Now
    } else if joined_as_learner {
        PromotionRetry::Exit
    } else {
        PromotionRetry::Slowly
    }
}

/// The bootstrap marker means "this member is a full voting member". A node
/// that joined as a learner may write it only once its own entry (found by
/// member ID or peer URL) is confirmed voting.
pub(crate) fn may_write_marker(joined_as_learner: bool, voter_confirmed: bool) -> bool {
    !joined_as_learner || voter_confirmed
}

/// Warn once when another member carries this node's name at another peer
/// URL. Such a pair predates the join-time refusal; this node still works
/// (it identifies itself by peer URL), the operator should still rename it.
async fn warn_if_name_shared(config: &Config, me: &SelfIdentity, telemetry: &Telemetry) {
    let Ok(Some(endpoint)) = get_voting_member_endpoint(&config.initial_cluster).await else {
        return;
    };
    let Ok(members) = get_member_list(&endpoint).await else {
        return;
    };
    if let Some(other) = member_sharing_my_name(&members, me, false) {
        telemetry.send(TelemetryEvent::ComponentError {
            component: "etcd".to_string(),
            error: shared_name_message(me, other),
            context: "ETCD_NAME shared with another member".to_string(),
        });
    }
}

/// Monitor and mark bootstrap complete
///
/// On every start, whatever the bootstrap path, this confirms that this
/// node's own member entry (found by member ID or peer URL, never by a name
/// another member may share) is a voting member, and promotes it when it is
/// still a learner. A restarted learner therefore retries promotion instead
/// of staying a learner forever.
///
/// IMPORTANT: This function runs in a spawned task. On fatal errors (health check
/// failure or promotion exhaustion of a fresh join), it calls exit(1) directly to
/// crash the process. This triggers container restart, and clean_stale_data() will
/// clear incomplete bootstrap data (no marker = data gets wiped) ensuring clean recovery.
pub async fn monitor_and_mark_bootstrap(
    config: &Config,
    joined_as_learner: bool,
    member_id: Option<String>,
    telemetry: Telemetry,
) {
    let me = SelfIdentity::from_config(config, member_id);
    let mut voter_confirmed = false;
    let mut promotion_attempts = 0u32;
    let mut next_promotion_attempt = Instant::now();
    let mut auth_enabled = config.root_password.is_none();

    loop {
        sleep(std::time::Duration::from_secs(5)).await;

        let is_healthy = match check_cluster_health(&config.initial_cluster).await {
            Ok(healthy) => healthy,
            Err(e) => {
                // Health check error (not just unhealthy) - crash to trigger recovery
                // On restart, clean_stale_data() will clear data since no bootstrap marker
                telemetry.send(TelemetryEvent::ComponentError {
                    component: "etcd".to_string(),
                    error: e.to_string(),
                    context: "health check failed with error".to_string(),
                });
                tracing::error!(error = %e, "Health check error - exiting for recovery");
                std::process::exit(1);
            }
        };

        if !is_healthy {
            continue;
        }

        // Turn on authentication once this member is a voting participant
        // of a healthy cluster. Idempotent on the etcd side, so every
        // member may attempt it; the first to succeed settles it.
        if !auth_enabled && may_write_marker(joined_as_learner, voter_confirmed) {
            match ensure_auth_enabled(config.root_password.as_deref().unwrap_or_default()).await {
                Ok(()) => auth_enabled = true,
                Err(e) => warn!(error = %e, "etcd auth enable failed; retrying next cycle"),
            }
        }

        if !voter_confirmed && Instant::now() >= next_promotion_attempt {
            if joined_as_learner {
                info!("Healthy, attempting promotion");
            }
            match promote_self(&config.initial_cluster, &me, &telemetry).await {
                Ok(outcome) => {
                    voter_confirmed = true;
                    if outcome == PromoteOutcome::Promoted && !joined_as_learner {
                        warn!("This member was still a learner after a restart and has now been promoted");
                    }
                    warn_if_name_shared(config, &me, &telemetry).await;
                }
                Err(e) => {
                    promotion_attempts += 1;
                    match after_failed_promotion(joined_as_learner, promotion_attempts) {
                        PromotionRetry::Exit => {
                            // Promotion exhausted - crash to trigger recovery
                            // On restart, clean_stale_data() will clear data since no bootstrap marker
                            telemetry.send(TelemetryEvent::EtcdPromotionFailed {
                                node: config.etcd_name.clone(),
                                attempts: promotion_attempts,
                                max_attempts: MAX_PROMOTION_RETRIES,
                                error: e.to_string(),
                            });
                            tracing::error!(
                                attempts = promotion_attempts,
                                error = %e,
                                "Promotion exhausted - exiting for recovery"
                            );
                            std::process::exit(1);
                        }
                        PromotionRetry::Now => {
                            warn!(
                                error = %e,
                                attempt = promotion_attempts,
                                max = MAX_PROMOTION_RETRIES,
                                "Promotion failed, will retry"
                            );
                        }
                        PromotionRetry::Slowly => {
                            if promotion_attempts == MAX_PROMOTION_RETRIES {
                                telemetry.send(TelemetryEvent::EtcdPromotionFailed {
                                    node: config.etcd_name.clone(),
                                    attempts: promotion_attempts,
                                    max_attempts: MAX_PROMOTION_RETRIES,
                                    error: e.to_string(),
                                });
                            }
                            warn!(
                                error = %e,
                                attempt = promotion_attempts,
                                retry_in_secs = SLOW_PROMOTION_RETRY.as_secs(),
                                "Could not confirm this member is a voting member; retrying"
                            );
                            next_promotion_attempt = Instant::now() + SLOW_PROMOTION_RETRY;
                        }
                    }
                }
            }
        }

        let marker_path = config.bootstrap_marker();
        let marker_present = bootstrap_marker_present(&marker_path);
        if !marker_present && may_write_marker(joined_as_learner, voter_confirmed) {
            if let Err(e) = fs::write(&marker_path, "1").await {
                telemetry.send(TelemetryEvent::ComponentError {
                    component: "etcd".to_string(),
                    error: e.to_string(),
                    context: "writing bootstrap marker".to_string(),
                });
                tracing::error!(error = %e, "Failed to write bootstrap marker - exiting");
                std::process::exit(1);
            }
            info!("Bootstrap marked complete");
        }

        // The marker is present or was just written: nothing left to watch.
        if voter_confirmed {
            break;
        }
    }
}

/// True when an etcdctl failure means the step already happened.
fn is_already_done(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("already exists") || e.contains("already enabled") || e.contains("already granted")
}

/// Run one auth-setup step, treating "already done" as success.
async fn auth_step(args: &[&str]) -> Result<()> {
    match etcdctl(args).await {
        Ok(_) => Ok(()),
        Err(e) if is_already_done(&e.to_string()) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Enable etcd authentication with `root` (root role) as the only account.
/// Each step is idempotent, and the root credential rides along on every
/// `etcdctl` call in its environment (accepted by etcd before enabling,
/// required after). The user itself is created through the gRPC gateway, so
/// the password is never on a command line (see `common::etcd_http_user_add`).
/// A cluster that already enforces has the user by construction, so the
/// status check comes first and the remaining steps run only while the
/// cluster is still open. If a peer enables authentication between the two,
/// the tokenless user add fails and the next cycle finds the status flipped.
async fn ensure_auth_enabled(root_password: &str) -> Result<()> {
    const LOCAL: &str = "127.0.0.1:2379";
    let ep = "--endpoints=127.0.0.1:2379";
    let status = etcdctl(&["auth", "status", ep]).await?;
    if status.contains("Authentication Status: true") {
        return Ok(());
    }
    etcd_http_user_add(LOCAL, "root", root_password).await?;
    auth_step(&["role", "add", "root", ep]).await?;
    auth_step(&["user", "grant-role", "root", "root", ep]).await?;
    etcdctl(&["auth", "enable", ep]).await?;
    info!("etcd authentication enabled (root)");
    Ok(())
}

/// Result of bootstrap determination
pub struct BootstrapParams {
    pub initial_cluster: String,
    pub initial_cluster_state: String,
    pub joined_as_learner: bool,
    /// This node's member ID when the bootstrap learned it (from
    /// `member add`); the monitor then finds its entry by ID.
    pub member_id: Option<String>,
}

/// Determine bootstrap parameters for the leader node
pub async fn bootstrap_as_leader(
    config: &Config,
    telemetry: &Telemetry,
) -> Result<Option<BootstrapParams>> {
    let marker_exists = bootstrap_marker_present(&config.bootstrap_marker());
    let cluster = parse_initial_cluster(&config.initial_cluster)?;

    if marker_exists {
        return Ok(Some(BootstrapParams {
            initial_cluster: config.initial_cluster.clone(),
            initial_cluster_state: "existing".to_string(),
            joined_as_learner: false,
            member_id: None,
        }));
    }

    // Check for recovery scenario - existing cluster on other peers.
    // A single miss is not enough: a rolling restart of the designated
    // leader can land here while the existing cluster is still coming up,
    // and `state=new` would mint a second cluster id. Probe a few more
    // times when the declared topology has peers; first-time deploys still
    // bootstrap as new once the probes miss.
    let mut existing_endpoint =
        check_existing_cluster(&config.initial_cluster, &config.etcd_name).await?;
    if existing_endpoint.is_none() && cluster.len() > 1 {
        const EXTRA_PROBES: u32 = 5;
        for i in 1..=EXTRA_PROBES {
            warn!(
                attempt = i,
                extra = EXTRA_PROBES,
                "no healthy peer yet; not bootstrapping a new cluster"
            );
            sleep(config.peer_check_interval).await;
            existing_endpoint =
                check_existing_cluster(&config.initial_cluster, &config.etcd_name).await?;
            if existing_endpoint.is_some() {
                break;
            }
        }
    }
    if let Some(existing_endpoint) = existing_endpoint {
        info!("RECOVERY MODE: Found existing cluster");

        telemetry.send(TelemetryEvent::EtcdRecoveryMode {
            node: config.etcd_name.clone(),
            reason: "Leader volume lost, cluster exists".to_string(),
        });

        let me = SelfIdentity::from_config(config, None);

        // Never join under a name a voting member already holds: the node
        // could not tell itself apart from that member afterwards.
        let members = match get_member_list(&existing_endpoint).await {
            Ok(members) => members,
            Err(e) => {
                warn!(error = %e, "Failed to list members during recovery");
                return Ok(None); // Signal retry needed
            }
        };
        refuse_name_collision(&members, &me, telemetry)?;

        if let Err(e) = remove_stale_self(&existing_endpoint, &me, telemetry).await {
            warn!(error = %e, "Failed to remove stale self, continuing anyway");
        }

        match add_as_learner(config, &existing_endpoint, &me).await {
            Ok(joined) => {
                telemetry.send(TelemetryEvent::EtcdNodeJoined {
                    node: config.etcd_name.clone(),
                    joined_as: "learner".to_string(),
                });

                info!(cluster = %joined.initial_cluster, "Joining as learner (recovery)");
                return Ok(Some(BootstrapParams {
                    initial_cluster: joined.initial_cluster,
                    initial_cluster_state: "existing".to_string(),
                    joined_as_learner: true,
                    member_id: joined.member_id,
                }));
            }
            Err(e) => {
                warn!(error = %e, "Failed to add as learner during recovery");
                return Ok(None); // Signal retry needed
            }
        }
    }

    // Fresh bootstrap - single node cluster
    // Use the node's own advertise URL directly instead of looking up in initial_cluster
    let my_peer_url = &config.initial_advertise_peer_urls;

    let single_node_cluster = format!("{}={}", config.etcd_name, my_peer_url);
    info!(cluster = %single_node_cluster, "Bootstrapping single-node cluster");

    telemetry.send(TelemetryEvent::EtcdBootstrap {
        node: config.etcd_name.clone(),
        is_leader: true,
        cluster_size: cluster.len(),
    });

    Ok(Some(BootstrapParams {
        initial_cluster: single_node_cluster,
        initial_cluster_state: "new".to_string(),
        joined_as_learner: false,
        member_id: None,
    }))
}

/// Determine bootstrap parameters for a follower node
pub async fn bootstrap_as_follower(
    config: &Config,
    bootstrap_leader: &str,
    telemetry: &Telemetry,
) -> Result<Option<BootstrapParams>> {
    let marker_exists = bootstrap_marker_present(&config.bootstrap_marker());

    if marker_exists {
        return Ok(Some(BootstrapParams {
            initial_cluster: config.initial_cluster.clone(),
            initial_cluster_state: "existing".to_string(),
            joined_as_learner: false,
            member_id: None,
        }));
    }

    // Wait for a healthy peer
    let (healthy_peer, endpoint) = match wait_for_any_healthy_peer(config, bootstrap_leader).await {
        Ok(result) => result,
        Err(e) => {
            warn!(error = %e, "Failed to find healthy peer");
            return Ok(None); // Signal retry needed
        }
    };

    match add_self_to_cluster(config, &healthy_peer, &endpoint, telemetry).await {
        Ok(joined) => {
            info!(cluster = %joined.initial_cluster, via = %healthy_peer, "Joining as learner");
            telemetry.send(TelemetryEvent::EtcdNodeJoined {
                node: config.etcd_name.clone(),
                joined_as: "learner".to_string(),
            });
            Ok(Some(BootstrapParams {
                initial_cluster: joined.initial_cluster,
                initial_cluster_state: "existing".to_string(),
                joined_as_learner: true,
                member_id: joined.member_id,
            }))
        }
        // A name collision is a configuration error: retrying cannot fix it.
        Err(e) if is_name_collision(&e) => Err(e),
        Err(e) => {
            warn!(error = %e, "Failed to add as learner");
            Ok(None) // Signal retry needed
        }
    }
}

// ---- Periodic defragmentation ----
// etcd compaction removes old revisions from the keyspace but does not reclaim
// physical space in the underlying bolt DB file. Without periodic defrag the DB
// grows continuously from fragmentation, causing slow fdatasync/reads and
// heartbeat timeouts. Each node defrags only itself (localhost:2379), so no
// cross-node coordination is needed. Jitter from the node name staggers defrag
// times so that the three nodes never defrag simultaneously.

const DEFRAG_INTERVAL: Duration = Duration::from_secs(8 * 60 * 60);

/// Derive a deterministic per-node jitter from the node name (no rand dep needed).
fn jitter_from_name(name: &str, max: Duration) -> Duration {
    let hash = name
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    Duration::from_secs(hash % max.as_secs())
}

/// Returns true if the local etcd node is currently the Raft leader.
async fn is_this_node_leader() -> bool {
    match etcdctl(&[
        "endpoint",
        "status",
        "--endpoints=127.0.0.1:2379",
        "--write-out=json",
    ])
    .await
    {
        Ok(output) => output.contains("\"isLeader\":true"),
        Err(_) => false,
    }
}

/// Transfer etcd leadership to a voting follower so this node can defrag safely.
async fn move_leader_away(config: &Config) -> Result<()> {
    let members = get_member_list("127.0.0.1:2379").await?;
    // Exclude this node by its own entry (peer URL), not by name: another
    // member may share the name and is a valid target.
    let me = SelfIdentity::from_config(config, None);
    let my_id = find_self(&members, &me).map(|(m, _)| m.id.clone());
    let follower = members.iter().find(|m| {
        !m.is_learner
            && match &my_id {
                Some(id) => &m.id != id,
                None => m.name != config.etcd_name,
            }
    });

    match follower {
        Some(m) => {
            etcdctl(&["move-leader", &m.id, "--endpoints=127.0.0.1:2379"]).await?;
            info!(target = %m.name, "Leadership transferred");
            Ok(())
        }
        None => Err(anyhow!(
            "No voting follower available to transfer leadership to"
        )),
    }
}

/// Periodic defragmentation loop. Runs as a background task after bootstrap completes.
pub async fn defrag_loop(config: Config, telemetry: Telemetry) {
    // Wait for bootstrap to complete before the first defrag. Same marker
    // semantics as everywhere else: a marker we cannot stat counts as present
    // (defrag is further gated on a local health probe below, so proceeding
    // on a sick volume is safe — the probe fails and defrag is skipped).
    let marker_path = config.bootstrap_marker();
    loop {
        if bootstrap_marker_present(&marker_path) {
            break;
        }
        sleep(Duration::from_secs(10)).await;
    }

    // Per-node jitter to prevent all three nodes defraging at the same time
    let jitter = jitter_from_name(&config.etcd_name, DEFRAG_INTERVAL);
    info!(jitter_secs = jitter.as_secs(), node = %config.etcd_name, "Defrag initial jitter");
    sleep(jitter).await;

    loop {
        match etcd_http_health("127.0.0.1:2379").await {
            Ok(true) => {
                if is_this_node_leader().await {
                    info!("This node is leader; transferring leadership and skipping defrag until next cycle");
                    match move_leader_away(&config).await {
                        Ok(_) => {}
                        Err(e) => {
                            warn!(error = %e, "Failed to transfer leadership, skipping defrag");
                        }
                    }
                    sleep(DEFRAG_INTERVAL).await;
                    continue;
                }
                info!("Starting defrag");
                match etcdctl(&["defrag", "--endpoints=127.0.0.1:2379"]).await {
                    Ok(_) => info!("Defrag complete"),
                    Err(e) => {
                        warn!(error = %e, "Defrag failed");
                        telemetry.send(TelemetryEvent::EtcdDefragFailed {
                            node: config.etcd_name.clone(),
                            error: e.to_string(),
                        });
                    }
                }
            }
            Ok(false) => info!("Skipping defrag: local etcd unhealthy"),
            Err(e) => warn!(error = %e, "Health check error, skipping defrag"),
        }
        sleep(DEFRAG_INTERVAL).await;
    }
}

// ---- Local liveness watchdog ----
// monitor_and_mark_bootstrap stops once bootstrap completes, and check_cluster_health
// treats a healthy *peer* as proof the cluster is up — so after bootstrap nothing
// watches whether THIS node's etcd is actually serving. A wedged-but-running etcd
// (process up, not answering) therefore sits forever as a healthy SUCCESS deployment
// while its peers carry quorum: a zombie member that reduces real fault tolerance
// without surfacing as a crash. This watchdog closes that gap by checking the LOCAL
// endpoint only and crashing the container if it stays unhealthy — flipping the
// deployment to CRASHED so the platform (and the postgres-ha monitor's crashed-node
// self-heal) restarts it, which clears the wedge (or, if the data dir is corrupt,
// triggers the wipe-and-re-clone recovery).

const LIVENESS_CHECK_INTERVAL: Duration = Duration::from_secs(5);

// Grace before we treat a non-serving local etcd as wedged. Generous enough to ride
// out the brief unavailability of a blocking defrag, compaction, snapshot install, or
// leader election (these etcd DBs are tiny — only Patroni keys), short enough to
// recover a genuinely dead node in ~1.5min instead of leaving a silent zombie.
const LIVENESS_UNHEALTHY_GRACE: Duration = Duration::from_secs(90);

/// Long-lived watchdog over the LOCAL etcd endpoint (127.0.0.1:2379). Runs for the
/// lifetime of the etcd process (aborted by main when the child exits). Only arms
/// after the local node has served at least once, so it never kills a node still
/// doing its initial clone/bootstrap.
pub async fn local_liveness_watchdog(config: Config, telemetry: Telemetry) {
    let mut healthy_once = false;
    let mut unhealthy_since: Option<Instant> = None;

    loop {
        sleep(LIVENESS_CHECK_INTERVAL).await;

        let local_ok = etcd_http_health("127.0.0.1:2379").await.unwrap_or(false);

        if local_ok {
            healthy_once = true;
            unhealthy_since = None;
            continue;
        }

        // Don't arm until the node has served once — a node still cloning/bootstrapping
        // is legitimately unhealthy and must not be killed.
        if !healthy_once {
            continue;
        }

        let since = *unhealthy_since.get_or_insert_with(Instant::now);
        let elapsed = since.elapsed();
        if elapsed >= LIVENESS_UNHEALTHY_GRACE {
            telemetry.send(TelemetryEvent::EtcdLocalUnhealthy {
                node: config.etcd_name.clone(),
                unhealthy_secs: elapsed.as_secs(),
            });
            tracing::error!(
                unhealthy_secs = elapsed.as_secs(),
                "Local etcd unhealthy while running - exiting so the platform restarts this node"
            );
            std::process::exit(1);
        }
        warn!(
            unhealthy_secs = elapsed.as_secs(),
            grace_secs = LIVENESS_UNHEALTHY_GRACE.as_secs(),
            "Local etcd not serving; will exit for restart if it does not recover"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_marker_is_absent() {
        assert!(!bootstrap_marker_present(
            "/no/such/etcd-data/.bootstrap_complete"
        ));
    }

    #[test]
    fn existing_marker_is_present() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("etcd-bootstrap-marker-test-{}", std::process::id()));
        std::fs::write(&path, "1").unwrap();
        let present = bootstrap_marker_present(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        assert!(present);
    }

    /// The load-bearing branch: a marker that exists but cannot be stat'd
    /// (EACCES via an unsearchable parent directory) must read as PRESENT,
    /// not absent — absent is what triggers the data wipe. Under root the
    /// stat succeeds instead (permissions are bypassed), which still reads
    /// as present, so the assertion holds either way.
    #[cfg(unix)]
    #[test]
    fn unreadable_marker_is_treated_as_present() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "etcd-bootstrap-marker-denied-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join(".bootstrap_complete");
        std::fs::write(&marker, "1").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let present = bootstrap_marker_present(marker.to_str().unwrap());

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(present);
    }

    #[test]
    fn a_learner_never_writes_the_marker_before_it_is_confirmed_voting() {
        // onyx-staging etcd-3 (ETCD_NAME=etcd-2) wrote its marker while still
        // a learner: the name lookup found the voting etcd-2 and reported
        // "Already a voting member". The marker now waits for this node's own
        // entry to be confirmed voting.
        assert!(!may_write_marker(true, false));
        assert!(may_write_marker(true, true));
        // A bootstrap leader or a restart with the marker never joined as a
        // learner in this run; its marker gating is unchanged.
        assert!(may_write_marker(false, false));
    }

    #[test]
    fn a_fresh_learner_retries_promotion_until_the_cap_then_exits() {
        for attempts in 1..MAX_PROMOTION_RETRIES {
            assert_eq!(after_failed_promotion(true, attempts), PromotionRetry::Now);
        }
        assert_eq!(
            after_failed_promotion(true, MAX_PROMOTION_RETRIES),
            PromotionRetry::Exit
        );
    }

    #[test]
    fn a_restarted_member_keeps_retrying_promotion_and_never_exits_for_it() {
        // Data dir and marker present: exiting would not wipe anything, it
        // would only crash-loop. It keeps trying, once a minute after the cap.
        assert_eq!(after_failed_promotion(false, 1), PromotionRetry::Now);
        assert_eq!(
            after_failed_promotion(false, MAX_PROMOTION_RETRIES),
            PromotionRetry::Slowly
        );
        assert_eq!(
            after_failed_promotion(false, MAX_PROMOTION_RETRIES * 50),
            PromotionRetry::Slowly
        );
    }

    #[test]
    fn already_done_errors_are_recognized() {
        assert!(is_already_done("etcdctl failed (exit 1): {\"level\":\"warn\"} Error: etcdserver: user name already exists"));
        assert!(is_already_done(
            "Error: etcdserver: role name already exists"
        ));
        assert!(is_already_done(
            "Error: etcdserver: authentication is already enabled"
        ));
        assert!(!is_already_done("Error: etcdserver: permission denied"));
        assert!(!is_already_done("connection refused"));
    }
}
