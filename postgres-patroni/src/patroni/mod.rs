//! Patroni runner components
//!
//! This module provides the core functionality for running Patroni:
//! - Configuration parsing from environment
//! - YAML config generation
//! - Health checking
//! - Process monitoring

mod backup_watcher;
mod config;
mod credential_pin;
mod exit_history;
mod health;
mod monitoring;
mod reconcile;
pub mod rest;
mod self_heal;
mod slot_recovery;
mod yaml;

pub use backup_watcher::spawn as spawn_backup_watcher;
pub use config::{Config, Credential, RestapiAddressSource};
pub use credential_pin::{
    apply_credential_pin, credentials_from_env_requested, read_credential_pin,
    write_credential_pin, PinOutcome, PinnedCredentials, CREDENTIAL_PIN_FILE,
};
pub use health::check_health;
pub use monitoring::run_monitoring_loop;
pub use reconcile::reconcile_pgbackrest_archive_config;
pub use self_heal::spawn as spawn_self_heal_watcher;
pub use slot_recovery::spawn as spawn_slot_recovery_watcher;
pub use yaml::{generate_patroni_config, update_pg_hba_for_replication};
