//! PostgreSQL connection pool and queries

use super::config::HealthServerConfig;
use anyhow::{Context, Result};
use bb8::Pool;
use bb8_postgres::PostgresConnectionManager;
use tokio_postgres::NoTls;
use tracing::debug;

/// Type alias for the connection pool
pub type PgPool = Pool<PostgresConnectionManager<NoTls>>;

/// Create a connection pool for health check queries
pub async fn create_pool(config: &HealthServerConfig) -> Result<PgPool> {
    let connection_string = format!(
        "host={} port={} user={} password={} dbname={} connect_timeout=5",
        config.pg_host,
        config.pg_port,
        config.pg_user,
        config.pg_password,
        config.pg_database
    );

    let manager = PostgresConnectionManager::new_from_stringlike(&connection_string, NoTls)
        .context("Failed to create PostgreSQL connection manager")?;

    let pool = Pool::builder()
        .max_size(2) // Health checks need minimal connections
        .min_idle(Some(1))
        .connection_timeout(std::time::Duration::from_secs(5))
        .build(manager)
        .await
        .context("Failed to create PostgreSQL connection pool")?;

    debug!("PostgreSQL health check connection pool created");
    Ok(pool)
}

/// Check if PostgreSQL is in recovery mode (i.e., is a replica)
///
/// Returns:
/// - Ok(true) if in recovery (replica)
/// - Ok(false) if not in recovery (primary)
/// - Err if unable to connect or query
pub async fn is_in_recovery(pool: &PgPool) -> Result<bool> {
    let conn = pool.get().await.context("Failed to get connection from pool")?;
    let row = conn
        .query_one("SELECT pg_is_in_recovery()", &[])
        .await
        .context("Failed to execute pg_is_in_recovery()")?;

    let in_recovery: bool = row.get(0);
    Ok(in_recovery)
}
