//! The passwords this member runs with right now, for the whole process.
//!
//! The runner resolves its credentials once at boot (`Config::from_env`, then
//! the credential pin) and hands clones of that `Config` to the watchers it
//! spawns, while the REST client reads `PATRONI_*` variables on every call.
//! A live rotation (`POST /credentials/rotate`) changes the password after
//! all of that happened, so the new value needs one place every caller reads
//! at call time. This is that place: seeded from the post-pin config, updated
//! by the rotation, consulted by the REST client (`config::restapi_auth_from_env`)
//! and by the wrapper-side psql calls that authenticate with the superuser
//! password.
//!
//! Until a rotation happens the REST and etcd overrides are `None` and the
//! variables stay authoritative, so a member that never rotates behaves as
//! before.

use std::sync::RwLock;

use super::Config;

/// The passwords in force. Role passwords mirror the credential pin; the
/// control-plane passwords are `Some` only after a rotation set them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveCredentials {
    pub superuser_pass: String,
    pub repl_pass: String,
    pub app_pass: String,
    /// The REST API password after a rotation; `None` until then (the
    /// variables decide).
    pub rest_password: Option<String>,
    /// The etcd password after a rotation; `None` until then.
    pub etcd_password: Option<String>,
}

static LIVE: RwLock<Option<LiveCredentials>> = RwLock::new(None);

/// Record the post-pin passwords as the ones in force. Called once by the
/// runner after `apply_credential_pin`.
pub fn seed(config: &Config) {
    let mut guard = LIVE.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(LiveCredentials {
        superuser_pass: config.superuser_pass.clone(),
        repl_pass: config.repl_pass.clone(),
        app_pass: config.app_pass.clone(),
        rest_password: None,
        etcd_password: None,
    });
}

/// The passwords in force, `None` before the runner seeded them.
pub fn snapshot() -> Option<LiveCredentials> {
    LIVE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The superuser password in force, for the wrapper's own psql calls.
pub fn role_password() -> Option<String> {
    snapshot().map(|live| live.superuser_pass)
}

/// The REST API password a rotation put in force, if any.
pub fn rest_password_override() -> Option<String> {
    snapshot().and_then(|live| live.rest_password)
}

/// A rotation moved every credential to `password`: the three roles, the
/// REST API when this member has one, etcd when this member authenticates
/// to it. The cluster keeps one password.
pub fn set_rotated(password: &str, has_rest_credential: bool, has_etcd_credential: bool) {
    let mut guard = LIVE.write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(LiveCredentials {
        superuser_pass: password.to_string(),
        repl_pass: password.to_string(),
        app_pass: password.to_string(),
        rest_password: has_rest_credential.then(|| password.to_string()),
        etcd_password: has_etcd_credential.then(|| password.to_string()),
    });
}

/// Overlay `live` onto a freshly built `Config` (whose passwords are the
/// variables' values) so it describes what this member runs with.
pub fn apply_to(config: &mut Config, live: &LiveCredentials) {
    config.superuser_pass = live.superuser_pass.clone();
    config.repl_pass = live.repl_pass.clone();
    config.app_pass = live.app_pass.clone();
    if let (Some(cred), Some(password)) = (config.restapi_auth.as_mut(), &live.rest_password) {
        cred.password = password.clone();
    }
    if let (Some(cred), Some(password)) = (config.etcd_auth.as_mut(), &live.etcd_password) {
        cred.password = password.clone();
    }
}

/// Test support: forget the seeded state so tests do not leak into each other.
#[cfg(test)]
pub fn reset() {
    *LIVE.write().unwrap_or_else(|e| e.into_inner()) = None;
}
