//! HTTP client for the local Patroni REST API.
//!
//! One constructor for every in-image caller so the credential is attached in
//! exactly one place. GET endpoints never require it; the mutating ones
//! (`PATCH /config`, `POST /reinitialize`, ...) do once the member enforces.

use super::config::{restapi_auth_from_env, Credential};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use std::sync::OnceLock;
use std::time::Duration;

/// The credential this process actually authenticates with, set once by
/// `patroni_runner::main` right after `apply_credential_pin` resolves it.
///
/// Every in-image caller of [`client`] (self-heal, slot-recovery, the backup
/// watcher, the archive-config reconcile task) must see that PINNED value,
/// not a fresh re-read of `PATRONI_RESTAPI_PASSWORD` from the environment:
/// once a drifted variable is pinned away (see `patroni::credential_pin`),
/// this member's own REST server enforces the PINNED credential, and a
/// caller still authenticating with the drifted one gets 401'd by its own
/// node — silently wedging whichever of those tasks hits an enforcing
/// endpoint next, forever (they retry, they never re-derive the credential).
static PINNED_RESTAPI_AUTH: OnceLock<Option<Credential>> = OnceLock::new();

/// Record the credential this process runs with, once. Later calls are
/// no-ops (`OnceLock` semantics) — there is exactly one legitimate write,
/// made by `patroni_runner::main` right after the credential pin resolves.
pub fn set_pinned_restapi_auth(cred: Option<Credential>) {
    let _ = PINNED_RESTAPI_AUTH.set(cred);
}

/// `Authorization: Basic ...` for a REST API credential.
pub(crate) fn basic_auth_header(cred: &Credential) -> Option<HeaderValue> {
    let token = BASE64.encode(format!("{}:{}", cred.username, cred.password));
    let mut value = HeaderValue::from_str(&format!("Basic {token}")).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// A client for `http://localhost:8008` with the process's REST credential
/// preset (none when no password is configured).
///
/// Prefers the credential pinned via [`set_pinned_restapi_auth`]; falls back
/// to a fresh environment read only for the window before
/// `patroni_runner::main` has set it. There is no legitimate caller in that
/// window today, and a raw env read is still the right value there anyway —
/// on a fresh volume (or before any credential has ever drifted) the pin and
/// the environment agree.
pub fn client(timeout: Duration) -> Result<reqwest::Client> {
    let cred = match PINNED_RESTAPI_AUTH.get() {
        Some(pinned) => pinned.clone(),
        None => restapi_auth_from_env(),
    };
    let mut headers = HeaderMap::new();
    if let Some(value) = cred.as_ref().and_then(basic_auth_header) {
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
