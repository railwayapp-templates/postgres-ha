//! PostgreSQL node parsing

use anyhow::{anyhow, Result};
use tracing::warn;

/// PostgreSQL node information
#[derive(Debug)]
pub struct PostgresNode {
    pub name: String,
    pub host: String,
    pub pg_port: String,
    pub health_port: String,
}

/// Parse one `hostname:pgport:healthport` entry. Ports must be numeric —
/// failing fast here (rather than at haproxy-config-render time) means a bad
/// entry is reported by name instead of producing a config whose every check
/// fails at runtime for no visible reason.
fn parse_one_node(node: &str) -> Result<PostgresNode> {
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
}

/// Parse nodes from the POSTGRES_NODES environment variable
///
/// Format: "hostname:pgport:healthport,..."
/// Example: "postgres-1.railway.internal:5432:8008,postgres-2.railway.internal:5432:8008"
///
/// A single malformed entry (e.g. an empty port from a broken variable
/// reference elsewhere in the cluster — a deleted PGPORT/PGHOST on the node
/// it points at) is skipped with a warning rather than failing the whole
/// list: previously one bad entry took down the ENTIRE haproxy/load-balancer
/// for the whole cluster, even when the other nodes were perfectly healthy
/// (considerate-illumination/production, 2026-08-23 — the root's PGPORT/PGHOST
/// had been deleted, so every one of the three configured nodes resolved to
/// the same empty-port entry, and haproxy never started at all). Only errors
/// when the list has NO usable entries — haproxy genuinely needs at least one
/// backend to route to.
pub fn parse_nodes(postgres_nodes: &str) -> Result<Vec<PostgresNode>> {
    let mut nodes = Vec::new();
    let mut skipped = 0usize;

    for entry in postgres_nodes.split(',') {
        match parse_one_node(entry) {
            Ok(n) => nodes.push(n),
            Err(e) => {
                skipped += 1;
                warn!(entry, error = %e, "skipping malformed POSTGRES_NODES entry");
            }
        }
    }

    if nodes.is_empty() {
        return Err(anyhow!(
            "POSTGRES_NODES had no valid entries ({skipped} malformed): {postgres_nodes}"
        ));
    }

    if skipped > 0 {
        warn!(
            skipped,
            valid = nodes.len(),
            "POSTGRES_NODES had malformed entries; continuing with the valid subset"
        );
    }

    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_nodes() {
        let nodes = parse_nodes(
            "postgres-1.railway.internal:5432:8008,postgres-2.railway.internal:5432:8008",
        )
        .unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].name, "postgres-1");
        assert_eq!(nodes[0].host, "postgres-1.railway.internal");
        assert_eq!(nodes[0].pg_port, "5432");
        assert_eq!(nodes[0].health_port, "8008");
    }

    #[test]
    fn skips_a_malformed_entry_and_keeps_the_valid_ones() {
        // The considerate-illumination shape: one entry's pg port resolved to
        // an empty string from a broken variable reference elsewhere.
        let nodes =
            parse_nodes("postgres.railway.internal::8008,postgres-2.railway.internal:5432:8008")
                .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "postgres-2");
    }

    #[test]
    fn skips_every_malformed_entry_but_keeps_going_if_any_are_valid() {
        let nodes = parse_nodes(
            "bad::8008,also-bad:x:8008,postgres-3.railway.internal:5432:8008",
        )
        .unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "postgres-3");
    }

    #[test]
    fn errors_when_every_entry_is_malformed() {
        let err = parse_nodes("postgres.railway.internal::8008,postgres-2.railway.internal::8008")
            .unwrap_err();
        assert!(err.to_string().contains("no valid entries"));
    }

    #[test]
    fn errors_on_empty_input() {
        assert!(parse_nodes("").is_err());
    }

    #[test]
    fn single_node_still_works() {
        let nodes = parse_nodes("postgres-1.railway.internal:5432:8008").unwrap();
        assert_eq!(nodes.len(), 1);
    }
}
