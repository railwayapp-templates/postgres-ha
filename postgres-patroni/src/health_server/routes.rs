//! HTTP route handlers for health checks

use super::config::HealthServerConfig;
use super::postgres::{check_patroni_role, is_in_recovery};
use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Router};
use std::time::Duration;
use tracing::debug;

/// Create the router with all health check endpoints
pub fn create_router(config: HealthServerConfig) -> Router {
    Router::new()
        .route("/primary", get(primary_handler))
        .route("/replica", get(replica_handler))
        .route("/health", get(health_handler))
        .with_state(config)
}

/// Handler for /primary endpoint
///
/// Returns 200 if this node is the primary (pg_is_in_recovery() = false)
/// Returns 503 if this node is a replica or unreachable
/// Falls back to Patroni API if PostgreSQL is unreachable
///
/// The whole check (including the Patroni fallback) is bounded by
/// check_timeout_ms. Without this, is_in_recovery's connect + check_patroni_role's
/// request each carry their own timeout, and since they run sequentially in
/// the fallback path, their worst cases stack -- easily exceeding HAProxy's
/// own timeout_check (default 3s) and turning an otherwise-healthy primary
/// into a Layer7 timeout / "no leader" flap on HAProxy's side.
async fn primary_handler(State(config): State<HealthServerConfig>) -> impl IntoResponse {
    let budget = Duration::from_millis(config.check_timeout_ms);
    match tokio::time::timeout(budget, primary_check(&config)).await {
        Ok(result) => result,
        Err(_) => {
            debug!(timeout_ms = config.check_timeout_ms, "Primary check: TIMEOUT (exceeded check budget)");
            (StatusCode::SERVICE_UNAVAILABLE, "timeout")
        }
    }
}

async fn primary_check(config: &HealthServerConfig) -> (StatusCode, &'static str) {
    match is_in_recovery(config).await {
        Ok(false) => {
            debug!("Primary check: OK (not in recovery)");
            (StatusCode::OK, "primary")
        }
        Ok(true) => {
            debug!("Primary check: FAIL (in recovery)");
            (StatusCode::SERVICE_UNAVAILABLE, "replica")
        }
        Err(e) => {
            debug!(error = %e, "Primary check: PostgreSQL unreachable, falling back to Patroni");
            match check_patroni_role(config, "primary").await {
                Ok(true) => {
                    debug!("Primary check: OK (via Patroni fallback)");
                    (StatusCode::OK, "primary")
                }
                Ok(false) => {
                    debug!("Primary check: FAIL (via Patroni fallback)");
                    (StatusCode::SERVICE_UNAVAILABLE, "replica")
                }
                Err(e) => {
                    debug!(error = %e, "Primary check: FAIL (Patroni also unreachable)");
                    (StatusCode::SERVICE_UNAVAILABLE, "error")
                }
            }
        }
    }
}

/// Handler for /replica endpoint
///
/// Returns 200 if this node is a replica (pg_is_in_recovery() = true)
/// Returns 503 if this node is the primary or unreachable
/// Falls back to Patroni API if PostgreSQL is unreachable
///
/// See primary_handler's doc comment for why this is wrapped in check_timeout_ms.
async fn replica_handler(State(config): State<HealthServerConfig>) -> impl IntoResponse {
    let budget = Duration::from_millis(config.check_timeout_ms);
    match tokio::time::timeout(budget, replica_check(&config)).await {
        Ok(result) => result,
        Err(_) => {
            debug!(timeout_ms = config.check_timeout_ms, "Replica check: TIMEOUT (exceeded check budget)");
            (StatusCode::SERVICE_UNAVAILABLE, "timeout")
        }
    }
}

async fn replica_check(config: &HealthServerConfig) -> (StatusCode, &'static str) {
    match is_in_recovery(config).await {
        Ok(true) => {
            debug!("Replica check: OK (in recovery)");
            (StatusCode::OK, "replica")
        }
        Ok(false) => {
            debug!("Replica check: FAIL (not in recovery)");
            (StatusCode::SERVICE_UNAVAILABLE, "primary")
        }
        Err(e) => {
            debug!(error = %e, "Replica check: PostgreSQL unreachable, falling back to Patroni");
            match check_patroni_role(config, "replica").await {
                Ok(true) => {
                    debug!("Replica check: OK (via Patroni fallback)");
                    (StatusCode::OK, "replica")
                }
                Ok(false) => {
                    debug!("Replica check: FAIL (via Patroni fallback)");
                    (StatusCode::SERVICE_UNAVAILABLE, "primary")
                }
                Err(e) => {
                    debug!(error = %e, "Replica check: FAIL (Patroni also unreachable)");
                    (StatusCode::SERVICE_UNAVAILABLE, "error")
                }
            }
        }
    }
}

/// Handler for /health endpoint
///
/// Returns 200 if PostgreSQL is reachable
/// Returns 503 if unreachable (no Patroni fallback - we want actual PG health)
async fn health_handler(State(config): State<HealthServerConfig>) -> impl IntoResponse {
    let budget = Duration::from_millis(config.check_timeout_ms);
    match tokio::time::timeout(budget, health_check(&config)).await {
        Ok(result) => result,
        Err(_) => {
            debug!(timeout_ms = config.check_timeout_ms, "Health check: TIMEOUT (exceeded check budget)");
            (StatusCode::SERVICE_UNAVAILABLE, "timeout")
        }
    }
}

async fn health_check(config: &HealthServerConfig) -> (StatusCode, &'static str) {
    match is_in_recovery(config).await {
        Ok(in_recovery) => {
            let role = if in_recovery { "replica" } else { "primary" };
            debug!(role, "Health check: OK");
            (StatusCode::OK, role)
        }
        Err(e) => {
            debug!(error = %e, "Health check: FAIL");
            (StatusCode::SERVICE_UNAVAILABLE, "error")
        }
    }
}
