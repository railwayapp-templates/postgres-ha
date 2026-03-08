//! HTTP route handlers for health checks

use super::postgres::{is_in_recovery, PgPool};
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Router,
};
use tracing::debug;

/// Create the router with all health check endpoints
pub fn create_router(pool: PgPool) -> Router {
    Router::new()
        .route("/primary", get(primary_handler))
        .route("/replica", get(replica_handler))
        .route("/health", get(health_handler))
        .with_state(pool)
}

/// Handler for /primary endpoint
///
/// Returns 200 if this node is the primary (pg_is_in_recovery() = false)
/// Returns 503 if this node is a replica or unreachable
async fn primary_handler(State(pool): State<PgPool>) -> impl IntoResponse {
    match is_in_recovery(&pool).await {
        Ok(false) => {
            debug!("Primary check: OK (not in recovery)");
            (StatusCode::OK, "primary")
        }
        Ok(true) => {
            debug!("Primary check: FAIL (in recovery)");
            (StatusCode::SERVICE_UNAVAILABLE, "replica")
        }
        Err(e) => {
            debug!(error = %e, "Primary check: FAIL (error)");
            (StatusCode::SERVICE_UNAVAILABLE, "error")
        }
    }
}

/// Handler for /replica endpoint
///
/// Returns 200 if this node is a replica (pg_is_in_recovery() = true)
/// Returns 503 if this node is the primary or unreachable
async fn replica_handler(State(pool): State<PgPool>) -> impl IntoResponse {
    match is_in_recovery(&pool).await {
        Ok(true) => {
            debug!("Replica check: OK (in recovery)");
            (StatusCode::OK, "replica")
        }
        Ok(false) => {
            debug!("Replica check: FAIL (not in recovery)");
            (StatusCode::SERVICE_UNAVAILABLE, "primary")
        }
        Err(e) => {
            debug!(error = %e, "Replica check: FAIL (error)");
            (StatusCode::SERVICE_UNAVAILABLE, "error")
        }
    }
}

/// Handler for /health endpoint
///
/// Returns 200 if PostgreSQL is reachable
/// Returns 503 if unreachable
async fn health_handler(State(pool): State<PgPool>) -> impl IntoResponse {
    match is_in_recovery(&pool).await {
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
