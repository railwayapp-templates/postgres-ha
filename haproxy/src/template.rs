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

{}
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
        generate_stats_listener(config),
        primary_backend,
        replica_backend
    )
}

/// The stats listener. Loopback clients (the in-container monitor and the
/// healthcheck) are always allowed. Anyone else must present the stats
/// credential; without a credential configured, remote access is denied.
///
/// The credential is read by haproxy from the environment at parse time
/// (`"${HAPROXY_STATS_USER}"` / `"${HAPROXY_STATS_PASSWORD}"`) so the
/// rendered config — which is logged at startup — never contains it.
fn generate_stats_listener(config: &Config) -> String {
    let (userlist, remote_rule) = if config.stats_auth.is_some() {
        (
            "userlist stats_users\n    user \"${HAPROXY_STATS_USER}\" insecure-password \"${HAPROXY_STATS_PASSWORD}\"\n\n",
            "http-request auth unless { http_auth(stats_users) }",
        )
    } else {
        ("", "http-request deny")
    };
    format!(
        r#"{userlist}# Stats page for monitoring
listen stats
    bind :::8404 v4v6
    mode http
    acl LOCALHOST src 127.0.0.1 ::1 ::ffff:127.0.0.1
    http-request allow if LOCALHOST
    {remote_rule}
    stats enable
    stats uri /stats
    stats refresh 10s
"#
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
            stats_auth: None,
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

#[cfg(test)]
mod stats_tests {
    use super::*;
    use crate::config::StatsAuth;

    fn test_config(stats_auth: Option<StatsAuth>) -> Config {
        Config {
            postgres_nodes: "pg-1:5432:8009,pg-2:5432:8009".into(),
            max_conn: "1000".into(),
            timeout_connect: "10s".into(),
            timeout_client: "30m".into(),
            timeout_server: "30m".into(),
            timeout_check: "3s".into(),
            check_interval: "3s".into(),
            check_fastinter: "500ms".into(),
            check_downinter: "500ms".into(),
            health_port_override: Some(8009),
            stats_auth,
        }
    }

    fn nodes() -> Vec<PostgresNode> {
        crate::nodes::parse_nodes("pg-1:5432:8009,pg-2:5432:8009").unwrap()
    }

    #[test]
    fn stats_page_requires_auth_for_remote_clients_when_credential_is_set() {
        let auth = StatsAuth {
            user: "railway".into(),
            password: "s3cret".into(),
        };
        let cfg = generate_config(&test_config(Some(auth)), &nodes());

        assert!(cfg.contains("userlist stats_users\n    user \"${HAPROXY_STATS_USER}\" insecure-password \"${HAPROXY_STATS_PASSWORD}\""));
        assert!(cfg.contains("acl LOCALHOST src 127.0.0.1 ::1 ::ffff:127.0.0.1"));
        assert!(cfg.contains("http-request allow if LOCALHOST\n    http-request auth unless { http_auth(stats_users) }\n    stats enable"));
        assert!(!cfg.contains("http-request deny"));
        // The secret itself never lands in the rendered file (it is logged at boot).
        assert!(!cfg.contains("s3cret"));
        assert!(!cfg.contains("ops-user"));
    }

    #[test]
    fn stats_page_denies_remote_clients_without_a_credential() {
        let cfg = generate_config(&test_config(None), &nodes());

        assert!(!cfg.contains("userlist"));
        assert!(cfg
            .contains("http-request allow if LOCALHOST\n    http-request deny\n    stats enable"));
    }

    #[test]
    fn stats_listener_keeps_its_bind_and_uri() {
        let cfg = generate_config(&test_config(None), &nodes());
        assert!(cfg.contains("listen stats\n    bind :::8404 v4v6\n    mode http\n"));
        assert!(cfg.contains("stats uri /stats\n    stats refresh 10s\n"));
    }
}
