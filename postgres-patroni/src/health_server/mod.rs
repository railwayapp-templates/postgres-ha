//! Health server for HAProxy HTTP health checks
//!
//! Provides fast HTTP endpoints for HAProxy to determine PostgreSQL
//! primary/replica status without depending on Patroni or etcd.

mod config;
mod postgres;
mod routes;

pub use config::HealthServerConfig;

use anyhow::{Context, Result};
use common::{Telemetry, TelemetryEvent};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tracing::{info, warn};

async fn run(config: HealthServerConfig) -> Result<()> {
    let port = config.port;
    let patroni_port = config.patroni_port;
    let app = routes::create_router(config);

    let addr = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr)
        .await
        .context("health server bind failed")?;

    info!(
        port,
        patroni_fallback_port = patroni_port,
        "Health server listening (endpoints: /primary, /replica, /health)"
    );

    axum::serve(listener, app)
        .await
        .context("health server exited")?;
    Ok(())
}

// A run that stayed up at least this long was healthy in between — the next
// failure is a new incident, not a continuation of the same crash loop, and
// earns its own telemetry event. Same thresholds as redis-ha's supervisor.
const HEALTHY_RUN_THRESHOLD: Duration = Duration::from_secs(60);
const RESPAWN_DELAY: Duration = Duration::from_secs(5);

/// Spawn the health server as a supervised background task.
///
/// Mirrors redis-ha's health-server supervisor. HAProxy probes /primary and
/// /replica on this server to build BOTH backends, so a dead health server
/// silently pulls the node from read AND write rotation with Postgres and
/// Patroni perfectly healthy — the previous shape (one `tokio::spawn` whose
/// JoinHandle the caller discarded, inner error only logged) left it dead
/// for good. Each attempt runs in its own spawned task so a panic surfaces
/// as a caught JoinError instead of killing the supervision loop; bind and
/// serve failures respawn after a delay; telemetry is deduped per incident
/// so a crash loop pages once, not once per respawn.
pub fn spawn(config: HealthServerConfig, telemetry: Telemetry) {
    tokio::spawn(async move {
        let mut alerted_for_current_incident = false;

        loop {
            let attempt_config = config.clone();
            let started_at = Instant::now();
            let handle = tokio::task::spawn(async move { run(attempt_config).await });
            let outcome = handle.await;
            let ran_for = started_at.elapsed();

            let failure = match outcome {
                Ok(Ok(())) => {
                    // axum::serve only returns on a graceful-shutdown signal
                    // we never send — unexpected, but the answer is the same.
                    warn!("health server returned cleanly — respawning in 5s");
                    "run loop returned cleanly".to_string()
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "health server bind/serve failed — respawning in 5s");
                    format!("bind/serve failed: {e:#}")
                }
                Err(e) if e.is_panic() => {
                    warn!(panic = ?e, "health server panicked — respawning in 5s");
                    "task panicked".to_string()
                }
                Err(e) => {
                    warn!(error = %e, "health server task cancelled — respawning in 5s");
                    "task cancelled".to_string()
                }
            };

            if ran_for >= HEALTHY_RUN_THRESHOLD {
                alerted_for_current_incident = false;
            }
            if !alerted_for_current_incident {
                alerted_for_current_incident = true;
                telemetry.send(TelemetryEvent::ComponentError {
                    component: "postgres-patroni".to_string(),
                    error: failure,
                    context: "health_server".to_string(),
                });
            }

            tokio::time::sleep(RESPAWN_DELAY).await;
        }
    });
}
