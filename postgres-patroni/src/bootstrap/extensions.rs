//! pg_stat_statements version reconcile
//!
//! Mirrors postgres-ssl's wrapper.sh fork_extension_reconcile. pg_upgrade
//! preserves an extension's SQL-level version and nothing else ever runs
//! `ALTER EXTENSION ... UPDATE`, so a major-upgraded cluster keeps
//! pg_stat_statements at the version its ORIGINAL major shipped while fresh
//! clusters get the current one — the fleet forks into per-major dialects of
//! the same view, and the dashboard's Stats tab (which owns this extension:
//! the image preloads it, the dashboard installs and queries it) has a single
//! hardcoded query serving both. Reconciling at boot and on promotion heals
//! upgrades and already-lagging volumes alike.
//!
//! Deliberately scoped to pg_stat_statements: user-installed extensions are
//! the user's to update (an update can carry semantics or cost that isn't
//! ours to decide); the dashboard preflight warns about those instead. For
//! pg_stat_statements the update only redefines views/functions — no table
//! data — so running it idempotently on every boot is safe.
//!
//! Must run on the primary: ALTER EXTENSION is DDL and fails on a replica
//! with a read-only-transaction error. Replicas receive the updated catalog
//! through normal WAL streaming. During a rolling image update a replica
//! still on the older image can briefly serve the new SQL definitions with
//! its older .so — reads of the view may error until that node's own
//! redeploy lands; transient by construction, resolved by the same rollout
//! that caused it.

use super::{read_credentials, run_psql, run_psql_in_db};

/// COPY TO STDOUT prints one bare value per line — no headers, no psql
/// formatting — which keeps this parseable through run_psql's plain output.
const LIST_DATABASES_SQL: &str =
    "COPY (SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate) TO STDOUT";

const VERSIONS_SQL: &str = "COPY (SELECT e.extversion || ' ' || ae.default_version \
     FROM pg_catalog.pg_extension e \
     JOIN pg_catalog.pg_available_extensions ae ON ae.name = e.extname \
     WHERE e.extname = 'pg_stat_statements') TO STDOUT";

/// Bring pg_stat_statements up to the version this image's binaries ship, in
/// every connectable database that has it installed. Every failure is logged
/// and swallowed: a failed extension update must never take the node down.
pub fn reconcile_pg_stat_statements() {
    let superuser = match read_credentials() {
        Ok(c) => c.superuser,
        Err(e) => {
            tracing::warn!(error = %e, "extension-reconcile: could not read credentials");
            return;
        }
    };

    let databases = match run_psql(&superuser, LIST_DATABASES_SQL) {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!(error = %e, "extension-reconcile: could not list databases");
            return;
        }
    };

    for db in databases.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let versions = match run_psql_in_db(&superuser, db, VERSIONS_SQL) {
            Ok(out) => out,
            Err(e) => {
                tracing::warn!(database = %db, error = %e, "extension-reconcile: version probe failed");
                continue;
            }
        };
        let mut parts = versions.split_whitespace();
        let (installed, available) = match (parts.next(), parts.next()) {
            (Some(i), Some(a)) => (i.to_string(), a.to_string()),
            // Not installed in this database (or, defensively, unparseable
            // output): nothing to reconcile here.
            _ => continue,
        };
        if installed == available {
            continue;
        }
        // `ALTER EXTENSION ... UPDATE` without TO walks the chained upgrade
        // scripts to the control file's default_version.
        match run_psql_in_db(&superuser, db, "ALTER EXTENSION pg_stat_statements UPDATE") {
            Ok(_) => tracing::info!(
                database = %db,
                from = %installed,
                to = %available,
                "extension-reconcile: pg_stat_statements updated"
            ),
            Err(e) => tracing::warn!(
                database = %db,
                from = %installed,
                to = %available,
                error = %e,
                "extension-reconcile: pg_stat_statements update failed; leaving it as it was"
            ),
        }
    }
}
