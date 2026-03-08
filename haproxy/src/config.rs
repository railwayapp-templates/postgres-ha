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
            health_port_override: std::env::var("HAPROXY_HEALTH_PORT")
                .ok()
                .and_then(|s| s.parse().ok()),
        })
    }
}
