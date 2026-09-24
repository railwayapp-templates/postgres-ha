//! HAProxy configuration generator and launcher
//!
//! Generates HAProxy configuration dynamically from PostgreSQL node information
//! in environment variables. Uses direct PostgreSQL health checks via
//! pg_is_in_recovery() to bypass Patroni REST API blocking issues.

mod config;
mod monitoring;
mod nodes;
mod probe;
mod signals;
mod sli;
mod template;

use anyhow::{Context, Result};
use common::{init_logging, Telemetry, TelemetryEvent};
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::process::Command;
use std::sync::{Arc, Mutex};
use tracing::info;

use config::Config;
use monitoring::run_monitoring_loop;
use nodes::parse_nodes;
use template::generate_config;

const CONFIG_FILE: &str = "/usr/local/etc/haproxy/haproxy.cfg";

fn main() -> Result<()> {
    let _guard = init_logging("haproxy");

    let telemetry = Telemetry::from_env("haproxy");
    let config = Config::from_env()?;
    let nodes = parse_nodes(&config.postgres_nodes)?;
    let single_node_mode = nodes.len() == 1;

    info!(
        nodes = %config.postgres_nodes,
        count = nodes.len(),
        "Generating HAProxy config"
    );

    if single_node_mode {
        info!("Single node mode: routing directly without role checks");
    } else if let Some(port) = config.health_port_override {
        info!(port, "Multi-node mode: health checks on overridden port");
    } else {
        info!("Multi-node mode: using Patroni REST API health checks (port 8008)");
    }

    telemetry.send(TelemetryEvent::HaproxyConfigGenerating {
        nodes: nodes.iter().map(|n| n.name.clone()).collect(),
    });

    let haproxy_config = generate_config(&config, &nodes);

    fs::write(CONFIG_FILE, &haproxy_config).context("Failed to write HAProxy config")?;
    info!(path = CONFIG_FILE, "Config written");

    // Log config for debugging
    for line in haproxy_config.lines() {
        info!("  {}", line);
    }

    telemetry.send(TelemetryEvent::HaproxyStarted {
        node_count: nodes.len(),
        single_node_mode,
    });

    info!("Starting HAProxy...");

    let mut haproxy = Command::new("haproxy");
    haproxy.arg("-f").arg(CONFIG_FILE);
    // The stats credential reaches haproxy through its environment, expanded
    // at config parse time — never through the rendered (and logged) file.
    if let Some(auth) = &config.stats_auth {
        haproxy
            .env("HAPROXY_STATS_USER", &auth.user)
            .env("HAPROXY_STATS_PASSWORD", &auth.password);
        info!(user = %auth.user, "Stats page: loopback open, remote clients authenticate");
    } else {
        info!("Stats page: loopback only (no HAPROXY_STATS_PASSWORD / PGPASSWORD)");
    }
    let child = haproxy.spawn().context("Failed to spawn haproxy")?;

    // We are PID 1: the runtime's stop signal lands here and nowhere else.
    // Relay it to haproxy so a stop is a soft stop instead of a SIGKILL after
    // the grace period (see signals.rs).
    signals::install_forwarding(child.id());

    // The uptime SLI's host probe: a login-free handshake on our own 5432,
    // the customer's path through this replica. A lone node is not HA and is
    // not measured (it logs no sli line at all).
    let sli_backends: sli::SharedBackends = Arc::new(Mutex::new(None));
    if !single_node_mode {
        sli::spawn(
            SocketAddr::from((Ipv4Addr::LOCALHOST, 5432)),
            config.probe_user.clone(),
            config.replica_identity.clone(),
            sli_backends.clone(),
        );
    }

    run_monitoring_loop(child, &telemetry, single_node_mode, sli_backends)
}
