//! etcd cluster operations
//!
//! Functions for managing cluster membership, starting etcd, and health checking.

use crate::config::{parse_initial_cluster, peer_to_client_url, Config};
use anyhow::{anyhow, Context, Result};
use common::{etcd_http_health, etcdctl, etcdctl_probe, Telemetry, TelemetryEvent};
use std::fmt;
use std::path::Path;
use std::process::Stdio;
use tokio::fs;
use tokio::process::Command;
use tracing::{info, warn};

/// Information about an etcd cluster member
#[derive(Debug, Clone)]
pub struct MemberInfo {
    pub id: String,
    pub name: String,
    pub peer_url: String,
    pub is_learner: bool,
}

/// Parse `etcdctl member list -w simple` output:
/// `ID, STATUS, NAME, PEER ADDRS, CLIENT ADDRS, IS LEARNER`.
/// A member that has been added but not started yet has an empty name.
pub fn parse_member_list(output: &str) -> Result<Vec<MemberInfo>> {
    output
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
        .collect()
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

    parse_member_list(&output)
}

/// Canonical form of a peer URL for comparison: no surrounding whitespace,
/// no trailing slash, lower case (scheme and host are case-insensitive).
fn normalize_peer_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// How this node recognises its own entry in `member list`.
///
/// The member NAME is not an identity: nothing stops two services from
/// carrying the same `ETCD_NAME`, and etcd accepts both. The peer URL is,
/// because etcd refuses to add a second member with a peer URL already in
/// use, and each service advertises its own private domain. So "self" is the
/// member whose ID etcd returned when this node added itself, else the member
/// that carries this node's advertised peer URL. The name is used only when no
/// peer URL is configured at all.
#[derive(Debug, Clone)]
pub struct SelfIdentity {
    pub name: String,
    /// Normalized peer URLs from `ETCD_INITIAL_ADVERTISE_PEER_URLS`.
    pub peer_urls: Vec<String>,
    /// The member ID `etcdctl member add` printed for this node, when this
    /// process added it.
    pub member_id: Option<String>,
}

/// Which key matched this node's entry in the member list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedBy {
    MemberId,
    PeerUrl,
    Name,
}

impl fmt::Display for MatchedBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            MatchedBy::MemberId => "member id",
            MatchedBy::PeerUrl => "peer url",
            MatchedBy::Name => "name (no peer url configured)",
        })
    }
}

impl SelfIdentity {
    pub fn new(name: &str, advertise_peer_urls: &str, member_id: Option<String>) -> Self {
        Self {
            name: name.to_string(),
            peer_urls: advertise_peer_urls
                .split(',')
                .map(normalize_peer_url)
                .filter(|u| !u.is_empty())
                .collect(),
            member_id: member_id.filter(|id| !id.trim().is_empty()),
        }
    }

    pub fn from_config(config: &Config, member_id: Option<String>) -> Self {
        Self::new(
            &config.etcd_name,
            &config.initial_advertise_peer_urls,
            member_id,
        )
    }

    /// Whether `peer_url` (as `member list` prints it, possibly several
    /// URLs) is one this node advertises.
    fn advertises(&self, peer_url: &str) -> bool {
        peer_url
            .split(',')
            .map(normalize_peer_url)
            .any(|u| self.peer_urls.contains(&u))
    }

    /// The peer URL this node advertises, for messages.
    fn display_peer_url(&self) -> &str {
        self.peer_urls
            .first()
            .map(String::as_str)
            .unwrap_or("(none)")
    }
}

/// Find this node's own entry in a member list: by the member ID etcd gave
/// this node, else by its advertised peer URL. By name only when no peer URL
/// is configured; a peer URL that is configured but absent means this node is
/// not a member, whatever other member shares its name.
pub fn find_self<'a>(
    members: &'a [MemberInfo],
    me: &SelfIdentity,
) -> Option<(&'a MemberInfo, MatchedBy)> {
    if let Some(id) = &me.member_id {
        if let Some(m) = members.iter().find(|m| m.id.eq_ignore_ascii_case(id)) {
            return Some((m, MatchedBy::MemberId));
        }
    }
    if !me.peer_urls.is_empty() {
        return members
            .iter()
            .find(|m| me.advertises(&m.peer_url))
            .map(|m| (m, MatchedBy::PeerUrl));
    }
    members
        .iter()
        .find(|m| !m.name.is_empty() && m.name == me.name)
        .map(|m| (m, MatchedBy::Name))
}

