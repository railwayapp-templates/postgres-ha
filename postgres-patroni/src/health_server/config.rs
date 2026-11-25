//! Health server configuration

use common::ConfigExt;

/// A password variable this boot did not adopt: it differs from the pinned
/// password, and the cluster had not vouched for it when the member started.
#[derive(Clone, PartialEq, Eq)]
pub struct UnadoptedVariable {
    /// The superuser password as the variables say it.
    pub password: String,
    /// The pinned password this boot kept instead.
    pub pinned: String,
}

impl std::fmt::Debug for UnadoptedVariable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("UnadoptedVariable(..)")
    }
}

/// Configuration for the health server
#[derive(Debug, Clone)]
pub struct HealthServerConfig {
    /// Port to listen on for health checks (default: 8009)
    pub port: u16,
    /// PostgreSQL port (default: 5432)
    pub pg_port: u16,
    /// PostgreSQL user (default: postgres)
    pub pg_user: String,
    /// PostgreSQL password
    pub pg_password: String,
    /// PostgreSQL database (default: postgres)
    pub pg_database: String,
    /// Patroni REST API port for fallback (default: 8008)
    pub patroni_port: u16,
    /// Total wall-clock budget in ms for a single /primary or /replica check,
    /// covering both the direct PostgreSQL query and the Patroni/etcd fallback
    /// (default: 2000). Must stay comfortably under HAProxy's own
    /// HAPROXY_TIMEOUT_CHECK (default 3s) -- see routes.rs for why. postgres.rs
    /// splits this in half internally to bound each of the two sequential
    /// stages (pg connect, then the Patroni fallback) so one can't starve the
    /// other of its turn.
    pub check_timeout_ms: u64,
    /// Set by the runner when this boot kept the pinned credentials against
    /// a password variable that differs from them (see
    /// `credentials::adopt_rotated_variable`).
    pub unadopted_variable: Option<UnadoptedVariable>,
}

impl HealthServerConfig {
    /// Create configuration from environment variables.
    /// Must be called BEFORE clearing PG* environment variables.
    ///
    /// The health server always connects to localhost — it runs inside the same
    /// container as PostgreSQL so PGHOST is irrelevant and not read here.
    pub fn from_env() -> Self {
        Self {
            port: u16::env_parse("HEALTH_SERVER_PORT", 8009),
            pg_port: u16::env_parse("PGPORT", 5432),
            // An empty PGUSER would reach Postgres as an empty user name and
            // fail every check, so it reads as unset.
            pg_user: String::env_non_empty_or("PGUSER", "postgres"),
            pg_password: String::env_parse("PGPASSWORD", String::new()),
            pg_database: String::env_parse("PGDATABASE", "postgres".to_string()),
            patroni_port: u16::env_parse("PATRONI_PORT", 8008),
            check_timeout_ms: u64::env_parse("HEALTH_CHECK_TIMEOUT_MS", 2000),
            unadopted_variable: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes the tests below, which all set `PGUSER` in the process.
    static PGUSER_LOCK: Mutex<()> = Mutex::new(());

    fn pg_user_with(value: Option<&str>) -> String {
        let _env = PGUSER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("PGUSER").ok();
        match value {
            Some(v) => std::env::set_var("PGUSER", v),
            None => std::env::remove_var("PGUSER"),
        }
        let user = HealthServerConfig::from_env().pg_user;
        match previous {
            Some(v) => std::env::set_var("PGUSER", v),
            None => std::env::remove_var("PGUSER"),
        }
        user
    }

    #[test]
    fn empty_pguser_connects_as_the_default_user() {
        assert_eq!(pg_user_with(Some("")), "postgres");
    }

    #[test]
    fn whitespace_pguser_connects_as_the_default_user() {
        assert_eq!(pg_user_with(Some(" ")), "postgres");
    }

    #[test]
    fn set_pguser_is_kept() {
        assert_eq!(pg_user_with(Some("app")), "app");
    }

    #[test]
    fn unset_pguser_connects_as_the_default_user() {
        assert_eq!(pg_user_with(None), "postgres");
    }
}
