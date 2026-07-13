//! Telemetry for reporting events to Railway
//!
//! Provides structured event reporting to Railway's backboard service.

use crate::config::RailwayEnv;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

/// All telemetry events that can be sent to Railway.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TelemetryEvent {
    // === PostgreSQL/Patroni Events ===
    /// Node was promoted to primary (failover occurred)
    PostgresFailover {
        node: String,
        new_role: String,
        scope: String,
    },

    /// Node rejoined cluster as replica
    PostgresRejoined { node: String, role: String, scope: String },

    /// Bootstrap process started
    BootstrapStarted { node: String, is_fresh: bool },

    /// Bootstrap completed successfully
    BootstrapCompleted {
        node: String,
        duration_ms: u64,
        users_created: Vec<String>,
    },

    /// Bootstrap failed
    BootstrapFailed {
        node: String,
        error: String,
        phase: String,
    },

    /// SSL certificates were renewed
    SslRenewed { node: String, reason: String },

    /// Health check failures detected
    HealthCheckFailed {
        node: String,
        consecutive_failures: u32,
        max_failures: u32,
    },

    /// Patroni or PostgreSQL process died
    ProcessDied {
        node: String,
        process: String,
        exit_code: Option<i32>,
    },

    /// DCS (etcd) unavailable - cluster has no leader
    /// This is a critical event: all nodes demoted, no writes possible
    DcsUnavailable { node: String, scope: String },

    /// Replica backend unavailable - no healthy replicas for read traffic
    ReplicaUnavailable { node: String, scope: String, servers: Vec<String> },

    /// Self-heal supervisor issued POST /reinitialize against the local
    /// Patroni REST API and Patroni accepted the call. Replica only; never
    /// fires against a leader. Leader must be reachable at action time so
    /// the re-clone has a source.
    SelfHealReinitTriggered {
        node: String,
        reason: String,
        attempt: u32,
    },

    /// Self-heal supervisor attempted POST /reinitialize but Patroni REST
    /// errored or was unreachable. Distinguishes "we tried but couldn't
    /// reach Patroni" from `SelfHealReinitTriggered`'s "Patroni accepted
    /// our reinit request"; without it, operators paged on a Triggered
    /// event would look for a reinit in progress and find none. The cap
    /// still ticks on failed attempts so a chronically-wedged Patroni
    /// REST escalates to `SelfHealGaveUp` instead of being retried
    /// forever.
    SelfHealReinitRequestFailed {
        node: String,
        reason: String,
        attempt: u32,
        error: String,
    },

    /// Replica returned to healthy state (running/streaming) after one or
    /// more self-heal actions. Emitted once per recovery cycle.
    SelfHealRecovered {
        node: String,
        recovered_in_secs: u64,
        attempts: u32,
    },

    /// Self-heal supervisor exhausted its per-hour attempt cap on this
    /// replica without recovery. Further action suppressed; escalates to
    /// operators via this event.
    SelfHealGaveUp {
        node: String,
        attempts: u32,
        last_reason: String,
    },

    /// Preflight wiped a non-empty data directory that had no `global/pg_control`
    /// — the debris a `pg_basebackup` killed mid-clone leaves behind — so Patroni
    /// re-clones from the leader. Replica only; fires only when a distinct member
    /// holds the leader lock (a clone source exists and it is never the primary's
    /// own data). A recurring event for one node signals a re-clone that keeps
    /// failing (e.g. a replica volume smaller than the primary).
    IncompleteCloneWiped { node: String, leader: String },

    /// Live `archive_command`/`archive_timeout` on this node diverged from
    /// what DCS says they should be, and the divergence doesn't look like
    /// Patroni's own apply-race (see reconcile.rs) — the live value isn't
    /// the untouched pre-config baseline, so it may be an operator's
    /// intentional `ALTER SYSTEM` edit. Reported rather than overwritten;
    /// an operator who set this on purpose should see why it's flagged,
    /// not have it silently reverted.
    ///
    /// Deliberately does NOT carry the live `archive_command` value itself.
    /// Unlike `ArchiveConfigForced` (only ever the known-safe empty or
    /// expected-wrapper-script baseline), this is exactly the branch where
    /// the live value can be genuine, arbitrary operator content — and this
    /// event is transmitted off-box to Railway's backboard service. Full
    /// detail is still in the `warn!` immediately preceding this event,
    /// which stays local to the node's own logs.
    ArchiveConfigDrifted {
        node: String,
        live_archive_command_matches_expected: bool,
        live_archive_timeout_secs: i64,
        expected_archive_timeout_secs: i64,
    },

    /// Live `archive_command`/`archive_timeout` on this node diverged from
    /// a correct DCS in exactly the shape Patroni's apply-race leaves
    /// behind (see reconcile.rs), and was force-corrected via
    /// `ALTER SYSTEM` + `pg_reload_conf()`. Every occurrence is a confirmed
    /// hit of the race that would otherwise have been an invisible
    /// PITR-coverage gap — fleet-wide frequency of this event measures how
    /// often the upstream Patroni bug actually fires.
    ArchiveConfigForced {
        node: String,
        live_archive_command: String,
        live_archive_timeout_secs: i64,
    },

    // === etcd Events ===
    /// etcd cluster bootstrap initiated
    EtcdBootstrap {
        node: String,
        is_leader: bool,
        cluster_size: usize,
    },

    /// Node joined etcd cluster
    EtcdNodeJoined { node: String, joined_as: String },

    /// Learner promoted to voting member
    EtcdNodePromoted { node: String },

    /// Stale member entry removed
    EtcdStaleMemberRemoved { node: String, removed_id: String },

    /// Data directory was cleared
    EtcdDataCleared { node: String, reason: String },

    /// Entering recovery mode
    EtcdRecoveryMode { node: String, reason: String },

    /// Startup attempt failed
    EtcdStartupFailed {
        node: String,
        attempt: u32,
        max_attempts: u32,
        error: String,
    },

    /// Learner promotion failed after exhausting retries
    EtcdPromotionFailed {
        node: String,
        attempts: u32,
        max_attempts: u32,
        error: String,
    },

    /// Defragmentation of the local etcd node failed
    EtcdDefragFailed { node: String, error: String },

    /// Corrupt etcd data directory wiped so the member re-clones from the cluster
    EtcdDataDirWiped { node: String, reason: String },

    /// Local etcd was unhealthy for too long while running — exiting so the
    /// platform restarts the node (it was a zombie reporting deployment SUCCESS).
    EtcdLocalUnhealthy { node: String, unhealthy_secs: u64 },

    // === HAProxy Events ===
    /// HAProxy started successfully
    HaproxyStarted { node_count: usize, single_node_mode: bool },

    /// HAProxy config generation starting
    HaproxyConfigGenerating { nodes: Vec<String> },

    // === Generic Events ===
    /// Component started
    ComponentStarted { component: String, version: String },

    /// Component error occurred
    ComponentError {
        component: String,
        error: String,
        context: String,
    },
}

