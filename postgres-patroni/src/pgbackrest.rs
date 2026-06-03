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
        // Force untranslated output so prefix matching on the English labels
        // below is stable regardless of the service's locale env.
        .env("LC_ALL", "C")
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

/// Read the cluster's configured `wal_level` (`minimal` / `replica` /
/// `logical`) from pg_control via `pg_controldata`. Returns None when
/// pg_control isn't on disk yet (fresh volume, pre-initdb) or parsing fails.
///
/// Used during HA conversion to detect whether the adopted standalone cluster
/// was running logical replication (e.g. a CDC pipeline like Fivetran) so the
/// generated Patroni bootstrap config preserves `wal_level: logical` instead
/// of silently downgrading it to `replica` — `replica` disables logical
/// decoding and breaks the customer's existing replication slots.
pub fn read_wal_level(data_dir: &str) -> Option<String> {
    let pg_control = format!("{data_dir}/global/pg_control");
    if !Path::new(&pg_control).exists() {
        // Fresh volume, pre-initdb. `replica` is the correct default; this is
        // not an adopted cluster, so there's nothing to preserve.
        return None;
    }
    // From here on pg_control EXISTS, so this is an existing (potentially
    // adopted) cluster. Any failure to read its wal_level means we fall back to
    // `replica` — which, if the cluster was actually `logical`, silently
    // downgrades it and breaks logical replication. That's the exact failure
    // this code prevents, so make it observable rather than swallowing it.
    let out = match std::process::Command::new("pg_controldata")
        // Force untranslated output so prefix matching in parse_wal_level is
        // stable regardless of the service's locale env.
        .env("LC_ALL", "C")
        .arg(data_dir)
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            warn!(error = %e, data_dir, "pg_controldata failed to spawn; cannot determine wal_level of existing cluster, defaulting to replica");
            return None;
        }
    };
    if !out.status.success() {
        warn!(
            status = ?out.status.code(),
            stderr = %String::from_utf8_lossy(&out.stderr),
            data_dir,
            "pg_controldata exited non-zero; cannot determine wal_level of existing cluster, defaulting to replica"
        );
        return None;
    }
    let level = parse_wal_level(&String::from_utf8_lossy(&out.stdout));
    if level.is_none() {
        warn!(data_dir, "pg_controldata output had no parseable wal_level line; defaulting to replica");
    }
    level
}

/// Parse the `wal_level setting:` line out of `pg_controldata` stdout.
fn parse_wal_level(controldata_stdout: &str) -> Option<String> {
    for line in controldata_stdout.lines() {
        if let Some(rest) = line.strip_prefix("wal_level setting:") {
            let level = rest.trim();
            if !level.is_empty() {
                return Some(level.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_wal_level;

    // Real `pg_controldata` output uses the label "wal_level setting:" padded
    // with spaces before the value.
    const SAMPLE: &str = "\
Database cluster state:               in production
Latest checkpoint location:           0/1A2B3C0
wal_level setting:                    logical
wal_log_hints setting:                off
max_connections setting:              200
";

    #[test]
    fn parses_logical() {
        assert_eq!(parse_wal_level(SAMPLE).as_deref(), Some("logical"));
    }

    #[test]
    fn parses_replica() {
        let out = SAMPLE.replace("logical", "replica");
        assert_eq!(parse_wal_level(&out).as_deref(), Some("replica"));
    }

    #[test]
    fn none_when_absent() {
        assert_eq!(parse_wal_level("Database cluster state: in production\n"), None);
    }
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
