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
    /// `ETCD_ROOT_PASSWORD`: when set, this member turns on etcd authentication
    /// (root user, root role) once the cluster is healthy, and every local
    /// `etcdctl` call authenticates as root. Unset keeps the cluster open.
    pub root_password: Option<String>,
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
            root_password: std::env::var(common::ETCD_ROOT_PASSWORD_ENV)
                .ok()
                .filter(|p| !p.trim().is_empty()),
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

/// Highest member index a sibling is looked for at: covers three- and
/// five-member clusters.
const MAX_SIBLING_INDEX: u32 = 5;

/// The sibling members implied by this node's own advertised peer URL, as
/// `(name, peer_url)` pairs.
///
/// The template names its etcd services `<prefix>-<index>` and each one
/// advertises its own private domain, while its `ETCD_INITIAL_CLUSTER` names
/// the bootstrap leader alone. `http://etcd-1.railway.internal:2380` therefore
/// implies `etcd-2` … `etcd-5` at the same domain and port; that naming is how
/// the bootstrap leader reaches the members beyond that list. Empty when the
/// host's first label has no `-<index>` suffix (an IP address, a renamed
/// service).
pub(crate) fn sibling_peers(advertise_peer_urls: &str) -> Vec<(String, String)> {
    let url = advertise_peer_urls.split(',').next().unwrap_or("").trim();
    let Some((scheme, rest)) = url.split_once("://") else {
        return Vec::new();
    };
    let label_end = rest.find(['.', ':', '/']).unwrap_or(rest.len());
    let (label, tail) = rest.split_at(label_end);
    let Some((prefix, index)) = label.rsplit_once('-') else {
        return Vec::new();
    };
    if prefix.is_empty() || index.is_empty() || !index.chars().all(|c| c.is_ascii_digit()) {
        return Vec::new();
    }
    let Ok(own) = index.parse::<u32>() else {
        return Vec::new();
    };
    (1..=MAX_SIBLING_INDEX)
        .filter(|i| *i != own)
        .map(|i| {
            let name = format!("{prefix}-{i}");
            let peer_url = format!("{scheme}://{name}{tail}");
            (name, peer_url)
        })
        .collect()
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
    fn the_template_leader_derives_its_siblings_from_its_own_peer_url() {
        // The template gives etcd-1 an ETCD_INITIAL_CLUSTER that names only
        // itself; its advertised peer URL is all it has to find etcd-2/3 with.
        let siblings = sibling_peers("http://etcd-1.railway.internal:2380");
        assert_eq!(siblings.len(), 4);
        assert_eq!(
            siblings[0],
            (
                "etcd-2".to_string(),
                "http://etcd-2.railway.internal:2380".to_string()
            )
        );
        assert_eq!(
            siblings[1],
            (
                "etcd-3".to_string(),
                "http://etcd-3.railway.internal:2380".to_string()
            )
        );
        assert!(siblings.iter().all(|(name, _)| name != "etcd-1"));
    }

    #[test]
    fn siblings_keep_the_prefix_of_a_single_label_host() {
        let siblings = sibling_peers("http://t-so-e1up-17-etcd-1:2380");
        assert_eq!(siblings[0].1, "http://t-so-e1up-17-etcd-2:2380");
        assert_eq!(siblings[1].1, "http://t-so-e1up-17-etcd-3:2380");
    }

    #[test]
    fn a_member_other_than_the_first_derives_the_first_too() {
        let names: Vec<String> = sibling_peers("http://etcd-3.railway.internal:2380")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["etcd-1", "etcd-2", "etcd-4", "etcd-5"]);
    }

    #[test]
    fn a_host_without_an_index_suffix_has_no_derivable_siblings() {
        assert!(sibling_peers("http://coordinator.railway.internal:2380").is_empty());
        assert!(sibling_peers("http://etcd-1-abcd.railway.internal:2380").is_empty());
        assert!(sibling_peers("http://10.0.0.5:2380").is_empty());
        assert!(sibling_peers("http://[fd12::1]:2380").is_empty());
        assert!(sibling_peers("http://-1.railway.internal:2380").is_empty());
        assert!(sibling_peers("etcd-1.railway.internal:2380").is_empty());
        assert!(sibling_peers("").is_empty());
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
