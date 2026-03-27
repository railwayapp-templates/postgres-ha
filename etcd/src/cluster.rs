//! etcd cluster operations
//!
//! Functions for managing cluster membership, starting etcd, and health checking.

use crate::config::{parse_initial_cluster, peer_to_client_url, Config};
use anyhow::{anyhow, Context, Result};
use common::{etcdctl, etcdctl_probe, Telemetry, TelemetryEvent};
use std::path::Path;
use std::process::Stdio;
use tokio::fs;
use tokio::process::Command;
use tracing::{info, warn};

/// Information about an etcd cluster member
#[derive(Debug)]
pub struct MemberInfo {
    pub id: String,
    pub name: String,
    pub peer_url: String,
    pub is_learner: bool,
}

/// Get member list from etcd
pub async fn get_member_list(endpoint: &str) -> Result<Vec<MemberInfo>> {
    let output = etcdctl(&[
        "member",
        "list",
        &format!("--endpoints={}", endpoint),
        "-w",
        "simple",
    ])
    .await?;

    let members: Vec<MemberInfo> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
            if parts.len() >= 5 {
                Ok(MemberInfo {
                    id: parts[0].to_string(),
                    name: parts[2].to_string(),
                    peer_url: parts[3].to_string(),
                    is_learner: parts.get(5).map(|s| *s == "true").unwrap_or(false),
                })
            } else {
                Err(anyhow!(
                    "Invalid member list line '{}': expected at least 5 comma-separated fields",
                    line
                ))
            }
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(members)
}

/// Check cluster health via localhost or voting member
pub async fn check_cluster_health(initial_cluster: &str) -> Result<bool> {
    if etcdctl_probe(&["endpoint", "health", "--endpoints=127.0.0.1:2379"]).await? {
        return Ok(true);
    }

    if let Some(endpoint) = get_voting_member_endpoint(initial_cluster).await? {
        return etcdctl_probe(&["endpoint", "health", &format!("--endpoints={}", endpoint)]).await;
    }

    Ok(false)
}

/// Find a voting member endpoint
pub async fn get_voting_member_endpoint(initial_cluster: &str) -> Result<Option<String>> {
    let cluster = parse_initial_cluster(initial_cluster)?;

    for (_name, peer_url) in cluster.iter() {
        let client_endpoint = peer_to_client_url(peer_url);
        if etcdctl_probe(&["member", "list", &format!("--endpoints={}", client_endpoint)]).await? {
            return Ok(Some(client_endpoint));
        }
    }

    Ok(None)
}

/// Get my member ID from etcd cluster
pub async fn get_my_member_id(endpoint: &str, my_name: &str) -> Result<Option<String>> {
    let members = get_member_list(endpoint).await?;
    for member in members {
        if member.name == my_name {
            return Ok(Some(member.id));
        }
    }
    Ok(None)
}

/// Check if this member is a learner
/// Returns Err if we can't determine state
pub async fn is_learner(endpoint: &str, my_name: &str) -> Result<bool> {
    let members = get_member_list(endpoint).await?;
    for member in members {
        if member.name == my_name {
            return Ok(member.is_learner);
        }
    }
    // Not found in member list - not a learner (or not a member at all)
    Ok(false)
}

/// Remove stale member entry for this node
pub async fn remove_stale_self(
    endpoint: &str,
    my_name: &str,
    my_peer_url: &str,
    telemetry: &Telemetry,
) -> Result<()> {
    info!("Checking for stale member entry...");

    let members = get_member_list(endpoint).await?;

    for member in members {
        if member.name == my_name || member.peer_url == my_peer_url {
            info!(id = %member.id, "Removing stale member entry");
            match etcdctl(&[
                "member",
                "remove",
                &member.id,
                &format!("--endpoints={}", endpoint),
            ])
            .await
            {
                Ok(_) => {
                    telemetry.send(TelemetryEvent::EtcdStaleMemberRemoved {
                        node: my_name.to_string(),
                        removed_id: member.id.clone(),
                    });
                    info!("Stale member removed");
                    return Ok(());
                }
                Err(e) => {
                    telemetry.send(TelemetryEvent::ComponentError {
                        component: "etcd".to_string(),
                        error: e.to_string(),
                        context: format!("removing stale member {}", member.id),
                    });
                    return Err(e);
                }
            }
        }
    }

    info!("No stale member entry found");
    Ok(())
}

/// Build current cluster membership for joining node
pub async fn get_current_cluster(
    endpoint: &str,
    my_name: &str,
    my_peer_url: &str,
) -> Result<String> {
    let members = get_member_list(endpoint).await?;

    let mut cluster_parts: Vec<String> = members
        .iter()
        .filter(|m| !m.name.is_empty() && !m.peer_url.is_empty())
        .map(|m| format!("{}={}", m.name, m.peer_url))
        .collect();

    if !cluster_parts
        .iter()
        .any(|p| p.starts_with(&format!("{}=", my_name)))
    {
        cluster_parts.push(format!("{}={}", my_name, my_peer_url));
    }

    Ok(cluster_parts.join(","))
}

