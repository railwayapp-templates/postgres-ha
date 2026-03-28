//! Bootstrap logic for etcd cluster
//!
//! Handles leader election, recovery detection, and cluster initialization.

use crate::cluster::{
    add_self_to_cluster, check_cluster_health, clear_directory, get_current_cluster,
    get_member_list, has_local_data, promote_self, remove_stale_self,
};
use crate::config::{get_leader_endpoint, parse_initial_cluster, peer_to_client_url, Config};
use anyhow::{anyhow, Result};
use common::{etcdctl, etcdctl_probe, Telemetry, TelemetryEvent};
use std::path::Path;
use std::time::Duration;
use tokio::fs;
use tokio::time::sleep;
use tracing::{info, warn};

/// Check if any other peer has a healthy cluster (for recovery detection)
pub async fn check_existing_cluster(initial_cluster: &str, my_name: &str) -> Result<Option<String>> {
    info!("Checking for existing cluster on other peers...");

    let cluster = parse_initial_cluster(initial_cluster)?;
    for (name, peer_url) in cluster.iter() {
        if name == my_name {
            continue;
        }

        let client_endpoint = peer_to_client_url(peer_url);
        info!(peer = %name, endpoint = %client_endpoint, "Checking peer");

        if etcdctl_probe(&["endpoint", "health", &format!("--endpoints={}", client_endpoint)])
            .await?
        {
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
            if etcdctl_probe(&["endpoint", "health", &format!("--endpoints={}", endpoint)]).await? {
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
            if etcdctl_probe(&["endpoint", "health", &format!("--endpoints={}", client_endpoint)])
                .await?
            {
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
    let marker_exists = Path::new(&config.bootstrap_marker()).exists();

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

/// Monitor and mark bootstrap complete
///
/// IMPORTANT: This function runs in a spawned task. On fatal errors (health check
/// failure or promotion exhaustion), it calls exit(1) directly to crash the process.
/// This triggers container restart, and clean_stale_data() will clear incomplete
/// bootstrap data (no marker = data gets wiped) ensuring clean recovery.
pub async fn monitor_and_mark_bootstrap(
    config: &Config,
    joined_as_learner: bool,
    telemetry: Telemetry,
) {
    let mut promoted = false;
    let mut promotion_attempts = 0u32;

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

        if is_healthy {
            if joined_as_learner && !promoted {
                info!("Healthy, attempting promotion");
                match promote_self(&config.initial_cluster, &config.etcd_name, &telemetry).await {
                    Ok(_) => {
                        promoted = true;
                    }
                    Err(e) => {
                        promotion_attempts += 1;
                        if promotion_attempts >= MAX_PROMOTION_RETRIES {
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
                        warn!(
                            error = %e,
                            attempt = promotion_attempts,
                            max = MAX_PROMOTION_RETRIES,
                            "Promotion failed, will retry"
                        );
                    }
                }
            }

            let marker_path = config.bootstrap_marker();
            if !Path::new(&marker_path).exists() && (!joined_as_learner || promoted) {
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
                break;
            }
        }
    }
}

/// Result of bootstrap determination
pub struct BootstrapParams {
    pub initial_cluster: String,
    pub initial_cluster_state: String,
    pub joined_as_learner: bool,
}

/// Determine bootstrap parameters for the leader node
pub async fn bootstrap_as_leader(
    config: &Config,
    telemetry: &Telemetry,
) -> Result<Option<BootstrapParams>> {
    let marker_exists = Path::new(&config.bootstrap_marker()).exists();
    let cluster = parse_initial_cluster(&config.initial_cluster)?;

    if marker_exists {
        return Ok(Some(BootstrapParams {
            initial_cluster: config.initial_cluster.clone(),
            initial_cluster_state: "existing".to_string(),
            joined_as_learner: false,
        }));
    }

    // Check for recovery scenario - existing cluster on other peers
    if let Some(existing_endpoint) =
        check_existing_cluster(&config.initial_cluster, &config.etcd_name).await?
    {
        info!("RECOVERY MODE: Found existing cluster");

        telemetry.send(TelemetryEvent::EtcdRecoveryMode {
            node: config.etcd_name.clone(),
            reason: "Leader volume lost, cluster exists".to_string(),
        });

        // Use the node's own advertise URL directly instead of looking up in initial_cluster
        let my_peer_url = &config.initial_advertise_peer_urls;

        if let Err(e) = remove_stale_self(&existing_endpoint, &config.etcd_name, my_peer_url, telemetry).await {
            warn!(error = %e, "Failed to remove stale self, continuing anyway");
        }

        let output = etcdctl(&[
            "member",
            "add",
            &config.etcd_name,
            "--learner",
            &format!("--peer-urls={}", my_peer_url),
            &format!("--endpoints={}", existing_endpoint),
        ])
        .await;

        match output {
            Ok(out) => {
                telemetry.send(TelemetryEvent::EtcdNodeJoined {
                    node: config.etcd_name.clone(),
                    joined_as: "learner".to_string(),
                });

                let mut cluster_str = String::new();
                for line in out.lines() {
                    if line.contains("ETCD_INITIAL_CLUSTER=") {
                        if let Some(c) = line
                            .split("ETCD_INITIAL_CLUSTER=")
                            .nth(1)
                            .map(|s| s.trim_matches('"').to_string())
                        {
                            cluster_str = c;
                            break;
                        }
                    }
                }

                if cluster_str.is_empty() {
                    cluster_str =
                        get_current_cluster(&existing_endpoint, &config.etcd_name, my_peer_url)
                            .await?;
                }

                info!(cluster = %cluster_str, "Joining as learner (recovery)");
                return Ok(Some(BootstrapParams {
                    initial_cluster: cluster_str,
                    initial_cluster_state: "existing".to_string(),
                    joined_as_learner: true,
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
    }))
}

/// Determine bootstrap parameters for a follower node
pub async fn bootstrap_as_follower(
    config: &Config,
    bootstrap_leader: &str,
    telemetry: &Telemetry,
) -> Result<Option<BootstrapParams>> {
    let marker_exists = Path::new(&config.bootstrap_marker()).exists();

    if marker_exists {
        return Ok(Some(BootstrapParams {
            initial_cluster: config.initial_cluster.clone(),
            initial_cluster_state: "existing".to_string(),
            joined_as_learner: false,
        }));
    }

    // Wait for a healthy peer
    let (healthy_peer, endpoint) =
        match wait_for_any_healthy_peer(config, bootstrap_leader).await {
            Ok(result) => result,
            Err(e) => {
                warn!(error = %e, "Failed to find healthy peer");
                return Ok(None); // Signal retry needed
            }
        };

    match add_self_to_cluster(config, &healthy_peer, &endpoint, telemetry).await {
        Ok(cluster_str) => {
            info!(cluster = %cluster_str, via = %healthy_peer, "Joining as learner");
            telemetry.send(TelemetryEvent::EtcdNodeJoined {
                node: config.etcd_name.clone(),
                joined_as: "learner".to_string(),
            });
            Ok(Some(BootstrapParams {
                initial_cluster: cluster_str,
                initial_cluster_state: "existing".to_string(),
                joined_as_learner: true,
            }))
        }
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
    let follower = members
        .iter()
        .find(|m| !m.is_learner && m.name != config.etcd_name);

    match follower {
        Some(m) => {
            etcdctl(&["move-leader", &m.id, "--endpoints=127.0.0.1:2379"]).await?;
            info!(target = %m.name, "Leadership transferred");
            Ok(())
        }
        None => Err(anyhow!("No voting follower available to transfer leadership to")),
    }
}

/// Periodic defragmentation loop. Runs as a background task after bootstrap completes.
pub async fn defrag_loop(config: Config, telemetry: Telemetry) {
    // Wait for bootstrap to complete before the first defrag
    let marker_path = config.bootstrap_marker();
    loop {
        if Path::new(&marker_path).exists() {
            break;
        }
        sleep(Duration::from_secs(10)).await;
    }

    // Per-node jitter to prevent all three nodes defraging at the same time
    let jitter = jitter_from_name(&config.etcd_name, DEFRAG_INTERVAL);
    info!(jitter_secs = jitter.as_secs(), node = %config.etcd_name, "Defrag initial jitter");
    sleep(jitter).await;

    loop {
        match etcdctl_probe(&["endpoint", "health", "--endpoints=127.0.0.1:2379"]).await {
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
