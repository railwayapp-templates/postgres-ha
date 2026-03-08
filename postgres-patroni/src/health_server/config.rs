//! Health server configuration

use common::ConfigExt;

/// Configuration for the health server
#[derive(Debug, Clone)]
pub struct HealthServerConfig {
    /// Port to listen on for health checks (default: 8009)
    pub port: u16,
    /// PostgreSQL host (default: localhost)
    pub pg_host: String,
    /// PostgreSQL port (default: 5432)
    pub pg_port: u16,
    /// PostgreSQL user (default: postgres)
    pub pg_user: String,
    /// PostgreSQL password
    pub pg_password: String,
    /// PostgreSQL database (default: postgres)
    pub pg_database: String,
}

impl HealthServerConfig {
    /// Create configuration from environment variables.
    /// Must be called BEFORE clearing PG* environment variables.
    pub fn from_env() -> Self {
        Self {
            port: u16::env_parse("HEALTH_SERVER_PORT", 8009),
            pg_host: String::env_parse("PGHOST", "localhost".to_string()),
            pg_port: u16::env_parse("PGPORT", 5432),
            pg_user: String::env_parse("PGUSER", "postgres".to_string()),
            pg_password: String::env_parse("PGPASSWORD", String::new()),
            pg_database: String::env_parse("PGDATABASE", "postgres".to_string()),
        }
    }
}
