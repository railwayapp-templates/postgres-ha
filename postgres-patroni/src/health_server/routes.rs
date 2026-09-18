//! HTTP route handlers for health checks

use super::config::HealthServerConfig;
use super::postgres::{check_patroni_role, is_in_recovery};
use crate::patroni::rotation::{self, MemberPorts, RotateRequest};
use axum::body::Bytes;
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use std::time::Duration;
use tracing::{debug, warn};

/// Create the router with all health check endpoints
///
/// The GET routes stay open: HAProxy builds both backends from /primary and
/// /replica. `POST /credentials/rotate` is the one mutating route and
/// requires the member's REST API credential (see `patroni::rotation`).
pub fn create_router(config: HealthServerConfig) -> Router {
    Router::new()
        .route("/primary", get(primary_handler))
        .route("/replica", get(replica_handler))
        .route("/health", get(health_handler))
        .route(rotation::ROUTE, post(rotate_handler))
        .with_state(config)
}

/// Handler for `POST /credentials/rotate`
///
/// Basic auth with the member's current REST API credential, JSON body
/// `{"password": "<new>"}`. 200 with a summary when the member moved (or
/// already ran on that password), 401 without the credential, 400 on a bad
/// body, 409 when a role does not carry the password yet, 503 when the
/// verifier cannot be read, 500 when the switch failed and was undone.
async fn rotate_handler(
    State(config): State<HealthServerConfig>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let authorization = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !rotation::authorize(authorization) {
        warn!("credentials/rotate refused: missing or wrong REST API credential");
        return (
            StatusCode::UNAUTHORIZED,
            [(WWW_AUTHENTICATE, "Basic realm=\"postgres-ha\"")],
            Json(serde_json::json!({ "error": rotation::RotateError::Unauthorized.message() })),
        )
            .into_response();
    }
    let request: RotateRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("body must be {{\"password\": \"...\"}}: {e}") })),
            )
                .into_response()
        }
    };
    let ports = MemberPorts {
        pg_port: config.pg_port,
        patroni_port: config.patroni_port,
    };
    match rotation::rotate(&request.password, &ports).await {
        Ok(summary) => (StatusCode::OK, Json(summary)).into_response(),
        Err(e) => {
            let status =
                StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            warn!(status = status.as_u16(), error = %e.message(), "credentials/rotate did not rotate");
            (status, Json(serde_json::json!({ "error": e.message() }))).into_response()
        }
    }
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
            debug!(
                timeout_ms = config.check_timeout_ms,
                "Primary check: TIMEOUT (exceeded check budget)"
            );
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
            debug!(
                timeout_ms = config.check_timeout_ms,
                "Replica check: TIMEOUT (exceeded check budget)"
            );
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
            debug!(
                timeout_ms = config.check_timeout_ms,
                "Health check: TIMEOUT (exceeded check budget)"
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patroni::live_credentials;
    use crate::patroni::rest::test_support::ENV_LOCK;
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

    fn test_config() -> HealthServerConfig {
        HealthServerConfig {
            port: 0,
            pg_port: 5432,
            pg_user: "postgres".into(),
            pg_password: String::new(),
            pg_database: "postgres".into(),
            patroni_port: 8008,
            check_timeout_ms: 2000,
        }
    }

    async fn serve() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, create_router(test_config()))
                .await
                .unwrap();
        });
        format!("http://{addr}")
    }

    fn basic(user: &str, pass: &str) -> String {
        format!("Basic {}", BASE64.encode(format!("{user}:{pass}")))
    }

    #[tokio::test]
    async fn rotate_route_requires_the_rest_credential_and_reads_the_body() {
        let _lock = ENV_LOCK.lock().unwrap();
        live_credentials::reset();
        std::env::set_var("PATRONI_SUPERUSER_USERNAME", "postgres");
        std::env::set_var("PATRONI_SUPERUSER_PASSWORD", "su");
        std::env::set_var("PATRONI_RESTAPI_PASSWORD", "rest");
        live_credentials::set_rotated("current", true, true);
        let base = serve().await;
        let client = reqwest::Client::new();
        let url = format!("{base}{}", rotation::ROUTE);

        // No credential: 401 and a challenge.
        let response = client
            .post(&url)
            .json(&serde_json::json!({ "password": "x" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401);
        assert!(response.headers().contains_key(WWW_AUTHENTICATE));

        // Wrong credential (the superuser password is not the REST one here).
        let response = client
            .post(&url)
            .header(AUTHORIZATION, basic("postgres", "su"))
            .json(&serde_json::json!({ "password": "x" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401);

        // Right credential, unusable body: 400.
        let response = client
            .post(&url)
            .header(AUTHORIZATION, basic("postgres", "current"))
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 400);

        // Right credential, the password already in force: 200 "already",
        // nothing touched (no Postgres or Patroni needed).
        let response = client
            .post(&url)
            .header(AUTHORIZATION, basic("postgres", "current"))
            .json(&serde_json::json!({ "password": "current" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["status"], "already");
        assert_eq!(body["reloaded"], false);

        // The GET routes stay open.
        let response = client.get(format!("{base}/health")).send().await.unwrap();
        assert_ne!(response.status().as_u16(), 401);

        live_credentials::reset();
        std::env::remove_var("PATRONI_RESTAPI_PASSWORD");
        std::env::remove_var("PATRONI_SUPERUSER_PASSWORD");
        std::env::remove_var("PATRONI_SUPERUSER_USERNAME");
    }
}
