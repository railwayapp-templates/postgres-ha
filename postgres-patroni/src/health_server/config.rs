//! Health server configuration

use common::ConfigExt;

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
    /// HAPROXY_TIMEOUT_CHECK (default 3s) -- see routes.rs for why.
    pub check_timeout_ms: u64,
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
            pg_user: String::env_parse("PGUSER", "postgres".to_string()),
            pg_password: String::env_parse("PGPASSWORD", String::new()),
            pg_database: String::env_parse("PGDATABASE", "postgres".to_string()),
            patroni_port: u16::env_parse("PATRONI_PORT", 8008),
            check_timeout_ms: u64::env_parse("HEALTH_CHECK_TIMEOUT_MS", 2000),
        }
    }
}