/// Another member that carries this node's `ETCD_NAME` at a different peer
/// URL. `voting_only` restricts the search to voting members. Never reports
/// anything when this node has no peer URL configured (the name is then the
/// only identity there is).
pub fn member_sharing_my_name<'a>(
    members: &'a [MemberInfo],
    me: &SelfIdentity,
    voting_only: bool,
) -> Option<&'a MemberInfo> {
    if me.peer_urls.is_empty() {
        return None;
    }
    members.iter().find(|m| {
        m.name == me.name && !me.advertises(&m.peer_url) && !(voting_only && m.is_learner)
    })
}

/// First DNS label of a peer URL's host: `http://etcd-3.railway.internal:2380`
/// gives `etcd-3`. On Railway that is the service's own name.
fn host_label(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or("");
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
    let label = host.split('.').next().unwrap_or("").trim();
    if label.is_empty() || label.starts_with('[') || label.chars().all(|c| c.is_ascii_digit()) {
        None
    } else {
        Some(label.to_string())
    }
}

/// The fix sentence: name the service's own name when its domain shows it.
fn rename_advice(me: &SelfIdentity) -> String {
    match host_label(me.display_peer_url()) {
        Some(label) if label != me.name => {
            format!("set this service's ETCD_NAME to its own name ({label}) and redeploy")
        }
        _ => "set this service's ETCD_NAME to a name no other member uses and redeploy".to_string(),
    }
}

/// The message a node logs when it refuses to join under a name a voting
/// member already holds.
pub fn name_collision_message(me: &SelfIdentity, other: &MemberInfo) -> String {
    format!(
        "ETCD_NAME is {name} but the voting member {name} already exists at {other_url}; this service advertises {my_url}. \
         Two members with one name cannot tell themselves apart: this node would join as a learner that is never promoted, and the cluster would lose its fault tolerance. \
         To recover: {advice}.",
        name = me.name,
        other_url = other.peer_url,
        my_url = me.display_peer_url(),
        advice = rename_advice(me),
    )
}

/// The warning a running member logs when it already shares its name with
/// another member (joined before the join-time refusal existed).
pub fn shared_name_message(me: &SelfIdentity, other: &MemberInfo) -> String {
    format!(
        "ETCD_NAME is {name} but the member {name} at {other_url} uses the same name; this service advertises {my_url}. \
         This node identifies itself by its peer URL, so it keeps working, but the duplicate name makes the member list ambiguous. \
         To recover: {advice}.",
        name = me.name,
        other_url = other.peer_url,
        my_url = me.display_peer_url(),
        advice = rename_advice(me),
    )
}

/// Refusal to join under a name a voting member already holds. Not retried:
/// the configuration has to change.
#[derive(Debug)]
pub struct NameCollision(pub String);

impl fmt::Display for NameCollision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NameCollision {}

/// Whether an error is a [`NameCollision`].
pub fn is_name_collision(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NameCollision>().is_some()
}

/// Fail when a voting member already holds this node's name at another peer
/// URL. Reports a ComponentError so the refusal is visible outside the logs.
pub fn refuse_name_collision(
    members: &[MemberInfo],
    me: &SelfIdentity,
    telemetry: &Telemetry,
) -> Result<()> {
    if let Some(other) = member_sharing_my_name(members, me, true) {
        let message = name_collision_message(me, other);
        telemetry.send(TelemetryEvent::ComponentError {
            component: "etcd".to_string(),
            error: message.clone(),
            context: "ETCD_NAME already used by a voting member".to_string(),
        });
        return Err(anyhow::Error::new(NameCollision(message)));
    }
    Ok(())
}

/// The member ID in `etcdctl member add` output:
/// `Member a1331e7952d5ee8a added to cluster 1c2b...`.
pub fn parse_added_member_id(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        if words.next()? != "Member" {
            return None;
        }
        let id = words.next()?;
        if words.next()? != "added" {
            return None;
        }
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(id.to_ascii_lowercase())
        } else {
            None
        }
    })
}

