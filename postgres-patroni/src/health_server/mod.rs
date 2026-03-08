//! Health server for HAProxy HTTP health checks
//!
//! Provides fast HTTP endpoints for HAProxy to determine PostgreSQL
//! primary/replica status without depending on Patroni or etcd.

mod config;
mod postgres;
mod routes;

pub use config::HealthServerConfig;

use anyhow::Result;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::{error, info};

/// Start the health server on the configured port.
///
/// Returns a JoinHandle that can be used to await the server or abort it.
/// The server runs in a background task and handles requests independently
/// of the main Patroni process.
pub async fn start(config: HealthServerConfig) -> Result<tokio::task::JoinHandle<()>> {
    let port = config.port;
    let app = routes::create_router(config);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await?;

    info!(port, "Health server listening");

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            error!(error = %e, "Health server error");
        }
    });

    Ok(handle)
}