impl TelemetryEvent {
    /// Get the event type name for logging/GraphQL.
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::PostgresFailover { .. } => "POSTGRES_HA_FAILOVER",
            Self::PostgresRejoined { .. } => "POSTGRES_HA_REJOINED",
            Self::BootstrapStarted { .. } => "POSTGRES_HA_BOOTSTRAP_STARTED",
            Self::BootstrapCompleted { .. } => "POSTGRES_HA_BOOTSTRAP_COMPLETED",
            Self::BootstrapFailed { .. } => "POSTGRES_HA_BOOTSTRAP_FAILED",
            Self::SslRenewed { .. } => "POSTGRES_HA_SSL_RENEWED",
            Self::HealthCheckFailed { .. } => "POSTGRES_HA_HEALTH_CHECK_FAILED",
            Self::ProcessDied { .. } => "POSTGRES_HA_PROCESS_DIED",
            Self::DcsUnavailable { .. } => "POSTGRES_HA_DCS_UNAVAILABLE",
            Self::ReplicaUnavailable { .. } => "POSTGRES_HA_REPLICA_UNAVAILABLE",
            Self::SelfHealReinitTriggered { .. } => "POSTGRES_HA_SELF_HEAL_REINIT_TRIGGERED",
            Self::SelfHealReinitRequestFailed { .. } => "POSTGRES_HA_SELF_HEAL_REINIT_REQUEST_FAILED",
            Self::SelfHealRecovered { .. } => "POSTGRES_HA_SELF_HEAL_RECOVERED",
            Self::SelfHealGaveUp { .. } => "POSTGRES_HA_SELF_HEAL_GAVE_UP",
            Self::IncompleteCloneWiped { .. } => "POSTGRES_HA_INCOMPLETE_CLONE_WIPED",
            Self::ArchiveConfigDrifted { .. } => "POSTGRES_HA_ARCHIVE_CONFIG_DRIFTED",
            Self::ArchiveConfigForced { .. } => "POSTGRES_HA_ARCHIVE_CONFIG_FORCED",
            Self::EtcdBootstrap { .. } => "ETCD_CLUSTER_BOOTSTRAP",
            Self::EtcdNodeJoined { .. } => "ETCD_NODE_JOINED",
            Self::EtcdNodePromoted { .. } => "ETCD_NODE_PROMOTED",
            Self::EtcdStaleMemberRemoved { .. } => "ETCD_STALE_MEMBER_REMOVED",
            Self::EtcdDataCleared { .. } => "ETCD_DATA_CLEARED",
            Self::EtcdRecoveryMode { .. } => "ETCD_RECOVERY_MODE",
            Self::EtcdStartupFailed { .. } => "ETCD_STARTUP_FAILED",
            Self::EtcdPromotionFailed { .. } => "ETCD_PROMOTION_FAILED",
            Self::EtcdDefragFailed { .. } => "ETCD_DEFRAG_FAILED",
            Self::EtcdDataDirWiped { .. } => "ETCD_DATA_DIR_WIPED",
            Self::EtcdLocalUnhealthy { .. } => "ETCD_LOCAL_UNHEALTHY",
            Self::HaproxyStarted { .. } => "HAPROXY_STARTED",
            Self::HaproxyConfigGenerating { .. } => "HAPROXY_CONFIG_GENERATING",
            Self::ComponentStarted { .. } => "COMPONENT_STARTED",
            Self::ComponentError { .. } => "COMPONENT_ERROR",
        }
    }

    /// Convert event to a human-readable message.
    pub fn message(&self) -> String {
        match self {
            Self::PostgresFailover { node, new_role, .. } => {
                format!("{} promoted to {}", node, new_role)
            }
            Self::PostgresRejoined { node, role, .. } => {
                format!("{} rejoined as {}", node, role)
            }
            Self::BootstrapStarted { node, is_fresh } => {
                format!("Bootstrap started on {} (fresh={})", node, is_fresh)
            }
            Self::BootstrapCompleted { node, duration_ms, .. } => {
                format!("Bootstrap completed on {} in {}ms", node, duration_ms)
            }
            Self::BootstrapFailed { node, error, phase } => {
                format!("Bootstrap failed on {} during {}: {}", node, phase, error)
            }
            Self::SslRenewed { node, reason } => {
                format!("SSL renewed on {} ({})", node, reason)
            }
            Self::HealthCheckFailed {
                node,
                consecutive_failures,
                max_failures,
            } => {
                format!(
                    "Health check failed on {} ({}/{})",
                    node, consecutive_failures, max_failures
                )
            }
            Self::ProcessDied {
                node,
                process,
                exit_code,
            } => {
                format!(
                    "{} died on {} (exit {:?})",
                    process, node, exit_code
                )
            }
            Self::DcsUnavailable { node, scope } => {
                format!(
                    "DCS unavailable - {} demoted, cluster {} has no leader (write outage)",
                    node, scope
                )
            }
            Self::ReplicaUnavailable { node, scope, servers } => {
                if servers.is_empty() {
                    format!(
                        "Replica unavailable - {} reports no healthy replicas in {} (read-only traffic affected)",
                        node, scope
                    )
                } else {
                    format!(
                        "Replica unavailable - {} reports no healthy replicas in {} (read-only traffic affected): {}",
                        node, scope, servers.join(", ")
                    )
                }
            }
            Self::SelfHealReinitTriggered { node, reason, attempt } => {
                format!(
                    "Self-heal: reinitializing {} (reason: {}, attempt {})",
                    node, reason, attempt
                )
            }
            Self::SelfHealReinitRequestFailed { node, reason, attempt, error } => {
                format!(
                    "Self-heal: reinitialize request for {} failed (reason: {}, attempt {}, error: {})",
                    node, reason, attempt, error
                )
            }
            Self::SelfHealRecovered { node, recovered_in_secs, attempts } => {
                format!(
                    "Self-heal: {} recovered after {} attempt(s) in {}s",
                    node, attempts, recovered_in_secs
                )
            }
            Self::SelfHealGaveUp { node, attempts, last_reason } => {
                format!(
                    "Self-heal: giving up on {} after {} attempts (last: {}); manual intervention required",
                    node, attempts, last_reason
                )
            }
            Self::IncompleteCloneWiped { node, leader } => {
                format!(
                    "Wiped incomplete-clone data dir on {} (non-empty, missing pg_control) — re-cloning from leader {}",
                    node, leader
                )
            }
            Self::ArchiveConfigDrifted {
                node,
                live_archive_command_matches_expected,
                live_archive_timeout_secs,
                expected_archive_timeout_secs,
            } => {
                format!(
                    "Archive config on {} doesn't match expected settings (archive_command matches expected: {}, archive_timeout={}s, expected timeout={}s) — not auto-corrected, looks like it may have been changed intentionally",
                    node, live_archive_command_matches_expected, live_archive_timeout_secs, expected_archive_timeout_secs
                )
            }
            Self::ArchiveConfigForced {
                node,
                live_archive_command,
                live_archive_timeout_secs,
            } => {
                format!(
                    "Archive config on {} was never applied by Patroni despite correct DCS (live archive_command={:?}, archive_timeout={}s) — force-corrected via ALTER SYSTEM + pg_reload_conf()",
                    node, live_archive_command, live_archive_timeout_secs
                )
            }
            Self::EtcdBootstrap {
                node,
                is_leader,
                cluster_size,
            } => {
                format!(
                    "etcd bootstrap on {} (leader={}, size={})",
                    node, is_leader, cluster_size
                )
            }
            Self::EtcdNodeJoined { node, joined_as } => {
                format!("etcd {} joined as {}", node, joined_as)
            }
            Self::EtcdNodePromoted { node } => {
                format!("etcd {} promoted to voting", node)
            }
            Self::EtcdStaleMemberRemoved { node, removed_id } => {
                format!("etcd {} removed stale member {}", node, removed_id)
            }
            Self::EtcdDataCleared { node, reason } => {
                format!("etcd {} data cleared: {}", node, reason)
            }
            Self::EtcdRecoveryMode { node, reason } => {
                format!("etcd {} recovery mode: {}", node, reason)
            }
            Self::EtcdStartupFailed {
                node,
                attempt,
                max_attempts,
                error,
            } => {
                format!(
                    "etcd {} startup failed ({}/{}): {}",
                    node, attempt, max_attempts, error
                )
            }
            Self::EtcdPromotionFailed {
                node,
                attempts,
                max_attempts,
                error,
            } => {
                format!(
                    "etcd {} promotion failed after {}/{} attempts: {}",
                    node, attempts, max_attempts, error
                )
            }
            Self::EtcdDefragFailed { node, error } => {
                format!("etcd {} defrag failed: {}", node, error)
            }
            Self::EtcdDataDirWiped { node, reason } => {
                format!("etcd {} data dir wiped: {}", node, reason)
            }
            Self::EtcdLocalUnhealthy {
                node,
                unhealthy_secs,
            } => {
                format!(
                    "etcd {} local node unhealthy for {}s while running - exiting for restart",
                    node, unhealthy_secs
                )
            }
            Self::HaproxyStarted {
                node_count,
                single_node_mode,
            } => {
                format!(
                    "HAProxy started ({} nodes, single={})",
                    node_count, single_node_mode
                )
            }
            Self::HaproxyConfigGenerating { nodes } => {
                format!("Generating HAProxy config for: {:?}", nodes)
            }
            Self::ComponentStarted { component, version } => {
                format!("{} v{} started", component, version)
            }
            Self::ComponentError {
                component,
                error,
                context,
            } => {
                format!("{} error in {}: {}", component, context, error)
            }
        }
    }
}