/// The `ETCD_INITIAL_CLUSTER=...` value in `etcdctl member add` output.
pub fn parse_added_initial_cluster(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        line.split("ETCD_INITIAL_CLUSTER=")
            .nth(1)
            .map(|s| s.trim().trim_matches('"').to_string())
            .filter(|c| !c.is_empty())
    })
}

/// Check cluster health via localhost or voting member
pub async fn check_cluster_health(initial_cluster: &str) -> Result<bool> {
    if etcd_http_health("127.0.0.1:2379").await? {
        return Ok(true);
    }

    if let Some(endpoint) = get_voting_member_endpoint(initial_cluster).await? {
        return etcd_http_health(&endpoint).await;
    }

    Ok(false)
}

/// Find a voting member endpoint
pub async fn get_voting_member_endpoint(initial_cluster: &str) -> Result<Option<String>> {
    let cluster = parse_initial_cluster(initial_cluster)?;

    for (_name, peer_url) in cluster.iter() {
        let client_endpoint = peer_to_client_url(peer_url);
        if etcdctl_probe(&[
            "member",
            "list",
            &format!("--endpoints={}", client_endpoint),
        ])
        .await?
        {
            return Ok(Some(client_endpoint));
        }
    }

    Ok(None)
}

/// Remove the member entry that belongs to this node (matched by peer URL,
/// never by a name another member may share).
pub async fn remove_stale_self(
    endpoint: &str,
    me: &SelfIdentity,
    telemetry: &Telemetry,
) -> Result<()> {
    info!("Checking for stale member entry...");

    let members = get_member_list(endpoint).await?;

    let Some((member, matched_by)) = find_self(&members, me) else {
        info!("No stale member entry found");
        return Ok(());
    };

    info!(id = %member.id, matched_by = %matched_by, "Removing stale member entry");
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
                node: me.name.clone(),
                removed_id: member.id.clone(),
            });
            info!("Stale member removed");
            Ok(())
        }
        Err(e) => {
            telemetry.send(TelemetryEvent::ComponentError {
                component: "etcd".to_string(),
                error: e.to_string(),
                context: format!("removing stale member {}", member.id),
            });
            Err(e)
        }
    }
}

/// Build the `ETCD_INITIAL_CLUSTER` value for a joining node from a member list.
pub fn current_cluster_string(members: &[MemberInfo], me: &SelfIdentity) -> String {
    let mut cluster_parts: Vec<String> = members
        .iter()
        .filter(|m| !m.name.is_empty() && !m.peer_url.is_empty())
        .map(|m| format!("{}={}", m.name, m.peer_url))
        .collect();

    if !members.iter().any(|m| me.advertises(&m.peer_url)) {
        cluster_parts.push(format!("{}={}", me.name, me.display_peer_url()));
    }

    cluster_parts.join(",")
}

/// Build current cluster membership for joining node
pub async fn get_current_cluster(endpoint: &str, me: &SelfIdentity) -> Result<String> {
    let members = get_member_list(endpoint).await?;
    Ok(current_cluster_string(&members, me))
}

/// Where a joining node stands after registering itself.
pub struct Joined {
    pub initial_cluster: String,
    /// This node's member ID, when known.
    pub member_id: Option<String>,
}

/// `etcdctl member add <name> --learner`, returning the cluster string to
/// start with and the new member's ID.
pub async fn add_as_learner(config: &Config, endpoint: &str, me: &SelfIdentity) -> Result<Joined> {
    let output = etcdctl(&[
        "member",
        "add",
        &config.etcd_name,
        "--learner",
        &format!("--peer-urls={}", config.initial_advertise_peer_urls),
        &format!("--endpoints={}", endpoint),
    ])
    .await?;

    let member_id = parse_added_member_id(&output);
    match &member_id {
        Some(id) => info!(id = %id, "Added as learner"),
        None => {
            warn!("Added as learner, but etcdctl printed no member ID; finding self by peer URL")
        }
    }

    let initial_cluster = match parse_added_initial_cluster(&output) {
        Some(c) => c,
        None => {
            info!("Extracting cluster from member list");
            get_current_cluster(endpoint, me).await?
        }
    };

    Ok(Joined {
        initial_cluster,
        member_id,
    })
}

