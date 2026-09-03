//! Configuration for etcd bootstrap
//!
//! Handles environment variable parsing and validation.

use anyhow::{anyhow, Result};
use common::ConfigExt;
use std::collections::HashMap;
use std::time::Duration;
use tracing::warn;

/// Configuration for etcd bootstrap process
pub struct Config {
    pub data_dir: String,
    pub max_retries: u32,
    pub retry_delay: Duration,
    pub peer_wait_timeout: Duration,
    pub peer_check_interval: Duration,
    pub etcd_name: String,
    pub initial_cluster: String,
    pub initial_advertise_peer_urls: String,
}

impl Config {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            data_dir: String::env_or("ETCD_DATA_DIR", "/var/lib/etcd"),
            max_retries: u32::env_parse("ETCD_MAX_RETRIES", 60),
            retry_delay: Duration::from_secs(u64::env_parse("ETCD_RETRY_DELAY", 5)),
            peer_wait_timeout: Duration::from_secs(u64::env_parse("ETCD_PEER_WAIT_TIMEOUT", 300)),
            peer_check_interval: Duration::from_secs(u64::env_parse("ETCD_PEER_CHECK_INTERVAL", 5)),
            etcd_name: String::env_required("ETCD_NAME")?,
            initial_cluster: String::env_required("ETCD_INITIAL_CLUSTER")?,
            initial_advertise_peer_urls: String::env_required("ETCD_INITIAL_ADVERTISE_PEER_URLS")?,
        })
    }

    /// Path to the bootstrap completion marker file
    pub fn bootstrap_marker(&self) -> String {
        format!("{}/.bootstrap_complete", self.data_dir)
    }
}

/// Convert peer URL (port 2380) to client endpoint (port 2379)
pub(crate) fn peer_to_client_url(peer_url: &str) -> String {
    peer_url.replace(":2380", ":2379")
}

/// Parse the initial cluster string into a map of name -> peer_url
///
/// Format: "name1=http://host1:2380,name2=http://host2:2380"
///
/// Entries with no name or no host are skipped with a warning instead of
/// failing the whole parse. The platform templates ETCD_INITIAL_CLUSTER from
/// sibling references (`etcd-1=http://${{etcd-1.RAILWAY_PRIVATE_DOMAIN}}:2380`);
/// once a member service is deleted, its reference resolves to an empty host
/// (`etcd-1=http://:2380`) and a strict parse rejected the entire variable —
/// the node then crash-looped on config it could never fix and never reached
/// the wipe/re-join recovery in the bootstrap (observed 2026-09-01 on a
/// customer etcd-5 whose etcd-1 had been removed). Joining through the members
/// that still resolve is exactly what the removed-member recovery does for a
/// live cluster, so a stale entry is treated the same way: ignored, not fatal.
///
/// Returns an error only when NO entry is usable.
pub(crate) fn parse_initial_cluster(cluster: &str) -> Result<HashMap<String, String>> {
    let mut members = HashMap::new();
    let mut skipped: Vec<String> = Vec::new();
    for entry in cluster.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let parts: Vec<&str> = entry.splitn(2, '=').collect();
        match parts.as_slice() {
            [name, url] if !name.trim().is_empty() && peer_url_has_host(url) => {
                members.insert(name.trim().to_string(), url.trim().to_string());
            }
            _ => skipped.push(entry.to_string()),
        }
    }
    if members.is_empty() {
        return Err(anyhow!(
            "ETCD_INITIAL_CLUSTER has no usable 'name=url' entry: {cluster:?}"
        ));
    }
    if !skipped.is_empty() {
        warn!(
            skipped = ?skipped,
            kept = members.len(),
            "ETCD_INITIAL_CLUSTER carries entries with no name or no host (a member that no longer exists resolves to an empty reference); ignoring them and joining through the remaining members"
        );
    }
    Ok(members)
}

/// Whether a peer URL names a host at all. `http://:2380` and `http://` do
/// not — that is what a reference to a deleted sibling resolves to.
fn peer_url_has_host(url: &str) -> bool {
    let rest = url.trim();
    let rest = rest.split_once("://").map(|(_, r)| r).unwrap_or(rest);
    let authority = rest.split('/').next().unwrap_or("");
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => authority,
    };
    !host.trim().is_empty()
}

/// Get bootstrap leader (alphabetically first node name)
pub(crate) fn get_bootstrap_leader(initial_cluster: &str) -> Result<String> {
    let cluster = parse_initial_cluster(initial_cluster)?;
    cluster
        .keys()
        .min()
        .cloned()
        .ok_or_else(|| anyhow!("ETCD_INITIAL_CLUSTER is empty"))
}

/// Get leader's client endpoint (port 2379)
pub(crate) fn get_leader_endpoint(initial_cluster: &str, leader: &str) -> Result<Option<String>> {
    let cluster = parse_initial_cluster(initial_cluster)?;
    Ok(cluster.get(leader).map(|url| peer_to_client_url(url)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_well_formed_entry() {
        let members = parse_initial_cluster(
            "etcd-1=http://etcd-1.railway.internal:2380,etcd-2=http://etcd-2.railway.internal:2380",
        )
        .unwrap();
        assert_eq!(members.len(), 2);
        assert_eq!(members["etcd-2"], "http://etcd-2.railway.internal:2380");
    }

    #[test]
    fn skips_an_entry_whose_host_resolved_to_nothing() {
        // etcd-1's service was deleted: its RAILWAY_PRIVATE_DOMAIN reference is empty.
        let cluster = "etcd-1=http://:2380,etcd-2=http://etcd-2.railway.internal:2380,etcd-3=http://etcd-3.railway.internal:2380";
        let members = parse_initial_cluster(cluster).unwrap();
        assert_eq!(members.len(), 2);
        assert!(!members.contains_key("etcd-1"));
        // Leadership falls to the first member that still exists.
        assert_eq!(get_bootstrap_leader(cluster).unwrap(), "etcd-2");
    }

    #[test]
    fn skips_entries_with_no_name_or_no_url() {
        let members = parse_initial_cluster(
            "=http://x:2380,etcd-2=,etcd-3=http://etcd-3.railway.internal:2380,,",
        )
        .unwrap();
        assert_eq!(members.len(), 1);
        assert!(members.contains_key("etcd-3"));
    }

    #[test]
    fn fails_only_when_nothing_is_usable() {
        assert!(parse_initial_cluster("etcd-1=http://:2380").is_err());
        assert!(parse_initial_cluster("").is_err());
        assert!(parse_initial_cluster("garbage").is_err());
    }

    #[test]
    fn host_detection_handles_ports_paths_and_ipv6() {
        assert!(peer_url_has_host("http://etcd-1.railway.internal:2380"));
        assert!(peer_url_has_host("http://etcd-1.railway.internal"));
        assert!(peer_url_has_host("http://[fd12::1]:2380"));
        assert!(peer_url_has_host("http://10.0.0.5:2380/"));
        assert!(!peer_url_has_host("http://:2380"));
        assert!(!peer_url_has_host("http://"));
        assert!(!peer_url_has_host(""));
    }
}
