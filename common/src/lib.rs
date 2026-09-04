//! Shared utilities for postgres-ha components
//!
//! This crate provides common functionality used across all postgres-ha components:
//! - Structured logging initialization
//! - Environment variable parsing helpers
//! - Command execution utilities
//! - Telemetry for reporting events to Railway

pub mod command;
pub mod config;
pub mod logging;
pub mod telemetry;

pub use command::{etcd_http_health, etcdctl, etcdctl_probe, ETCD_ROOT_PASSWORD_ENV};
pub use config::{ConfigExt, RailwayEnv};
pub use logging::init_logging;
pub use telemetry::{Telemetry, TelemetryEvent};
