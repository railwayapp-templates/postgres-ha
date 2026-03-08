//! PostgreSQL connection and queries

use super::config::HealthServerConfig;
use anyhow::{Context, Result};
use tokio_postgres::NoTls;

/// Check if PostgreSQL is in recovery mode (i.e., is a replica)
///
/// Returns:
/// - Ok(true) if in recovery (replica)
/// - Ok(false) if not in recovery (primary)
/// - Err if unable to connect or query
pub async fn is_in_recovery(config: &HealthServerConfig) -> Result<bool> {
    let connection_string = format!(
        "host={} port={} user={} password={} dbname={} connect_timeout=5",
        config.pg_host, config.pg_port, config.pg_user, config.pg_password, config.pg_database
    );

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .context("Failed to connect to PostgreSQL")?;

    // Spawn connection handler - it will terminate when client is dropped
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::debug!(error = %e, "PostgreSQL connection closed");
        }
    });

    let row = client
        .query_one("SELECT pg_is_in_recovery()", &[])
        .await
        .context("Failed to execute pg_is_in_recovery()")?;

    let in_recovery: bool = row.get(0);
    Ok(in_recovery)
}

/// Fallback: check role via Patroni REST API
///
/// Returns Ok(true) if the endpoint returns 200, Ok(false) otherwise.
/// Returns Err only on network/request failures.
pub async fn check_patroni_role(config: &HealthServerConfig, role: &str) -> Result<bool> {
    let url = format!("http://{}:{}/{}", config.pg_host, config.patroni_port, role);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("Failed to create HTTP client")?;

    let response = client
        .get(&url)
        .send()
        .await
        .context("Failed to connect to Patroni")?;

    Ok(response.status().is_success())
}