/// Add this node to an existing cluster as a learner
pub async fn add_self_to_cluster(
    config: &Config,
    leader: &str,
    leader_endpoint: &str,
    telemetry: &Telemetry,
) -> Result<Joined> {
    // Use the node's own advertise URL directly instead of looking up in initial_cluster.
    // This is necessary for learner bootstrap where ETCD_INITIAL_CLUSTER only contains the leader.
    let me = SelfIdentity::from_config(config, None);

    info!(node = %config.etcd_name, via = %leader_endpoint, "Adding self as learner");

    let members = get_member_list(leader_endpoint).await?;
    refuse_name_collision(&members, &me, telemetry)?;

    // Check if already a member
    if let Some((member, matched_by)) = find_self(&members, &me) {
        let has_data = has_local_data(&config.data_dir).await?;

        if !has_data {
            warn!(id = %member.id, matched_by = %matched_by, "Registered as member but no local data - removing stale entry");
            remove_stale_self(leader_endpoint, &me, telemetry).await?;

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
        } else {
            info!(id = %member.id, matched_by = %matched_by, "Already a member with local data");
            return Ok(Joined {
                initial_cluster: current_cluster_string(&members, &me),
                member_id: Some(member.id.clone()),
            });
        }
    }

    // Add as learner
    let joined = match add_as_learner(config, leader_endpoint, &me).await {
        Ok(joined) => joined,
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
    Ok(joined)
}

/// Result of a promotion attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromoteOutcome {
    /// This call promoted the node.
    Promoted,
    /// The node's own entry is already a voting member.
    AlreadyVoting,
}

