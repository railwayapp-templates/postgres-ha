//! Patroni runner components
//!
//! This module provides the core functionality for running Patroni:
//! - Configuration parsing from environment
//! - YAML config generation
//! - Health checking and cluster state management
//! - Timeline divergence detection and recovery (via monitoring loop)
//! - Process monitoring

mod config;
mod health;
mod monitoring;
mod yaml;

pub use config::Config;
pub use health::{
    check_health, check_reinit_safety_from_cluster, get_cluster_status, get_leader_info,
    get_node_state, is_start_failed, safe_reinitialize, trigger_reinitialize, ClusterMember,
    ClusterStatus,
};
pub use monitoring::run_monitoring_loop;
pub use yaml::{generate_patroni_config, update_pg_hba_for_replication};
