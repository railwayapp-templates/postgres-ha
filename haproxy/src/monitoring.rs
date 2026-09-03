//! HAProxy process monitoring
//!
//! Monitors HAProxy backend health and emits telemetry when no primary is available.

use crate::signals;
use anyhow::Result;
use common::{Telemetry, TelemetryEvent};
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

const STATS_URL: &str = "http://localhost:8404/stats;csv";
const CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// How often the loop looks for haproxy's exit between health checks. Short
/// on purpose: after a forwarded stop signal, haproxy's exit is what ends the
/// container, and the runtime's grace period is 10s — noticing it up to a
/// whole CHECK_INTERVAL late would eat most of that budget.
const EXIT_POLL: Duration = Duration::from_millis(200);

/// End the entrypoint with haproxy's own exit status.
fn exit_after_haproxy(status: ExitStatus) -> ! {
    let code = signals::exit_code_for(status.code(), status.signal());
    if signals::stop_requested() {
        info!(
            ?status,
            exit_code = code,
            "HAProxy exited after the requested stop"
        );
    } else {
        error!(?status, exit_code = code, "HAProxy exited unexpectedly");
    }
    std::process::exit(code);
}

/// Run the monitoring loop for HAProxy
///
/// Monitors:
/// - HAProxy process health
/// - Backend availability (emits telemetry when no primary available)
pub fn run_monitoring_loop(
    mut child: Child,
    telemetry: &Telemetry,
    single_node_mode: bool,
) -> Result<()> {
    let pid = child.id();
    info!(pid, "HAProxy started, beginning monitoring");

    // Skip backend monitoring in single node mode - no Patroni health checks
    if single_node_mode {
        info!("Single node mode: skipping backend health monitoring");
        let status = child.wait()?;
        exit_after_haproxy(status);
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    let mut no_primary_alerted = false;
    let mut no_replica_alerted = false;

    loop {
        // Wait out the check interval in short slices, watching for haproxy's
        // exit the whole time (thread::sleep is not interrupted by the signal
        // handler — it resumes after EINTR — so a single long sleep would
        // delay noticing the exit that follows a forwarded stop).
        let next_check = Instant::now() + CHECK_INTERVAL;
        while Instant::now() < next_check {
            match child.try_wait() {
                Ok(Some(status)) => exit_after_haproxy(status),
                Ok(None) => {} // Still running
                Err(e) => {
                    error!(error = %e, "Failed to check HAProxy status");
                    std::process::exit(1);
                }
            }
            thread::sleep(EXIT_POLL);
        }

        // Once a stop has been relayed the stats socket is going away with
        // haproxy; a failed health check then is shutdown, not an alert.
        if signals::stop_requested() {
            continue;
        }

        // Check backend health (single request for both primary and replica)
        match check_backend_health(&client) {
            Ok(BackendHealth {
                primary,
                replica,
                down_replicas,
            }) => {
                // Handle primary backend
                if primary == 0 {
                    if !no_primary_alerted {
                        warn!("No healthy primary backend - cluster has no leader");
                        telemetry.send(TelemetryEvent::DcsUnavailable {
                            node: "haproxy".to_string(),
                            scope: "postgresql_primary_backend".to_string(),
                        });
                        no_primary_alerted = true;
                    }
                } else {
                    if no_primary_alerted {
                        info!(healthy_count = primary, "Primary backend recovered");
                    }
                    no_primary_alerted = false;
                }

                // Handle replica backend
                if replica == 0 {
                    if !no_replica_alerted {
                        warn!(servers = ?down_replicas, "No healthy replica backend - no replicas available for reads");
                        telemetry.send(TelemetryEvent::ReplicaUnavailable {
                            node: "haproxy".to_string(),
                            scope: "postgresql_replicas_backend".to_string(),
                            servers: down_replicas,
                        });
                        no_replica_alerted = true;
                    }
                } else {
                    if no_replica_alerted {
                        info!(healthy_count = replica, "Replica backend recovered");
                    }
                    no_replica_alerted = false;
                }
            }
            Err(e) => {
                warn!(error = %e, "Failed to check backend health");
            }
        }
    }
}

struct BackendHealth {
    primary: usize,
    replica: usize,
    down_replicas: Vec<String>,
}

/// Check how many healthy servers are in each backend (single HTTP request)
fn check_backend_health(client: &reqwest::blocking::Client) -> Result<BackendHealth> {
    let resp = client.get(STATS_URL).send()?;
    let body = resp.text()?;

    let mut primary = 0;
    let mut replica = 0;
    let mut down_replicas = Vec::new();

    // HAProxy CSV format: pxname,svname,status,...
    // pxname is column 0, svname is column 1, status is column 17.
    // Transitional statuses (haproxy stats-proxy.c, srv_hlt_st): a server
    // that is UP but failing checks reports "UP n/m" (up, going down); a
    // server that is DOWN but passing checks reports "DOWN n/m" (down,
    // going up). starts_with("UP") therefore counts exactly the set
    // HAProxy still routes to. A mid-rise server is still down for
    // routing, so it stays in down_replicas — its raw status is attached
    // to the alert so a rising replica reads as recovering, not dead.
    for line in body.lines() {
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() > 17 && parts[1] != "BACKEND" {
            match parts[0] {
                "postgresql_primary_backend" => {
                    if parts[17].starts_with("UP") {
                        primary += 1;
                    }
                }
                "postgresql_replicas_backend" => {
                    if parts[17].starts_with("UP") {
                        replica += 1;
                    } else {
                        down_replicas.push(format!("{} ({})", parts[1], parts[17]));
                    }
                }
                _ => {}
            }
        }
    }

    Ok(BackendHealth {
        primary,
        replica,
        down_replicas,
    })
}
