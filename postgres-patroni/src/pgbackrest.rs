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
        warn!(
            data_dir,
            "pg_controldata output had no parseable wal_level line; defaulting to replica"
        );
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
        assert_eq!(
            parse_wal_level("Database cluster state: in production\n"),
            None
        );
    }
}

fn write_pgbackrest_repo_path_marker(marker: &str, path: &str) {
    let tmp = format!("{marker}.{}.tmp", std::process::id());
    let write = || -> std::io::Result<()> {
        fs::write(&tmp, format!("{path}\n"))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o640))?;
        fs::rename(&tmp, marker)
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&tmp);
        warn!(error = %e, marker = %marker, "pgbackrest: failed to atomically write repo-path marker");
    }
}

/// A repo prefix must be absolute and occupy a single config/marker line.
pub fn repo_path_is_usable(path: &str) -> bool {
    path.starts_with('/') && !path.contains(['\n', '\r'])
}

pub fn archive_base_path(value: &str) -> &str {
    if repo_path_is_usable(value) {
        value
    } else {
        "/pgbackrest"
    }
}

/// Invalid prefixes cannot have archived data. Clear local success/cache state
/// before replacing their marker so stale timestamps or spool acks cannot make
/// the new archive look protected. Never remove bucket objects.
fn reset_invalid_repo_state(data_dir: &str) -> std::io::Result<()> {
    for name in [".pgbackrest_backup_state", ".pgbackrest_gap_pending"] {
        match fs::remove_file(format!("{data_dir}/{name}")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let spool = format!("{data_dir}/pgbackrest-spool/archive/main/out");
    match fs::read_dir(spool) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let path = entry.path();
                if matches!(
                    path.extension().and_then(|v| v.to_str()),
                    Some("ok" | "error")
                ) {
                    fs::remove_file(path)?;
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(())
}

/// Resolve the effective repo1-path for archiving. Uses the per-cluster
/// `<base>/cluster-<sysid>` form so a wipe-and-reuse-bucket cycle (volume
/// wiped, container redeployed against the same WAL_ARCHIVE_BUCKET) lets
/// the new cluster's history coexist with the old at distinct sub-prefixes.
///
/// 1. Usable marker present → trust it; rederive an invalid one. Idempotent across boots; survives
///    container restarts; wiped with the volume.
/// 2. pg_control exists, marker absent → derive `<base>/cluster-<sysid>`,
///    write marker.
///
/// pg_control must exist before calling (i.e. Postgres has initialised).
/// Returns base path on the theoretically-unreachable read failure rather
/// than panicking.
pub fn derive_pgbackrest_repo_path(data_dir: &str) -> String {
    let configured = env::var("WAL_ARCHIVE_PATH").unwrap_or_default();
    let user_path = archive_base_path(&configured).to_string();
    let marker = format!("{data_dir}/.pgbackrest_repo_path");

    if let Ok(existing) = fs::read_to_string(&marker) {
        let trimmed = existing.trim();
        if repo_path_is_usable(trimmed) {
            return trimmed.to_string();
        }
        if let Err(e) = reset_invalid_repo_state(data_dir) {
            warn!(error = %e, "pgbackrest: cannot clear invalid repo-path state; retrying on next bootstrap");
            return user_path;
        }
        warn!("pgbackrest: re-deriving an unusable repo-path marker");
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

#[cfg(test)]
mod repo_path_tests {
    use super::*;
    #[test]
    fn accepts_only_absolute_single_line_paths() {
        for path in [
            "",
            "relative",
            "C:/Program Files/Git/pgbackrest",
            "/a\nb",
            "/a\rb",
        ] {
            assert!(!repo_path_is_usable(path));
            assert_eq!(archive_base_path(path), "/pgbackrest");
        }
        assert!(repo_path_is_usable("/pgbackrest/cluster-1-2"));
    }
    #[test]
    fn valid_migrated_marker_wins_without_pg_control() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(".pgbackrest_repo_path"),
            "/old/cluster-1-2\n",
        )
        .unwrap();
        assert_eq!(
            derive_pgbackrest_repo_path(dir.path().to_str().unwrap()),
            "/old/cluster-1-2"
        );
    }
    #[test]
    fn invalid_marker_reset_removes_only_local_backup_state_and_acks() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("pgbackrest-spool/archive/main/out");
        fs::create_dir_all(&spool).unwrap();
        for name in ["old.ok", "old.error", "keep"] {
            fs::write(spool.join(name), "x").unwrap();
        }
        fs::write(
            dir.path().join(".pgbackrest_backup_state"),
            "last_full_at=1\n",
        )
        .unwrap();
        reset_invalid_repo_state(dir.path().to_str().unwrap()).unwrap();
        assert!(!dir.path().join(".pgbackrest_backup_state").exists());
        assert!(!spool.join("old.ok").exists());
        assert!(!spool.join("old.error").exists());
        assert!(spool.join("keep").exists());
    }
}
