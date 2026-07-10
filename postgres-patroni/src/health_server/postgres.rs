//! PostgreSQL connection and queries

use super::config::HealthServerConfig;
use anyhow::{Context, Result};
use std::time::Duration;
use tokio_postgres::NoTls;
use tracing::debug;

/// Check if PostgreSQL is in recovery mode (i.e., is a replica)
///
/// Returns:
/// - Ok(true) if in recovery (replica)
/// - Ok(false) if not in recovery (primary)
/// - Err if unable to connect or query
pub async fn is_in_recovery(config: &HealthServerConfig) -> Result<bool> {
    // This runs on every HAProxy poll (as often as every 500ms once
    // `fastinter` kicks in), and its failure is what triggers the
    // Patroni/etcd fallback below, which shares the same overall
    // check_timeout_ms budget (see routes.rs). Reserve half that budget for
    // the connect attempt so a slow-to-fail connect can't starve the
    // fallback of its own turn -- derived from config rather than a fixed
    // constant so tuning HEALTH_CHECK_TIMEOUT_MS actually changes this too.
    //
    // Wrapped in tokio::time::timeout rather than libpq's connect_timeout=
    // connparam: that's whole-seconds-only and explicitly documented as
    // unreliable under 2s, too coarse for a sub-second budget.
    let stage_budget = Duration::from_millis(config.check_timeout_ms / 2);

    let connection_string = format!(
        "host=localhost port={} user={} password={} dbname={}",
        config.pg_port, config.pg_user, config.pg_password, config.pg_database
    );

    let (client, connection) = tokio::time::timeout(
        stage_budget,
        tokio_postgres::connect(&connection_string, NoTls),
    )
    .await
    .context("Timed out connecting to PostgreSQL")?
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
    // is_in_recovery's stage_budget above (and derived from config the same
    // way), it shares the outer check_timeout_ms budget with the
    // already-failed direct check that ran before it.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(config.check_timeout_ms / 2))
        .build()
        .context("Failed to create HTTP client")?;

    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to connect to Patroni")?;

    Ok(response.status().is_success())
}