/// Promote self from learner to voting member. Finds this node's entry by
/// member ID or peer URL (see [`find_self`]); fails when the entry is not in
/// the member list, so the caller retries instead of assuming success.
pub async fn promote_self(
    initial_cluster: &str,
    me: &SelfIdentity,
    telemetry: &Telemetry,
) -> Result<PromoteOutcome> {
    let endpoint = get_voting_member_endpoint(initial_cluster)
        .await?
        .ok_or_else(|| anyhow!("Could not find voting member endpoint"))?;

    let members = get_member_list(&endpoint).await?;
    let (member, matched_by) = find_self(&members, me).ok_or_else(|| {
        anyhow!(
            "this node ({} at {}) is not in the member list returned by {}",
            me.name,
            me.display_peer_url(),
            endpoint
        )
    })?;
    let member_id = member.id.clone();

    if !member.is_learner {
        info!(id = %member_id, matched_by = %matched_by, "Already a voting member");
        return Ok(PromoteOutcome::AlreadyVoting);
    }

    info!(id = %member_id, matched_by = %matched_by, via = %endpoint, "Promoting from learner to voting member");

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
                node: me.name.clone(),
            });
            Ok(PromoteOutcome::Promoted)
        }
        Err(e) => {
            if e.to_string().contains("is not a learner") {
                info!("Already a voting member");
                Ok(PromoteOutcome::AlreadyVoting)
            } else {
                telemetry.send(TelemetryEvent::ComponentError {
                    component: "etcd".to_string(),
                    error: e.to_string(),
                    context: format!("promoting {} to voting member", me.name),
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
        Err(e) => Err(anyhow::anyhow!(
            "Failed to read WAL directory entries: {}",
            e
        )),
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

    let mut cmd = Command::new("/usr/local/bin/etcd");

    // Apply wrapper defaults as CLI flags only when the operator hasn't set the
    // corresponding ETCD_* env var. etcd 3.6 treats a flag that is also set via
    // its env var as a fatal conflict ("conflicting environment variable ... is
    // shadowed by corresponding command-line flag") and refuses to start. A user
    // tuning a documented setting like ETCD_AUTO_COMPACTION_RETENTION would
    // otherwise crash the node, so let the env var win and skip our default.
    apply_default_flag(
        &mut cmd,
        "--auto-compaction-retention",
        "ETCD_AUTO_COMPACTION_RETENTION",
        "1",
    );
    apply_default_flag(&mut cmd, "--snapshot-count", "ETCD_SNAPSHOT_COUNT", "1000");
    apply_default_flag(&mut cmd, "--max-learners", "ETCD_MAX_LEARNERS", "2");

    withhold_entrypoint_secrets(&mut cmd);

    // stderr is piped (not inherited) so the supervisor can watch etcd's log
    // stream for the unrecoverable raft-log corruption panic; the caller tees it
    // back to its own stderr so container logs are unaffected.
    let child = cmd
        .env("ETCD_INITIAL_CLUSTER", initial_cluster)
        .env("ETCD_INITIAL_CLUSTER_STATE", initial_cluster_state)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start etcd")?;

    Ok(child)
}

/// Keep the entrypoint's own secrets out of the etcd child's environment.
///
/// etcd reads every `ETCD_*` variable as a flag and, for one it does not know,
/// logs `unrecognized environment variable` with the whole `NAME=value` pair
/// verbatim at warn level (etcd 3.6.6 `pkg/flags/flag.go`, `verifyEnv`). The
/// root password is such a variable: it is the entrypoint's credential for
/// `etcdctl` and the gateway, never an etcd setting, so an inherited copy only
/// ends up printed in the member's log at every boot.
fn withhold_entrypoint_secrets(cmd: &mut Command) {
    cmd.env_remove(common::ETCD_ROOT_PASSWORD_ENV);
}

/// Apply a wrapper default as a CLI flag, but only when the operator has not set
/// the corresponding `ETCD_*` environment variable.
///
/// etcd 3.6 fatally rejects startup when a flag is supplied that is also set via
/// its matching env var, so hardcoding the flag unconditionally would crash any
/// node whose owner tuned the setting through the documented env var. Deferring
/// to the env var keeps operator configuration authoritative while still
/// providing a sane default when it is absent.
fn apply_default_flag(cmd: &mut Command, flag: &str, env_key: &str, default: &str) {
    match std::env::var(env_key) {
        Ok(value) if !value.trim().is_empty() => {
            info!(flag = %flag, env = %env_key, value = %value, "Operator set env var; deferring to it instead of default flag");
        }
        _ => {
            cmd.arg(format!("{}={}", flag, default));
        }
    }
}

#[cfg(test)]
mod entrypoint_secret_tests {
    use super::withhold_entrypoint_secrets;
    use std::ffi::OsStr;
    use tokio::process::Command;

    #[test]
    fn the_root_password_is_withheld_from_the_etcd_child() {
        let mut cmd = Command::new("/usr/local/bin/etcd");
        cmd.env("ETCD_INITIAL_CLUSTER", "etcd-1=http://etcd-1:2380");
        withhold_entrypoint_secrets(&mut cmd);
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(
            envs.contains(&(OsStr::new(common::ETCD_ROOT_PASSWORD_ENV), None)),
            "ETCD_ROOT_PASSWORD must be removed from the child's environment: {envs:?}"
        );
        assert!(envs.contains(&(
            OsStr::new("ETCD_INITIAL_CLUSTER"),
            Some(OsStr::new("etcd-1=http://etcd-1:2380"))
        )));
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    /// `member list -w simple` of onyx-staging/staging on 2026-10-13: the
    /// service etcd-3 runs with ETCD_NAME=etcd-2, so two members carry the
    /// name etcd-2. The real etcd-2 (a voter) is listed first; etcd-3's own
    /// entry is the learner at its own private domain.
    const ONYX_STAGING: &str = "\
eae04cfdcb9f0b03, started, etcd-2, http://etcd-2.railway.internal:2380, http://etcd-2.railway.internal:2379, false
1f4c1e9b3a2d7c60, started, etcd-1, http://etcd-1.railway.internal:2380, http://etcd-1.railway.internal:2379, false
a1331e7952d5ee8a, started, etcd-2, http://etcd-3.railway.internal:2380, http://etcd-3.railway.internal:2379, true
";

    fn etcd3_named_etcd2() -> SelfIdentity {
        SelfIdentity::new("etcd-2", "http://etcd-3.railway.internal:2380", None)
    }

    #[test]
    fn parses_member_list_including_learner_flag() {
        let members = parse_member_list(ONYX_STAGING).unwrap();
        assert_eq!(members.len(), 3);
        assert_eq!(members[2].id, "a1331e7952d5ee8a");
        assert_eq!(members[2].name, "etcd-2");
        assert_eq!(members[2].peer_url, "http://etcd-3.railway.internal:2380");
        assert!(members[2].is_learner);
        assert!(!members[0].is_learner);
    }

    #[test]
    fn self_is_found_by_peer_url_when_two_members_share_the_name() {
        let members = parse_member_list(ONYX_STAGING).unwrap();
        let (me, by) = find_self(&members, &etcd3_named_etcd2()).unwrap();
        // Not the voting etcd-2 listed first: this node's own learner entry.
        assert_eq!(me.id, "a1331e7952d5ee8a");
        assert!(me.is_learner);
        assert_eq!(by, MatchedBy::PeerUrl);
    }

    #[test]
    fn the_real_etcd2_still_finds_its_own_entry() {
        let members = parse_member_list(ONYX_STAGING).unwrap();
        let real = SelfIdentity::new("etcd-2", "http://etcd-2.railway.internal:2380", None);
        let (me, by) = find_self(&members, &real).unwrap();
        assert_eq!(me.id, "eae04cfdcb9f0b03");
        assert_eq!(by, MatchedBy::PeerUrl);
    }

    #[test]
    fn member_id_from_member_add_wins() {
        let members = parse_member_list(ONYX_STAGING).unwrap();
        let me = SelfIdentity::new(
            "etcd-2",
            "http://etcd-3.railway.internal:2380",
            Some("A1331E7952D5EE8A".to_string()),
        );
        let (m, by) = find_self(&members, &me).unwrap();
        assert_eq!(m.id, "a1331e7952d5ee8a");
        assert_eq!(by, MatchedBy::MemberId);
    }

    #[test]
    fn a_member_id_no_longer_listed_falls_back_to_the_peer_url() {
        let members = parse_member_list(ONYX_STAGING).unwrap();
        let me = SelfIdentity::new(
            "etcd-2",
            "http://etcd-3.railway.internal:2380",
            Some("deadbeefdeadbeef".to_string()),
        );
        let (m, by) = find_self(&members, &me).unwrap();
        assert_eq!(m.id, "a1331e7952d5ee8a");
        assert_eq!(by, MatchedBy::PeerUrl);
    }

    #[test]
    fn a_configured_peer_url_that_is_absent_is_not_a_member_whatever_the_name() {
        let members = parse_member_list(ONYX_STAGING).unwrap();
        let me = SelfIdentity::new("etcd-2", "http://etcd-4.railway.internal:2380", None);
        assert!(find_self(&members, &me).is_none());
    }

    #[test]
    fn name_is_the_fallback_only_when_no_peer_url_is_configured() {
        let members = parse_member_list(ONYX_STAGING).unwrap();
        let me = SelfIdentity::new("etcd-1", "", None);
        let (m, by) = find_self(&members, &me).unwrap();
        assert_eq!(m.id, "1f4c1e9b3a2d7c60");
        assert_eq!(by, MatchedBy::Name);
        // And no collision is ever claimed without a peer URL to compare.
        assert!(member_sharing_my_name(&members, &me, true).is_none());
    }

    #[test]
    fn peer_url_comparison_ignores_case_and_trailing_slash() {
        let members = parse_member_list(ONYX_STAGING).unwrap();
        let me = SelfIdentity::new("x", " HTTP://etcd-3.Railway.Internal:2380/ ", None);
        assert_eq!(find_self(&members, &me).unwrap().0.id, "a1331e7952d5ee8a");
    }

    #[test]
    fn unstarted_member_is_found_by_peer_url() {
        // Added, not started yet: etcd prints no name and no client URL.
        let out = "a1331e7952d5ee8a, unstarted, , http://etcd-3.railway.internal:2380, , true\n";
        let members = parse_member_list(out).unwrap();
        let (m, _) = find_self(&members, &etcd3_named_etcd2()).unwrap();
        assert!(m.is_learner);
    }

    #[test]
    fn joining_under_a_voting_members_name_is_refused_with_the_fix() {
        // Before etcd-3 (ETCD_NAME=etcd-2) adds itself: etcd-1 and etcd-2 vote.
        let before_join = "\
eae04cfdcb9f0b03, started, etcd-2, http://etcd-2.railway.internal:2380, http://etcd-2.railway.internal:2379, false
1f4c1e9b3a2d7c60, started, etcd-1, http://etcd-1.railway.internal:2380, http://etcd-1.railway.internal:2379, false
";
        let members = parse_member_list(before_join).unwrap();
        let me = etcd3_named_etcd2();
        let other = member_sharing_my_name(&members, &me, true).unwrap();
        assert_eq!(other.id, "eae04cfdcb9f0b03");

        let err = refuse_name_collision(&members, &me, &Telemetry::from_env("etcd")).unwrap_err();
        assert!(is_name_collision(&err));
        let msg = err.to_string();
        assert!(
            msg.starts_with(
                "ETCD_NAME is etcd-2 but the voting member etcd-2 already exists at http://etcd-2.railway.internal:2380;"
            ),
            "{msg}"
        );
        assert!(
            msg.contains("this service advertises http://etcd-3.railway.internal:2380"),
            "{msg}"
        );
        assert!(
            msg.ends_with(
                "To recover: set this service's ETCD_NAME to its own name (etcd-3) and redeploy."
            ),
            "{msg}"
        );
    }

    #[test]
    fn a_learner_sharing_the_name_is_not_a_join_collision() {
        let out = "a1331e7952d5ee8a, started, etcd-2, http://etcd-3.railway.internal:2380, http://etcd-3.railway.internal:2379, true\n";
        let members = parse_member_list(out).unwrap();
        let real = SelfIdentity::new("etcd-2", "http://etcd-2.railway.internal:2380", None);
        assert!(member_sharing_my_name(&members, &real, true).is_none());
        assert!(member_sharing_my_name(&members, &real, false).is_some());
        assert!(refuse_name_collision(&members, &real, &Telemetry::from_env("etcd")).is_ok());
    }

    #[test]
    fn rejoining_under_its_own_name_and_url_is_not_a_collision() {
        let members = parse_member_list(ONYX_STAGING).unwrap();
        let etcd1 = SelfIdentity::new("etcd-1", "http://etcd-1.railway.internal:2380", None);
        assert!(refuse_name_collision(&members, &etcd1, &Telemetry::from_env("etcd")).is_ok());
    }

    #[test]
    fn rename_advice_without_a_usable_host_label() {
        let me = SelfIdentity::new("etcd-2", "http://10.0.0.5:2380", None);
        let other = MemberInfo {
            id: "1".into(),
            name: "etcd-2".into(),
            peer_url: "http://10.0.0.6:2380".into(),
            is_learner: false,
        };
        assert!(name_collision_message(&me, &other).ends_with(
            "To recover: set this service's ETCD_NAME to a name no other member uses and redeploy."
        ));
    }

    #[test]
    fn parses_member_id_from_member_add_output() {
        let out = "Member a1331e7952d5ee8a added to cluster 6d6c4f4a1f0e2b77\n\n\
ETCD_NAME=\"etcd-2\"\n\
ETCD_INITIAL_CLUSTER=\"etcd-1=http://etcd-1.railway.internal:2380,etcd-2=http://etcd-3.railway.internal:2380\"\n\
ETCD_INITIAL_ADVERTISE_PEER_URLS=\"http://etcd-3.railway.internal:2380\"\n\
ETCD_INITIAL_CLUSTER_STATE=\"existing\"\n";
        assert_eq!(
            parse_added_member_id(out).as_deref(),
            Some("a1331e7952d5ee8a")
        );
        assert_eq!(
            parse_added_initial_cluster(out).as_deref(),
            Some("etcd-1=http://etcd-1.railway.internal:2380,etcd-2=http://etcd-3.railway.internal:2380")
        );
        assert_eq!(parse_added_member_id("Error: something"), None);
        assert_eq!(parse_added_member_id("Member list follows"), None);
    }

    #[test]
    fn cluster_string_adds_self_by_peer_url_not_by_name() {
        // The voting etcd-2 shares our name; we are still missing by peer URL.
        let out = "eae04cfdcb9f0b03, started, etcd-2, http://etcd-2.railway.internal:2380, http://etcd-2.railway.internal:2379, false\n";
        let members = parse_member_list(out).unwrap();
        assert_eq!(
            current_cluster_string(&members, &etcd3_named_etcd2()),
            "etcd-2=http://etcd-2.railway.internal:2380,etcd-2=http://etcd-3.railway.internal:2380"
        );
    }
}
