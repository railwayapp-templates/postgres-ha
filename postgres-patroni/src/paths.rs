//! Path utilities for PostgreSQL data directories
//!
//! Provides consistent path resolution for volume mounts, SSL certificates,
//! and PostgreSQL data directories.

use common::ConfigExt;
use std::env;

/// Expected volume mount path for PostgreSQL data
pub const EXPECTED_VOLUME_MOUNT_PATH: &str = "/var/lib/postgresql/data";

/// Get the volume root path from environment or default
pub fn volume_root() -> String {
    String::env_or("RAILWAY_VOLUME_MOUNT_PATH", EXPECTED_VOLUME_MOUNT_PATH)
}

/// Get the SSL directory path
pub fn ssl_dir() -> String {
    format!("{}/certs", volume_root())
}

/// Get the PGDATA path
pub fn pgdata() -> String {
    env::var("PGDATA").unwrap_or_else(|_| format!("{}/pgdata", volume_root()))
}

/// Path of the record `patroni-initdb` writes at the volume root right before
/// the initdb of a first bootstrap starts writing PGDATA. `post-bootstrap`
/// removes it when the bootstrap completes, so one found at boot without the
/// bootstrap-complete marker means PGDATA holds what an interrupted first
/// bootstrap left behind.
pub fn initdb_started_marker(volume_root: &str) -> String {
    format!("{}/.patroni_initdb_started", volume_root)
}
