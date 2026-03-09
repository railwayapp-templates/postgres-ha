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
    ReplicaUnavailable { node: String, scope: String },

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
            Self::EtcdBootstrap { .. } => "ETCD_CLUSTER_BOOTSTRAP",
            Self::EtcdNodeJoined { .. } => "ETCD_NODE_JOINED",
            Self::EtcdNodePromoted { .. } => "ETCD_NODE_PROMOTED",
            Self::EtcdStaleMemberRemoved { .. } => "ETCD_STALE_MEMBER_REMOVED",
            Self::EtcdDataCleared { .. } => "ETCD_DATA_CLEARED",
            Self::EtcdRecoveryMode { .. } => "ETCD_RECOVERY_MODE",
            Self::EtcdStartupFailed { .. } => "ETCD_STARTUP_FAILED",
            Self::EtcdPromotionFailed { .. } => "ETCD_PROMOTION_FAILED",
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
            Self::ReplicaUnavailable { node, scope } => {
                format!(
                    "Replica unavailable - {} reports no healthy replicas in {} (read-only traffic affected)",
                    node, scope
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
