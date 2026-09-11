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

/// A client with `cred` preset as HTTP Basic on every request (no header when
/// `None`).
pub fn client_for(cred: Option<&Credential>, timeout: Duration) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    if let Some(value) = cred.and_then(basic_auth_header) {
        headers.insert(AUTHORIZATION, value);
    }
    reqwest::Client::builder()
        .timeout(timeout)
        .default_headers(headers)
        .build()
        .context("build Patroni REST client")
}

/// A client for `http://localhost:8008` with the process's REST credential
/// preset (none when no password is configured).
pub fn client(timeout: Duration) -> Result<reqwest::Client> {
    client_for(restapi_auth_from_env().as_ref(), timeout)
}

/// Test support: a one-shot HTTP server on a random loopback port that answers
/// `200` and hands back the raw request head it received.
#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{Read, Write};
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::Mutex;

    /// Serializes tests that set `PATRONI_*` variables in the process.
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Returns the URL to post to and the receiver for the captured request.
    pub(crate) fn capture_one_request(path: &str) -> (String, Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&chunk[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = tx.send(String::from_utf8_lossy(&head).into_owned());
        });
        (format!("http://{addr}{path}"), rx)
    }

    /// The value of header `name` (case-insensitive) in a raw request head.
    pub(crate) fn header_value(request: &str, name: &str) -> Option<String> {
        request.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{capture_one_request, header_value};
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

    #[tokio::test]
    async fn client_for_presents_the_credential_on_the_wire() {
        let (url, rx) = capture_one_request("/reinitialize");
        let client = client_for(
            Some(&Credential {
                username: "postgres".into(),
                password: "pw".into(),
            }),
            Duration::from_secs(5),
        )
        .unwrap();
        client
            .post(&url)
            .json(&serde_json::json!({ "force": true }))
            .send()
            .await
            .unwrap();
        let request = rx.recv().unwrap();
        assert_eq!(
            header_value(&request, "authorization").as_deref(),
            Some("Basic cG9zdGdyZXM6cHc="),
            "{request}"
        );
    }

    #[tokio::test]
    async fn client_for_without_a_credential_sends_no_authorization() {
        let (url, rx) = capture_one_request("/patroni");
        let client = client_for(None, Duration::from_secs(5)).unwrap();
        client.get(&url).send().await.unwrap();
        let request = rx.recv().unwrap();
        assert_eq!(header_value(&request, "authorization"), None, "{request}");
    }
}