/// Telemetry client for sending events to Railway.
#[derive(Clone)]
pub struct Telemetry {
    client: Arc<Client>,
    endpoint: String,
    project_id: String,
    environment_id: String,
    service_id: String,
    component: String,
}

impl Telemetry {
    /// Create a new telemetry client from environment variables.
    pub fn from_env(component: &str) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            client: Arc::new(client),
            endpoint: RailwayEnv::graphql_endpoint(),
            project_id: RailwayEnv::project_id(),
            environment_id: RailwayEnv::environment_id(),
            service_id: RailwayEnv::service_id(),
            component: component.to_string(),
        }
    }

    /// Send a telemetry event synchronously.
    ///
    /// Blocks until the event is sent. Errors are logged but do not affect the caller.
    pub fn send(&self, event: TelemetryEvent) {
        let event_type = event.event_type();
        let message = event.message();
        let metadata = serde_json::to_string(&event).unwrap_or_default();

        info!(event = %event_type, "{}", message);

        let payload = json!({
            "query": "mutation telemetrySend($input: TelemetrySendInput!) { telemetrySend(input: $input) }",
            "variables": {
                "input": {
                    "command": event_type,
                    "error": message,
                    "stacktrace": metadata,
                    "projectId": self.project_id,
                    "environmentId": self.environment_id,
                    "serviceId": self.service_id,
                    "version": self.component
                }
            }
        });

        match self.client
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
        {
            Ok(resp) if resp.status().is_success() => {
                // Success - no action needed
            }
            Ok(resp) => {
                warn!("Telemetry got status {}", resp.status());
            }
            Err(e) => {
                warn!("Telemetry send failed: {}", e);
            }
        }
    }
}
