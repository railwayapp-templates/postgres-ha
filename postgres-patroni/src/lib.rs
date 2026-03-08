//! Shared utilities for postgres-patroni binaries
//!
//! This crate provides PostgreSQL-specific utilities for:
//! - Volume and data directory path resolution
//! - SSL certificate management
//! - Patroni runner components
//! - Common helper functions

pub mod bootstrap;
pub mod health_server;
mod paths;
pub mod patroni;
mod ssl;

// Re-export path utilities
pub use paths::{pgdata, ssl_dir, volume_root, EXPECTED_VOLUME_MOUNT_PATH};

// Re-export SSL utilities
pub use ssl::{cert_expires_within, is_valid_x509v3_cert};

// Re-export common utilities
pub use common::{ConfigExt, RailwayEnv, Telemetry, TelemetryEvent};

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::info;

/// Check if Patroni mode is enabled
pub fn is_patroni_enabled() -> bool {
    bool::env_parse("PATRONI_ENABLED", false)
}

/// Run a command with sudo
pub async fn sudo_command(args: &[&str]) -> Result<()> {
    let status = Command::new("sudo")
        .args(args)
        .stdin(Stdio::null())
        .status()
        .await
        .context("Failed to run sudo command")?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("sudo command failed with status: {}", status);
    }
}

/// Add pg_stat_statements to shared_preload_libraries in a config file
fn add_pg_stat_statements(config_file: &Path) -> Result<()> {
    let content = fs::read_to_string(config_file)?;

    // Find the last shared_preload_libraries line and extract its value
    let current_libs: Option<String> = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.starts_with("shared_preload_libraries")
        })
        .next_back()
        .and_then(|line| {
            // Extract value after '=' - handles quoted ('val', "val") and unquoted (val) formats
            line.split('=').nth(1).map(|v| {
                v.trim()
                    .trim_start_matches(['\'', '"'])
                    .trim_end_matches(['\'', '"'])
                    .trim()
                    .to_string()
            })
        })
        .filter(|s| !s.is_empty());

    let new_setting = match current_libs {
        Some(libs) => format!("shared_preload_libraries = '{},pg_stat_statements'\n", libs),
        None => "shared_preload_libraries = 'pg_stat_statements'\n".to_string(),
    };

    // Append the new setting to the config file
    let mut new_content = content;
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(&new_setting);

    fs::write(config_file, new_content)?;
    Ok(())
}

/// Ensure pg_stat_statements is configured in shared_preload_libraries
/// This handles databases created before this setting was added
pub fn ensure_pg_stat_statements(pgdata: &str) -> Result<()> {
    let postgres_conf = Path::new(pgdata).join("postgresql.conf");
    let auto_conf = Path::new(pgdata).join("postgresql.auto.conf");

    // Only proceed if postgresql.conf exists (database is initialized)
    if !postgres_conf.exists() {
        return Ok(());
    }

    let conf_content = fs::read_to_string(&postgres_conf)?;

    // Skip if pg_stat_statements is already configured
    if conf_content.contains("pg_stat_statements") {
        return Ok(());
    }

    info!("Adding pg_stat_statements to shared_preload_libraries...");
    add_pg_stat_statements(&postgres_conf)?;

    // Only update auto.conf if it has shared_preload_libraries set (which would override postgresql.conf)
    // and doesn't already have pg_stat_statements
    if auto_conf.exists() {
        let auto_content = fs::read_to_string(&auto_conf)?;
        let has_shared_preload = auto_content
            .lines()
            .any(|line| line.trim().starts_with("shared_preload_libraries"));
        let has_pg_stat = auto_content.contains("pg_stat_statements");

        if has_shared_preload && !has_pg_stat {
            add_pg_stat_statements(&auto_conf)?;
        }
    }

    Ok(())
}
