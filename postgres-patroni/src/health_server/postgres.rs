//! PostgreSQL connection and queries

use super::config::HealthServerConfig;
use anyhow::{Context, Result};
use tokio_postgres::NoTls;
use tracing::debug;

/// Check if PostgreSQL is in recovery mode (i.e., is a replica)
///
/// Returns:
/// - Ok(true) if in recovery (replica)
/// - Ok(false) if not in recovery (primary)
/// - Err if unable to connect or query
pub async fn is_in_recovery(config: &HealthServerConfig) -> Result<bool> {
    // connect_timeout=1: this runs on every HAProxy poll (as often as every
    // 500ms once `fastinter` kicks in), and its failure is what triggers the
    // Patroni/etcd fallback below. It must fail fast, not slow -- a generous
    // timeout here just eats into the check_timeout_ms budget that's meant to
    // cover both stages combined.
    let connection_string = format!(
        "host=localhost port={} user={} password={} dbname={} connect_timeout=1",
        config.pg_port, config.pg_user, config.pg_password, config.pg_database
    );

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .context("Failed to connect to PostgreSQL")?;

    // The connection future handles network I/O and must be running for
    // the client to work. It completes when the client is dropped.
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            debug!(error = %e, "PostgreSQL connection closed");
        }
    });

    let row = client
        .query_one("SELECT pg_is_in_recovery()", &[])
        .await
        .context("Failed to execute pg_is_in_recovery()")?;

    let in_recovery: bool = row.get(0);
    Ok(in_recovery)
}

/// Fallback: check role via local Patroni REST API
///
/// Patroni knows the cluster state from etcd, so we just ask it.
/// Returns Ok(true) if the endpoint returns 200, Ok(false) otherwise.
pub async fn check_patroni_role(config: &HealthServerConfig, role: &str) -> Result<bool> {
    let url = format!("http://localhost:{}/{}", config.patroni_port, role);

    // Patroni's own answer is gated on its DCS (etcd) state, so this request
    // can legitimately block for as long as etcd is slow to respond to
    // Patroni's internal loop. Keep this short -- same reasoning as
    // is_in_recovery's connect_timeout above, it shares the outer
    // check_timeout_ms budget with the (already-failed) direct check.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1))
        .build()
        .context("Failed to create HTTP client")?;

    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to connect to Patroni")?;

    Ok(response.status().is_success())
}
