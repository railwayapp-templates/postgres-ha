//! PostgreSQL node parsing

use anyhow::{anyhow, Result};

/// PostgreSQL node information
#[derive(Debug)]
pub struct PostgresNode {
    pub name: String,
    pub host: String,
    pub pg_port: String,
    pub health_port: String,
}

/// Parse nodes from the POSTGRES_NODES environment variable
///
/// Format: "hostname:pgport:healthport,..."
/// Example: "postgres-1.railway.internal:5432:8008,postgres-2.railway.internal:5432:8008"
pub fn parse_nodes(postgres_nodes: &str) -> Result<Vec<PostgresNode>> {
    postgres_nodes
        .split(',')
        .map(|node| {
            let parts: Vec<&str> = node.split(':').collect();
            if parts.len() != 3 {
                return Err(anyhow!(
                    "Invalid node format: {}. Expected: hostname:pgport:healthport",
                    node
                ));
            }

            let host = parts[0].to_string();
            let name = host.split('.').next().unwrap_or(&host).to_string();
            let pg_port = parts[1].to_string();
            let health_port = parts[2].to_string();
            // Fail fast at boot on non-numeric ports (a stray letter, a
            // truncated entry) instead of rendering an haproxy config whose
            // every check fails at runtime for no visible reason.
            for (label, port) in [("pg port", &pg_port), ("health port", &health_port)] {
                if port.parse::<u16>().is_err() {
                    return Err(anyhow!(
                        "Invalid {label} '{port}' in node format: {}. Expected numeric ports (hostname:pgport:healthport)",
                        node
                    ));
                }
            }

            Ok(PostgresNode {
                name,
                host,
                pg_port,
                health_port,
            })
        })
        .collect()
}
