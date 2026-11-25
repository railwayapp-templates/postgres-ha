//! Reads through etcd's v3 HTTP gateway, authenticated where the cluster is.

use super::config::Credential;
use serde::{Deserialize, Serialize};

/// Request body for etcd v3 range API
#[derive(Serialize)]
struct RangeRequest {
    key: String,
}

#[derive(Serialize)]
struct AuthRequest<'a> {
    name: &'a str,
    password: &'a str,
}

#[derive(Deserialize)]
struct AuthResponse {
    #[serde(default)]
    token: Option<String>,
}

/// Token for etcd's HTTP gateway, or `None` when the cluster has not enabled
/// authentication (the request is then accepted without one). Any other
/// failure also yields `None`; the following request surfaces the real error.
pub async fn etcd_gateway_token(
    client: &reqwest::Client,
    host: &str,
    cred: Option<&Credential>,
) -> Option<String> {
    let cred = cred?;
    let url = format!("http://{}/v3/auth/authenticate", host.trim());
    let resp = client
        .post(&url)
        .json(&AuthRequest {
            name: &cred.username,
            password: &cred.password,
        })
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<AuthResponse>().await.ok()?.token
}

/// `POST /v3/kv/range` for one key on one etcd host, authenticating first when
/// a credential is configured.
pub async fn etcd_range(
    client: &reqwest::Client,
    host: &str,
    key_base64: &str,
    cred: Option<&Credential>,
) -> reqwest::Result<reqwest::Response> {
    let url = format!("http://{}/v3/kv/range", host.trim());
    let mut req = client.post(&url).json(&RangeRequest {
        key: key_base64.to_string(),
    });
    if let Some(token) = etcd_gateway_token(client, host, cred).await {
        req = req.header(reqwest::header::AUTHORIZATION, token);
    }
    req.send().await
}
