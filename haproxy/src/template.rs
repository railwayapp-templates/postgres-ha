//! HAProxy configuration template generation

use crate::config::Config;
use crate::nodes::PostgresNode;

/// Generate server entries for backend configuration
fn generate_server_entries(
    nodes: &[PostgresNode],
    single_node_mode: bool,
    health_port_override: Option<u16>,
) -> String {
    nodes
        .iter()
        .map(|node| {
            if single_node_mode {
                // Single node: basic TCP check
                format!(
                    "    server {} {}:{} check resolvers railway",
                    node.name, node.host, node.pg_port
                )
            } else {
                // Multi-node: HTTP health check on health port
                let health_port = match health_port_override {
                    Some(port) => port.to_string(),
                    None => node.health_port.clone(),
                };
                format!(
                    "    server {} {}:{} check port {} resolvers railway",
                    node.name, node.host, node.pg_port, health_port
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Generate primary backend configuration
fn generate_primary_backend(
    config: &Config,
    server_entries: &str,
    single_node_mode: bool,
) -> String {
    if single_node_mode {
        format!(
            r#"backend postgresql_primary_backend
    default-server inter {} fall 3 rise 2 fastinter {} downinter {} on-marked-down shutdown-sessions
{}"#,
            config.check_interval, config.check_fastinter, config.check_downinter, server_entries
        )
    } else {
        // HTTP health check (works for both Patroni API and direct health server)
        format!(
            r#"backend postgresql_primary_backend
    option httpchk
    http-check send meth GET uri /primary
    http-check expect status 200
    default-server inter {} fall 3 rise 2 fastinter {} downinter {} on-marked-down shutdown-sessions
{}"#,
            config.check_interval, config.check_fastinter, config.check_downinter, server_entries
        )
    }
}

/// Generate replica backend configuration
fn generate_replica_backend(
    config: &Config,
    server_entries: &str,
    single_node_mode: bool,
) -> String {
    if single_node_mode {
        format!(
            r#"backend postgresql_replicas_backend
    balance leastconn
    default-server inter {} fall 3 rise 2 fastinter {} downinter {} on-marked-down shutdown-sessions
{}"#,
            config.check_interval, config.check_fastinter, config.check_downinter, server_entries
        )
    } else {
        // HTTP health check (works for both Patroni API and direct health server)
        format!(
            r#"backend postgresql_replicas_backend
    balance leastconn
    option httpchk
    http-check send meth GET uri /replica
    http-check expect status 200
    default-server inter {} fall 3 rise 2 fastinter {} downinter {} on-marked-down shutdown-sessions
{}"#,
            config.check_interval, config.check_fastinter, config.check_downinter, server_entries
        )
    }
}

/// Generate complete HAProxy configuration
pub fn generate_config(config: &Config, nodes: &[PostgresNode]) -> String {
    let single_node_mode = nodes.len() == 1;
    let server_entries =
        generate_server_entries(nodes, single_node_mode, config.health_port_override);
    let primary_backend = generate_primary_backend(config, &server_entries, single_node_mode);
    let replica_backend = generate_replica_backend(config, &server_entries, single_node_mode);

    format!(
        r#"global
    maxconn {}
    log stdout format raw local0
    # The container's STOPSIGNAL (SIGUSR1, inherited from the upstream image)
    # starts a soft stop that waits for open sessions to end. With client and
    # server timeouts of 30m that drain would outlive the container runtime's
    # stop grace period (10s) and end in SIGKILL anyway; 8s keeps the graceful
    # exit inside the grace with room for the entrypoint to report it.
    hard-stop-after 8s

defaults
    log global
    mode tcp
    option tcpka
    option clitcpka
    option srvtcpka
    option redispatch
    retries 3
    timeout connect {}
    timeout client {}
    timeout server {}
    timeout check {}

resolvers railway
    parse-resolv-conf
    resolve_retries 3
    timeout resolve 1s
    timeout retry   1s
    hold other      10s
    hold refused    10s
    hold nx         10s
    hold timeout    10s
    hold valid      10s
    hold obsolete   10s

# Stats page for monitoring
listen stats
    bind :::8404 v4v6
    mode http
    stats enable
    stats uri /stats
    stats refresh 10s

# Primary PostgreSQL (read-write)
frontend postgresql_primary
    bind :::5432 v4v6
    default_backend postgresql_primary_backend

{}

# Replica PostgreSQL (read-only)
frontend postgresql_replicas
    bind :::5433 v4v6
    default_backend postgresql_replicas_backend

{}
"#,
        config.max_conn,
        config.timeout_connect,
        config.timeout_client,
        config.timeout_server,
        config.timeout_check,
        primary_backend,
        replica_backend
    )
}

#[cfg(test)]
mod tests {
    use super::generate_config;
    use crate::config::Config;
    use crate::nodes::parse_nodes;

    fn test_config(postgres_nodes: &str) -> Config {
        Config {
            postgres_nodes: postgres_nodes.to_string(),
            max_conn: "1000".to_string(),
            timeout_connect: "10s".to_string(),
            timeout_client: "30m".to_string(),
            timeout_server: "30m".to_string(),
            timeout_check: "3s".to_string(),
            check_interval: "3s".to_string(),
            check_fastinter: "500ms".to_string(),
            check_downinter: "500ms".to_string(),
            health_port_override: None,
        }
    }

    #[test]
    fn soft_stop_is_bounded_inside_the_stop_grace() {
        let config = test_config("pg-1:5432:8008,pg-2:5432:8008");
        let nodes = parse_nodes(&config.postgres_nodes).unwrap();
        let rendered = generate_config(&config, &nodes);

        let global = rendered
            .split("\ndefaults")
            .next()
            .expect("config starts with the global section");
        assert!(
            global.contains("hard-stop-after 8s"),
            "hard-stop-after must live in the global section:\n{global}"
        );
    }
}
