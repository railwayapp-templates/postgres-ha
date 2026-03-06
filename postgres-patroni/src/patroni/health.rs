//! Patroni health checking and cluster state management
//!
//! Provides health checking, cluster state queries, and safe reinitialize triggers.

use serde::Deserialize;
use std::time::Duration;
use tracing::{info, warn};

/// A member in the Patroni cluster
#[derive(Debug, Deserialize, Clone)]
pub struct ClusterMember {
    pub name: String,
    pub role: String,
    pub state: String,
    #[serde(default)]
    pub timeline: Option<i64>,
    #[serde(default)]
    pub lag: Option<i64>,
}

/// Response from Patroni /cluster endpoint
#[derive(Debug, Deserialize)]
pub struct ClusterStatus {
    pub members: Vec<ClusterMember>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Check Patroni health via REST API
pub async fn check_health(timeout_secs: u64) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    client
        .get("http://localhost:8008/health")
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Get cluster status from Patroni /cluster endpoint
pub async fn get_cluster_status(timeout_secs: u64) -> Option<ClusterStatus> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to create HTTP client for cluster status");
            return None;
        }
    };

    match client.get("http://localhost:8008/cluster").send().await {
        Ok(response) if response.status().is_success() => {
            match response.json::<ClusterStatus>().await {
                Ok(status) => Some(status),
                Err(e) => {
                    warn!(error = %e, "Failed to parse cluster status");
                    None
                }
            }
        }
        Ok(response) => {
            warn!(status = %response.status(), "Cluster endpoint returned non-success");
            None
        }
        Err(e) => {
            warn!(error = %e, "Failed to query cluster endpoint");
            None
        }
    }
}

/// Get the state of a specific node from cluster status
pub async fn get_node_state(node_name: &str, timeout_secs: u64) -> Option<String> {
    let status = get_cluster_status(timeout_secs).await?;

    for member in status.members {
        // Match by exact name or by partial match (node names may have suffixes)
        if member.name == node_name
            || member.name.starts_with(node_name)
            || node_name.starts_with(&member.name)
        {
            return Some(member.state);
        }
    }

    None
}

/// Check if this node is in "start failed" state
pub async fn is_start_failed(node_name: &str, timeout_secs: u64) -> bool {
    match get_node_state(node_name, timeout_secs).await {
        Some(state) => state == "start failed",
        None => false,
    }
}

/// Find the leader from cluster status
pub async fn get_leader_info(timeout_secs: u64) -> Option<ClusterMember> {
    let status = get_cluster_status(timeout_secs).await?;

    status
        .members
        .into_iter()
        .find(|m| m.role == "leader" && m.state == "running")
}

/// Trigger Patroni reinitialize via REST API
///
/// This calls POST /reinitialize which tells Patroni to:
/// 1. Stop PostgreSQL
/// 2. Clear the data directory
/// 3. Clone fresh data from the leader via pg_basebackup
///
/// Only call this after safety checks have passed!
pub async fn trigger_reinitialize(timeout_secs: u64) -> Result<bool, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // POST /reinitialize with force=true to reinit even if Patroni thinks we're OK
    let response = client
        .post("http://localhost:8008/reinitialize")
        .json(&serde_json::json!({"force": true}))
        .send()
        .await
        .map_err(|e| format!("Failed to send reinitialize request: {}", e))?;

    if response.status().is_success() {
        info!("Reinitialize request accepted by Patroni");
        Ok(true)
    } else {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "unknown".to_string());
        Err(format!(
            "Reinitialize request failed: status={}, body={}",
            status, body
        ))
    }
}

/// Check if it's safe to reinitialize based on cluster state
///
/// Returns (can_reinit, reason, local_timeline, leader_timeline)
pub async fn check_reinit_safety_from_cluster(
    node_name: &str,
    timeout_secs: u64,
) -> (bool, String, Option<i64>, Option<i64>) {
    let status = match get_cluster_status(timeout_secs).await {
        Some(s) => s,
        None => {
            return (
                false,
                "Cannot reach Patroni cluster endpoint".to_string(),
                None,
                None,
            );
        }
    };

    // Find the leader
    let leader = match status
        .members
        .iter()
        .find(|m| m.role == "leader" && m.state == "running")
    {
        Some(l) => l,
        None => {
            return (
                false,
                "No running leader found in cluster".to_string(),
                None,
                None,
            );
        }
    };

    // Find ourselves
    let us = match status.members.iter().find(|m| {
        m.name == node_name || m.name.starts_with(node_name) || node_name.starts_with(&m.name)
    }) {
        Some(u) => u,
        None => {
            return (
                false,
                "Cannot find ourselves in cluster status".to_string(),
                None,
                leader.timeline,
            );
        }
    };

    // Check we are NOT the leader
    if us.role == "leader" {
        return (
            false,
            "We are the leader - cannot reinitialize".to_string(),
            us.timeline,
            leader.timeline,
        );
    }

    // Check timelines
    let local_tl = us.timeline.unwrap_or(0);
    let leader_tl = leader.timeline.unwrap_or(0);

    if local_tl >= leader_tl {
        return (
            false,
            format!(
                "Local timeline {} >= leader timeline {} - unsafe to reinit",
                local_tl, leader_tl
            ),
            us.timeline,
            leader.timeline,
        );
    }

    (
        true,
        format!(
            "Local timeline {} < leader timeline {} - safe to reinitialize",
            local_tl, leader_tl
        ),
        us.timeline,
        leader.timeline,
    )
}

/// Perform safe reinitialize with all safety checks
///
/// Returns Ok(true) if reinitialize was triggered successfully.
/// Returns Ok(false) if reinitialize was not triggered (safety check failed).
/// Returns Err if an error occurred.
pub async fn safe_reinitialize(
    node_name: &str,
    timeout_secs: u64,
) -> Result<(bool, String, Option<i64>, Option<i64>), String> {
    let (can_reinit, reason, local_tl, leader_tl) =
        check_reinit_safety_from_cluster(node_name, timeout_secs).await;

    if !can_reinit {
        info!(
            reason = %reason,
            local_timeline = ?local_tl,
            leader_timeline = ?leader_tl,
            "Reinitialize blocked by safety check"
        );
        return Ok((false, reason, local_tl, leader_tl));
    }

    info!(
        local_timeline = ?local_tl,
        leader_timeline = ?leader_tl,
        "Safety check passed, triggering reinitialize"
    );

    match trigger_reinitialize(timeout_secs).await {
        Ok(_) => Ok((true, reason, local_tl, leader_tl)),
        Err(e) => Err(e),
    }
}