/// Add this node to an existing cluster as a learner
pub async fn add_self_to_cluster(
    config: &Config,
    leader: &str,
    leader_endpoint: &str,
    telemetry: &Telemetry,
) -> Result<String> {
    // Use the node's own advertise URL directly instead of looking up in initial_cluster.
    // This is necessary for learner bootstrap where ETCD_INITIAL_CLUSTER only contains the leader.
    let my_peer_url = &config.initial_advertise_peer_urls;

    info!(node = %config.etcd_name, via = %leader_endpoint, "Adding self as learner");

    // Check if already a member
    let members = get_member_list(leader_endpoint).await?;
    for member in &members {
        if member.name == config.etcd_name || member.peer_url == *my_peer_url {
            let has_data = has_local_data(&config.data_dir).await?;

            if !has_data {
                warn!("Registered as member but no local data - removing stale entry");
                remove_stale_self(leader_endpoint, &config.etcd_name, &my_peer_url, telemetry).await?;

                // Clean partial data
                match clear_directory(Path::new(&config.data_dir)).await {
                    Ok(()) => {
                        telemetry.send(TelemetryEvent::EtcdDataCleared {
                            node: config.etcd_name.clone(),
                            reason: "no local data but registered as member".to_string(),
                        });
                    }
                    Err(e) => {
                        telemetry.send(TelemetryEvent::ComponentError {
                            component: "etcd".to_string(),
                            error: e.to_string(),
                            context: "clearing partial data".to_string(),
                        });
                    }
                }
                break;
            } else {
                info!("Already a member with local data");
                return get_current_cluster(leader_endpoint, &config.etcd_name, &my_peer_url).await;
            }
        }
    }

    // Add as learner
    let output = match etcdctl(&[
        "member",
        "add",
        &config.etcd_name,
        "--learner",
        &format!("--peer-urls={}", my_peer_url),
        &format!("--endpoints={}", leader_endpoint),
    ])
    .await
    {
        Ok(output) => output,
        Err(e) => {
            telemetry.send(TelemetryEvent::ComponentError {
                component: "etcd".to_string(),
                error: e.to_string(),
                context: format!("adding {} as learner", config.etcd_name),
            });
            return Err(e);
        }
    };

    info!(via = %leader, "Successfully added as learner");

    // Extract ETCD_INITIAL_CLUSTER from output
    for line in output.lines() {
        if line.contains("ETCD_INITIAL_CLUSTER=") {
            let cluster = line
                .split("ETCD_INITIAL_CLUSTER=")
                .nth(1)
                .map(|s| s.trim_matches('"').to_string());
            if let Some(c) = cluster {
                if !c.is_empty() {
                    return Ok(c);
                }
            }
        }
    }

    info!("Extracting cluster from member list");
    get_current_cluster(leader_endpoint, &config.etcd_name, &my_peer_url).await
}

/// Promote self from learner to voting member
pub async fn promote_self(
    initial_cluster: &str,
    my_name: &str,
    telemetry: &Telemetry,
) -> Result<()> {
    let endpoint = get_voting_member_endpoint(initial_cluster)
        .await?
        .ok_or_else(|| anyhow!("Could not find voting member endpoint"))?;

    let member_id = get_my_member_id(&endpoint, my_name)
        .await?
        .ok_or_else(|| anyhow!("Could not find my member ID"))?;

    let learner = is_learner(&endpoint, my_name).await?;

    if !learner {
        info!("Already a voting member");
        return Ok(());
    }

    info!(id = %member_id, via = %endpoint, "Promoting from learner to voting member");

    match etcdctl(&[
        "member",
        "promote",
        &member_id,
        &format!("--endpoints={}", endpoint),
    ])
    .await
    {
        Ok(_) => {
            info!("Promoted to voting member");
            telemetry.send(TelemetryEvent::EtcdNodePromoted {
                node: my_name.to_string(),
            });
            Ok(())
        }
        Err(e) => {
            if e.to_string().contains("is not a learner") {
                info!("Already a voting member");
                Ok(())
            } else {
                telemetry.send(TelemetryEvent::ComponentError {
                    component: "etcd".to_string(),
                    error: e.to_string(),
                    context: format!("promoting {} to voting member", my_name),
                });
                Err(e)
            }
        }
    }
}

/// Check if we have valid local etcd data
/// Returns Err if we can't determine state (fail-safe: don't assume no data)
pub async fn has_local_data(data_dir: &str) -> Result<bool> {
    let wal_dir = format!("{}/member/wal", data_dir);

    // If WAL dir doesn't exist, that's a clear "no data" case
    if !Path::new(&wal_dir).exists() {
        return Ok(false);
    }

    // If WAL dir exists but we can't read it, that's an error - don't assume no data
    let mut entries = fs::read_dir(&wal_dir)
        .await
        .context("Failed to read WAL directory")?;

    match entries.next_entry().await {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => Err(anyhow::anyhow!("Failed to read WAL directory entries: {}", e)),
    }
}

/// Clear all contents of a directory without removing the directory itself
pub async fn clear_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut entries = fs::read_dir(path).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path).await?;
        } else {
            fs::remove_file(&path).await?;
        }
    }
    Ok(())
}

/// Start etcd process
pub async fn start_etcd(
    initial_cluster: &str,
    initial_cluster_state: &str,
) -> Result<tokio::process::Child> {
    info!(
        cluster = %initial_cluster,
        state = %initial_cluster_state,
        "Starting etcd"
    );

    let child = Command::new("/usr/local/bin/etcd")
        .arg("--auto-compaction-retention=1")
        .arg("--max-learners=2")
        .env("ETCD_INITIAL_CLUSTER", initial_cluster)
        .env("ETCD_INITIAL_CLUSTER_STATE", initial_cluster_state)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to start etcd")?;

    Ok(child)
}
