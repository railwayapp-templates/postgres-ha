//! Patroni runner components
//!
//! This module provides the core functionality for running Patroni:
//! - Configuration parsing from environment
//! - YAML config generation
//! - Health checking
//! - Process monitoring

mod config;
mod health;
mod monitoring;
mod reconcile;
mod yaml;

pub use config::Config;
pub use health::check_health;
pub use monitoring::run_monitoring_loop;
pub use reconcile::reconcile_pgbackrest_archive_config;
pub use yaml::{generate_patroni_config, update_pg_hba_for_replication};
