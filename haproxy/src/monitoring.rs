//! HAProxy process monitoring
//!
//! Monitors HAProxy backend health and emits telemetry when no primary is available.

use anyhow::Result;
use common::{Telemetry, TelemetryEvent};
use std::process::Child;
use std::thread;
use std::time::Duration;
use tracing::{error, info, warn};

const STATS_URL: &str = "http://localhost:8404/stats;csv";
const CHECK_INTERVAL: Duration = Duration::from_secs(5);

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
        error!(?status, "HAProxy exited");
        std::process::exit(status.code().unwrap_or(1));
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    let mut no_primary_alerted = false;
    let mut no_replica_alerted = false;

    loop {
        // Check if HAProxy is still running
        match child.try_wait() {
            Ok(Some(status)) => {
                error!(?status, "HAProxy exited unexpectedly");
                std::process::exit(status.code().unwrap_or(1));
            }
            Ok(None) => {} // Still running
            Err(e) => {
                error!(error = %e, "Failed to check HAProxy status");
                std::process::exit(1);
            }
        }

        // Check backend health (single request for both primary and replica)
        match check_backend_health(&client) {
            Ok(BackendHealth { primary, replica }) => {
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
                        warn!("No healthy replica backend - no replicas available for reads");
                        telemetry.send(TelemetryEvent::ReplicaUnavailable {
                            node: "haproxy".to_string(),
                            scope: "postgresql_replicas_backend".to_string(),
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

        thread::sleep(CHECK_INTERVAL);
    }
}

struct BackendHealth {
    primary: usize,
    replica: usize,
}

/// Check how many healthy servers are in each backend (single HTTP request)
fn check_backend_health(client: &reqwest::blocking::Client) -> Result<BackendHealth> {
    let resp = client.get(STATS_URL).send()?;
    let body = resp.text()?;

    let mut primary = 0;
    let mut replica = 0;

    // HAProxy CSV format: pxname,svname,status,...
    // pxname is column 0, svname is column 1, status is column 17
    for line in body.lines() {
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() > 17 && parts[1] != "BACKEND" && parts[17] == "UP" {
            match parts[0] {
                "postgresql_primary_backend" => primary += 1,
                "postgresql_replicas_backend" => replica += 1,
                _ => {}
            }
        }
    }

    Ok(BackendHealth { primary, replica })
}
