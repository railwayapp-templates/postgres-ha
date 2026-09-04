//! HAProxy configuration from environment variables

use anyhow::{Context, Result};
use common::ConfigExt;

/// Configuration for HAProxy
pub struct Config {
    pub postgres_nodes: String,
    pub max_conn: String,
    pub timeout_connect: String,
    pub timeout_client: String,
    pub timeout_server: String,
    pub timeout_check: String,
    pub check_interval: String,
    pub check_fastinter: String,
    pub check_downinter: String,
    /// Override the health check port from POSTGRES_NODES.
    /// If set, uses this port instead of the patroni port from POSTGRES_NODES.
    /// Set to 8009 to use the direct health server instead of Patroni API.
    pub health_port_override: Option<u16>,
    /// Basic-auth account for the stats page when reached over the network.
    /// Loopback (the local monitor and the container healthcheck) never
    /// authenticates. `None` means no credential is available, and remote
    /// access to the stats page is denied outright.
    pub stats_auth: Option<StatsAuth>,
}

/// Credential guarding the stats page for non-loopback clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsAuth {
    pub user: String,
    pub password: String,
}

/// Resolve the stats credential: `HAPROXY_STATS_USER` / `HAPROXY_STATS_PASSWORD`
/// when set, else the database account the proxy already carries
/// (`PGUSER` / `PGPASSWORD`). An empty password yields `None`.
pub(crate) fn resolve_stats_auth(
    stats_user: Option<String>,
    stats_password: Option<String>,
    pg_user: Option<String>,
    pg_password: Option<String>,
) -> Option<StatsAuth> {
    let non_empty = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
    let password = non_empty(stats_password).or_else(|| non_empty(pg_password))?;
    let user = non_empty(stats_user)
        .or_else(|| non_empty(pg_user))
        .unwrap_or_else(|| "postgres".to_string());
    Some(StatsAuth { user, password })
}

impl Config {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self> {
        let postgres_nodes = String::env_required("POSTGRES_NODES").context(
            "POSTGRES_NODES is required.\n\
             Format: hostname:pgport:patroniport,hostname:pgport:patroniport,...\n\
             Example: postgres-1.railway.internal:5432:8008,postgres-2.railway.internal:5432:8008",
        )?;

        Ok(Self {
            postgres_nodes,
            max_conn: String::env_or("HAPROXY_MAX_CONN", "1000"),
            timeout_connect: String::env_or("HAPROXY_TIMEOUT_CONNECT", "10s"),
            timeout_client: String::env_or("HAPROXY_TIMEOUT_CLIENT", "30m"),
            timeout_server: String::env_or("HAPROXY_TIMEOUT_SERVER", "30m"),
            timeout_check: String::env_or("HAPROXY_TIMEOUT_CHECK", "3s"),
            check_interval: String::env_or("HAPROXY_CHECK_INTERVAL", "3s"),
            check_fastinter: String::env_or("HAPROXY_CHECK_FASTINTER", "500ms"),
            check_downinter: String::env_or("HAPROXY_CHECK_DOWNINTER", "500ms"),
            health_port_override: match std::env::var("HAPROXY_HEALTH_PORT") {
                // The override exists because of "Patroni REST API blocking
                // issues" — a typo'd value silently dropping it would point
                // every health check at the Patroni port the override was
                // created to avoid. Fail loud on a malformed value instead.
                Ok(raw) => Some(
                    raw.parse::<u16>()
                        .context(format!("HAPROXY_HEALTH_PORT={raw} is not a valid port"))?,
                ),
                Err(_) => None,
            },
            stats_auth: resolve_stats_auth(
                std::env::var("HAPROXY_STATS_USER").ok(),
                std::env::var("HAPROXY_STATS_PASSWORD").ok(),
                std::env::var("PGUSER").ok(),
                std::env::var("PGPASSWORD").ok(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn stats_auth_prefers_explicit_over_database_account() {
        let auth = resolve_stats_auth(s("ops"), s("secret"), s("railway"), s("dbpass")).unwrap();
        assert_eq!(
            auth,
            StatsAuth {
                user: "ops".into(),
                password: "secret".into()
            }
        );
    }

    #[test]
    fn stats_auth_falls_back_to_database_account() {
        let auth = resolve_stats_auth(None, None, s("railway"), s("dbpass")).unwrap();
        assert_eq!(
            auth,
            StatsAuth {
                user: "railway".into(),
                password: "dbpass".into()
            }
        );
    }

    #[test]
    fn stats_auth_defaults_user_when_only_password_is_known() {
        let auth = resolve_stats_auth(None, s("secret"), None, None).unwrap();
        assert_eq!(auth.user, "postgres");
    }

    #[test]
    fn stats_auth_is_none_without_a_password() {
        assert!(resolve_stats_auth(s("ops"), None, s("railway"), None).is_none());
        assert!(resolve_stats_auth(None, s("  "), None, s("")).is_none());
    }
}
