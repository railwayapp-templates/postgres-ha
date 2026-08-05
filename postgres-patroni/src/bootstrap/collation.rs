//! Collation version refresh
//!
//! Mirrors postgres-ssl's wrapper.sh fork_collation_refresh. Container image rebuilds
//! (any minor version bump, not just a major one) can ship a newer glibc while the
//! volume's databases still carry the old collation version stamp — postgres emits a
//! WARNING on every connection to an affected database until it's refreshed.
//!
//! Must run on the primary: ALTER DATABASE fails with a read-only-transaction error on
//! a replica. The corrected pg_database.datcollversion then reaches replicas through
//! normal WAL streaming — no per-replica action needed, since a replica's own mismatch
//! check compares that (replicated) stored value against its own locally-observed
//! glibc, and both sides converge once every node in the cluster runs the same image.

use super::{read_credentials, run_psql};
use crate::pgdata;
use std::fs;

const REFRESH_ALL_DATABASES_SQL: &str = r#"
DO $body$
DECLARE
  db record;
BEGIN
  FOR db IN
    SELECT datname FROM pg_database
    WHERE datallowconn AND datname <> 'template0'
  LOOP
    BEGIN
      EXECUTE format('ALTER DATABASE %I REFRESH COLLATION VERSION', db.datname);
    EXCEPTION WHEN OTHERS THEN
      NULL;
    END;
  END LOOP;
END
$body$;
"#;

/// Refresh collation versions on all connectable databases. No-op on PG < 15
/// (`ALTER DATABASE ... REFRESH COLLATION VERSION` was introduced in PG 15) and when
/// PG_VERSION can't be read (pre-initdb).
pub fn refresh_collation_versions() {
    let pg_version_file = format!("{}/PG_VERSION", pgdata());
    let pg_major: u32 = match fs::read_to_string(&pg_version_file) {
        Ok(v) => v.trim().parse().unwrap_or(0),
        Err(_) => return,
    };
    if pg_major < 15 {
        return;
    }

    let superuser = match read_credentials() {
        Ok(c) => c.superuser,
        Err(e) => {
            tracing::warn!(error = %e, "collation-refresh: could not read credentials");
            return;
        }
    };

    match run_psql(&superuser, REFRESH_ALL_DATABASES_SQL) {
        Ok(_) => tracing::info!("collation-refresh: completed for all databases"),
        Err(e) => tracing::warn!(error = %e, "collation-refresh: failed"),
    }
}
