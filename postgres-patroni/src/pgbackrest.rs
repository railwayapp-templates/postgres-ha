//! pgBackRest path utilities shared between patroni-runner and on_role_change.

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tracing::warn;

/// Read Postgres' `system_identifier` from pg_control via the
/// `pg_controldata` binary. Returns None when pg_control isn't on disk yet
/// (fresh volume, pre-initdb) or when parsing fails.
pub fn read_postgres_sysid(data_dir: &str) -> Option<String> {
    let pg_control = format!("{data_dir}/global/pg_control");
    if !Path::new(&pg_control).exists() {
        return None;
    }
    let out = std::process::Command::new("pg_controldata")
        .arg(data_dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Database system identifier:") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn write_pgbackrest_repo_path_marker(marker: &str, path: &str) {
    if let Err(e) = fs::write(marker, format!("{path}\n")) {
        warn!(error = %e, marker = %marker, "pgbackrest: failed to write repo-path marker");
        return;
    }
    if let Err(e) = fs::set_permissions(marker, std::fs::Permissions::from_mode(0o640)) {
        warn!(error = %e, marker = %marker, "pgbackrest: failed to set marker permissions");
    }
}

/// Resolve the effective repo1-path for archiving. Uses the per-cluster
/// `<base>/cluster-<sysid>` form so a wipe-and-reuse-bucket cycle (volume
/// wiped, container redeployed against the same WAL_ARCHIVE_BUCKET) lets
/// the new cluster's history coexist with the old at distinct sub-prefixes.
///
/// 1. Marker file present → trust it. Idempotent across boots; survives
///    container restarts; wiped with the volume.
/// 2. pg_control exists, marker absent → derive `<base>/cluster-<sysid>`,
///    write marker.
///
/// pg_control must exist before calling (i.e. Postgres has initialised).
/// Returns base path on the theoretically-unreachable read failure rather
/// than panicking.
pub fn derive_pgbackrest_repo_path(data_dir: &str) -> String {
    let user_path = env::var("WAL_ARCHIVE_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/pgbackrest".to_string());
    let marker = format!("{data_dir}/.pgbackrest_repo_path");

    if let Ok(existing) = fs::read_to_string(&marker) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let Some(sysid) = read_postgres_sysid(data_dir) else {
        warn!("pgbackrest: pg_control missing at derive_pgbackrest_repo_path; using base path");
        return user_path;
    };

    let trimmed_base = user_path.trim_end_matches('/');
    let cluster_path = format!("{trimmed_base}/cluster-{sysid}");
    write_pgbackrest_repo_path_marker(&marker, &cluster_path);
    cluster_path
}
