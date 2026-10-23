//! Shared utilities for postgres-patroni binaries
//!
//! This crate provides PostgreSQL-specific utilities for:
//! - Volume and data directory path resolution
//! - SSL certificate management
//! - Patroni runner components
//! - Common helper functions

pub mod bootstrap;
pub mod health_server;
pub mod major_upgrade;
pub mod orphan_slots;
mod paths;
pub mod patroni;
pub mod pgbackrest;
mod ssl;
pub mod standalone_pitr;
pub mod volume_lock;
pub mod wal_archive;

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

/// The libraries the image itself preloads on every volume: pg_stat_statements
/// because the dashboard's Stats tab reads its view, and pgaudit because it
/// refuses to load any other way (its `_PG_init` errors outside
/// shared_preload_libraries, so `CREATE EXTENSION pgaudit` — the dashboard's
/// Install button — only works when it is already here) while costing nothing
/// until `pgaudit.log` is set. Standalone mode writes them into the config
/// files below; Patroni mode seeds the same list into DCS at bootstrap
/// (patroni/yaml.rs) and reconciles it into DCS on every node boot
/// (patroni/reconcile.rs) for clusters bootstrapped before.
pub const IMAGE_PRELOAD_LIBRARIES: [&str; 2] = ["pg_stat_statements", "pgaudit"];

fn is_preload_assignment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("shared_preload_libraries")
        .map(|rest| rest.trim_start().starts_with('='))
        .unwrap_or(false)
}

/// The effective value of the LAST shared_preload_libraries assignment in a
/// config file's contents, as bare library names: quotes of both kinds and
/// whitespace stripped, trailing comment dropped. Empty when there is no
/// assignment. The quote stripping is what makes ALTER SYSTEM's single-string
/// form (`'"a,b"'`, which Postgres reads as ONE library named `a,b`) come out
/// as `a` and `b` here.
fn preload_libraries(contents: &str) -> Vec<String> {
    contents
        .lines()
        .filter(|line| is_preload_assignment(line))
        .next_back()
        .map(|line| {
            let value = line.splitn(2, '=').nth(1).unwrap_or("");
            let value = value.split('#').next().unwrap_or("");
            value
                .split(',')
                .map(|lib| {
                    lib.chars()
                        .filter(|c| !c.is_whitespace() && *c != '\'' && *c != '"')
                        .collect::<String>()
                })
                .filter(|lib| !lib.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Append an assignment that keeps the file's current list and adds `lib`.
fn add_preload_library(config_file: &Path, lib: &str) -> Result<()> {
    let mut content = fs::read_to_string(config_file)?;
    let mut libs = preload_libraries(&content);
    libs.push(lib.to_string());
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&format!(
        "shared_preload_libraries = '{}'\n",
        libs.join(",")
    ));
    fs::write(config_file, content)?;
    Ok(())
}

/// Ensure every image preload library is in shared_preload_libraries (standalone
/// mode). postgresql.conf always carries the list (init-ssl.sh wrote it; a volume
/// initialized before a library was added gets it appended here).
/// postgresql.auto.conf is only touched when it already assigns the GUC: an
/// ALTER SYSTEM there overrides postgresql.conf as a whole, so an operator list
/// without the image's libraries would silently drop them. Membership is
/// checked per list element — `pgaudit.log = 'ddl'` on another line of
/// auto.conf must not read as pgaudit being preloaded.
pub fn ensure_image_preload_libraries(pgdata: &str) -> Result<()> {
    let postgres_conf = Path::new(pgdata).join("postgresql.conf");
    let auto_conf = Path::new(pgdata).join("postgresql.auto.conf");

    // Only proceed if postgresql.conf exists (database is initialized)
    if !postgres_conf.exists() {
        return Ok(());
    }

    for (file, only_when_assigned) in [(&postgres_conf, false), (&auto_conf, true)] {
        if !file.exists() {
            continue;
        }
        if only_when_assigned && !fs::read_to_string(file)?.lines().any(is_preload_assignment) {
            continue;
        }
        for lib in IMAGE_PRELOAD_LIBRARIES {
            let current = preload_libraries(&fs::read_to_string(file)?);
            if current.iter().any(|l| l == lib) {
                continue;
            }
            info!(library = lib, file = %file.display(), "Adding image preload library to shared_preload_libraries");
            add_preload_library(file, lib)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod preload_tests {
    use super::*;

    fn pgdata(conf: &str, auto: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("postgresql.conf"), conf).unwrap();
        if let Some(auto) = auto {
            fs::write(dir.path().join("postgresql.auto.conf"), auto).unwrap();
        }
        dir
    }

    fn value(dir: &tempfile::TempDir, file: &str) -> Vec<String> {
        preload_libraries(&fs::read_to_string(dir.path().join(file)).unwrap())
    }

    #[test]
    fn fresh_volume_is_left_alone() {
        let dir = pgdata(
            "#shared_preload_libraries = ''\nshared_preload_libraries = 'pg_stat_statements,pgaudit'\n",
            Some("max_connections = '500'\n"),
        );
        ensure_image_preload_libraries(dir.path().to_str().unwrap()).unwrap();
        let conf = fs::read_to_string(dir.path().join("postgresql.conf")).unwrap();
        assert_eq!(conf.matches("shared_preload_libraries").count(), 2);
        let auto = fs::read_to_string(dir.path().join("postgresql.auto.conf")).unwrap();
        assert!(!auto.contains("shared_preload_libraries"));
    }

    #[test]
    fn legacy_volume_gets_the_missing_library_appended_to_its_list() {
        let dir = pgdata(
            "shared_preload_libraries = 'pg_stat_statements'\t# comment\n",
            None,
        );
        ensure_image_preload_libraries(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(
            value(&dir, "postgresql.conf"),
            ["pg_stat_statements", "pgaudit"]
        );
    }

    #[test]
    fn operator_library_is_kept_ahead_of_the_image_ones() {
        let dir = pgdata("shared_preload_libraries = 'auto_explain'\n", None);
        ensure_image_preload_libraries(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(
            value(&dir, "postgresql.conf"),
            ["auto_explain", "pg_stat_statements", "pgaudit"]
        );
    }

    #[test]
    fn operator_auto_conf_list_is_repaired_by_element_not_by_substring() {
        let dir = pgdata(
            "shared_preload_libraries = 'pg_stat_statements,pgaudit'\n",
            Some("pgaudit.log = 'ddl'\nshared_preload_libraries = 'pg_stat_statements'\n"),
        );
        ensure_image_preload_libraries(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(
            value(&dir, "postgresql.auto.conf"),
            ["pg_stat_statements", "pgaudit"]
        );
        let conf = fs::read_to_string(dir.path().join("postgresql.conf")).unwrap();
        assert_eq!(conf.matches("shared_preload_libraries").count(), 1);
    }

    #[test]
    fn alter_system_renderings_normalize_to_the_same_list() {
        assert_eq!(
            preload_libraries("shared_preload_libraries = 'pg_stat_statements, pgaudit'\n"),
            ["pg_stat_statements", "pgaudit"]
        );
        assert_eq!(
            preload_libraries("shared_preload_libraries = '\"pg_stat_statements,pgaudit\"'\n"),
            ["pg_stat_statements", "pgaudit"]
        );
        assert!(preload_libraries("shared_preload_libraries = ''\n").is_empty());
        assert!(preload_libraries("#shared_preload_libraries = 'x'\n").is_empty());
    }

    #[test]
    fn uninitialized_volume_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        ensure_image_preload_libraries(dir.path().to_str().unwrap()).unwrap();
        assert!(!dir.path().join("postgresql.conf").exists());
    }
}
