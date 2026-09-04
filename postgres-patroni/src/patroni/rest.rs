//! HTTP client for the local Patroni REST API.
//!
//! One constructor for every in-image caller so the credential is attached in
//! exactly one place. GET endpoints never require it; the mutating ones
//! (`PATCH /config`, `POST /reinitialize`, ...) do once the member enforces.

use super::config::{restapi_auth_from_env, Credential};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use std::time::Duration;

/// `Authorization: Basic ...` for a REST API credential.
pub(crate) fn basic_auth_header(cred: &Credential) -> Option<HeaderValue> {
    let token = BASE64.encode(format!("{}:{}", cred.username, cred.password));
    let mut value = HeaderValue::from_str(&format!("Basic {token}")).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// A client for `http://localhost:8008` with the process's REST credential
/// preset (none when no password is configured).
pub fn client(timeout: Duration) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    if let Some(value) = restapi_auth_from_env().as_ref().and_then(basic_auth_header) {
        headers.insert(AUTHORIZATION, value);
    }
    reqwest::Client::builder()
        .timeout(timeout)
        .default_headers(headers)
        .build()
        .context("build Patroni REST client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_encodes_username_and_password() {
        let value = basic_auth_header(&Credential {
            username: "postgres".into(),
            password: "pw".into(),
        })
        .unwrap();
        assert_eq!(value.to_str().unwrap(), "Basic cG9zdGdyZXM6cHc=");
        assert!(value.is_sensitive());
    }
}
