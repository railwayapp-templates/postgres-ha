//! Ask etcd whether it accepts this member's credential before Patroni does.
//!
//! etcd's `root` password is fixed the moment the etcd entrypoint first
//! enables authentication. The credential a Postgres member presents is
//! re-derived from its variables on every boot (`PATRONI_ETCD3_PASSWORD`,
//! else the superuser password). Once a password variable is edited after the
//! cluster was created the two disagree, and Patroni stops with
//! `Etcd3 authentication failed` — a line that names neither the variable nor
//! the fix. This module asks etcd the same question first and, on a
//! rejection, stops the member with a message that does.
//!
//! Only an explicit rejection stops the boot. An etcd cluster that has not
//! enabled authentication, an unreachable host, or any other answer leaves
//! the boot alone: Patroni's own retry loop is the right handler for those.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::Credential;

/// Stable first words of the rejection message, for log matching.
pub const REJECTION_PREFIX: &str = "etcd rejected this member's credential";

/// What etcd said about the credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtcdAuthProbe {
    /// etcd issued a token: the credential is right.
    Accepted,
    /// etcd has not enabled authentication; the credential is not checked.
    NotEnabled,
    /// etcd knows the user and refused the password.
    Rejected,
    /// No usable answer (unreachable, timeout, unexpected shape).
    Inconclusive,
}

/// Classify one `/v3/auth/authenticate` answer. etcd's gRPC gateway reports
/// both "not enabled" and "wrong password" as HTTP 400, and etcd 3.5 puts the
/// text under `error` while 3.6 puts it under `message`, so the body text is
/// the only reliable signal.
pub fn classify_auth_response(status: u16, body: &str) -> EtcdAuthProbe {
    let text = body.to_ascii_lowercase();
    if (200..300).contains(&status) {
        return if text.contains("\"token\"") {
            EtcdAuthProbe::Accepted
        } else {
            EtcdAuthProbe::Inconclusive
        };
    }
    if text.contains("authentication is not enabled") {
        return EtcdAuthProbe::NotEnabled;
    }
    if text.contains("authentication failed") {
        return EtcdAuthProbe::Rejected;
    }
    EtcdAuthProbe::Inconclusive
}

/// One verdict for the cluster from the per-host answers. A host that accepts
/// the credential proves it works, so acceptance outranks a rejection from a
/// member that may be behind; a rejection outranks the softer answers.
pub fn combine(probes: impl IntoIterator<Item = EtcdAuthProbe>) -> EtcdAuthProbe {
    let mut verdict = EtcdAuthProbe::Inconclusive;
    for probe in probes {
        verdict = match (verdict, probe) {
            (EtcdAuthProbe::Accepted, _) | (_, EtcdAuthProbe::Accepted) => EtcdAuthProbe::Accepted,
            (EtcdAuthProbe::Rejected, _) | (_, EtcdAuthProbe::Rejected) => EtcdAuthProbe::Rejected,
            (EtcdAuthProbe::NotEnabled, _) | (_, EtcdAuthProbe::NotEnabled) => {
                EtcdAuthProbe::NotEnabled
            }
            _ => EtcdAuthProbe::Inconclusive,
        };
    }
    verdict
}

/// The variable the etcd password was read from: the dedicated one when it is
/// set, else the superuser password (the etcd image enables authentication
/// with that same value).
pub fn etcd_password_source_variable(etcd3_password: Option<&str>) -> &'static str {
    match etcd3_password {
        Some(value) if !value.trim().is_empty() => "PATRONI_ETCD3_PASSWORD",
        _ => "PATRONI_SUPERUSER_PASSWORD",
    }
}

/// The message a member logs when etcd refuses its credential. Names the
/// variable the password came from, the variables that differ from the
/// cluster's pinned credentials (when the credential pin saw the edit), and
/// the fix.
pub fn rejection_message(
    username: &str,
    source_variable: &str,
    drifted_variables: &[&str],
) -> String {
    let mut lines = vec![format!(
        "{REJECTION_PREFIX} (user \"{username}\"): the password in {source_variable} is not the one etcd was created with."
    )];
    lines.push(
        "This cluster's passwords were fixed when it was created; editing a password variable afterwards does not change them. \
         On the postgres-ha template the edited variable is normally POSTGRES_PASSWORD, which PATRONI_SUPERUSER_PASSWORD and the etcd credential are derived from."
            .to_string(),
    );
    if !drifted_variables.is_empty() {
        lines.push(format!(
            "Variables that differ from the credentials this cluster runs with: {}.",
            drifted_variables.join(", ")
        ));
    }
    lines.push(
        "To recover: restore the previous value of the edited variable and redeploy this member. \
         Members that have not restarted keep serving with the original credentials until they do, so restore the value before they restart."
            .to_string(),
    );
    lines.join("\n")
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

/// Ask every etcd host whether it accepts `cred`, and combine the answers.
/// `hosts` is the `PATRONI_ETCD3_HOSTS` list (`host:port,host:port,...`).
pub async fn probe_etcd_credential(
    hosts: &str,
    cred: &Credential,
    timeout: Duration,
) -> EtcdAuthProbe {
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(client) => client,
        Err(e) => {
            warn!(error = %e, "etcd credential pre-flight skipped: could not build an HTTP client");
            return EtcdAuthProbe::Inconclusive;
        }
    };
    let mut probes = Vec::new();
    for host in hosts.split(',').map(str::trim).filter(|h| !h.is_empty()) {
        let probe = probe_host(&client, host, cred).await;
        info!(host, probe = ?probe, "etcd credential pre-flight");
        probes.push(probe);
    }
    combine(probes)
}

async fn probe_host(client: &reqwest::Client, host: &str, cred: &Credential) -> EtcdAuthProbe {
    let url = format!("http://{host}/v3/auth/authenticate");
    let response = match client
        .post(&url)
        .json(&AuthRequest {
            name: &cred.username,
            password: &cred.password,
        })
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            warn!(host, error = %e, "etcd credential pre-flight: host did not answer");
            return EtcdAuthProbe::Inconclusive;
        }
    };
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();
    let probe = classify_auth_response(status, &body);
    if probe == EtcdAuthProbe::Accepted
        && serde_json::from_str::<AuthResponse>(&body)
            .ok()
            .and_then(|r| r.token)
            .is_none()
    {
        return EtcdAuthProbe::Inconclusive;
    }
    probe
}

#[cfg(test)]
mod tests {
    use super::*;

    const ETCD_35_REJECTED: &str = r#"{"error":"etcdserver: authentication failed, invalid user ID or password","code":3,"message":"etcdserver: authentication failed, invalid user ID or password"}"#;
    const ETCD_36_REJECTED: &str =
        r#"{"code":3,"message":"etcdserver: authentication failed, invalid user ID or password"}"#;
    const ETCD_35_NOT_ENABLED: &str = r#"{"error":"etcdserver: authentication is not enabled","code":9,"message":"etcdserver: authentication is not enabled"}"#;
    const ETCD_36_NOT_ENABLED: &str =
        r#"{"code":9,"message":"etcdserver: authentication is not enabled"}"#;

    #[test]
    fn a_token_means_accepted() {
        assert_eq!(
            classify_auth_response(200, r#"{"header":{"cluster_id":"1"},"token":"abc.123"}"#),
            EtcdAuthProbe::Accepted
        );
    }

    #[test]
    fn a_wrong_password_is_rejected_in_both_gateway_shapes() {
        assert_eq!(
            classify_auth_response(400, ETCD_35_REJECTED),
            EtcdAuthProbe::Rejected
        );
        assert_eq!(
            classify_auth_response(400, ETCD_36_REJECTED),
            EtcdAuthProbe::Rejected
        );
    }

    #[test]
    fn auth_not_enabled_is_not_a_rejection_in_both_gateway_shapes() {
        assert_eq!(
            classify_auth_response(400, ETCD_35_NOT_ENABLED),
            EtcdAuthProbe::NotEnabled
        );
        assert_eq!(
            classify_auth_response(400, ETCD_36_NOT_ENABLED),
            EtcdAuthProbe::NotEnabled
        );
    }

    #[test]
    fn anything_else_is_inconclusive() {
        assert_eq!(classify_auth_response(503, ""), EtcdAuthProbe::Inconclusive);
        assert_eq!(
            classify_auth_response(502, "<html>bad gateway</html>"),
            EtcdAuthProbe::Inconclusive
        );
        assert_eq!(
            classify_auth_response(200, r#"{"header":{}}"#),
            EtcdAuthProbe::Inconclusive
        );
        assert_eq!(
            classify_auth_response(
                400,
                r#"{"code":3,"message":"etcdserver: user name is empty"}"#
            ),
            EtcdAuthProbe::Inconclusive
        );
    }

    #[test]
    fn one_accepting_host_outranks_a_rejecting_one() {
        use EtcdAuthProbe::*;
        assert_eq!(combine([Rejected, Accepted, Inconclusive]), Accepted);
        assert_eq!(combine([Inconclusive, Rejected, NotEnabled]), Rejected);
        assert_eq!(combine([Inconclusive, NotEnabled]), NotEnabled);
        assert_eq!(combine([Inconclusive, Inconclusive]), Inconclusive);
        assert_eq!(combine([]), Inconclusive);
    }

    #[test]
    fn the_source_variable_is_the_dedicated_one_only_when_set() {
        assert_eq!(
            etcd_password_source_variable(Some("secret")),
            "PATRONI_ETCD3_PASSWORD"
        );
        assert_eq!(
            etcd_password_source_variable(Some("  ")),
            "PATRONI_SUPERUSER_PASSWORD"
        );
        assert_eq!(
            etcd_password_source_variable(None),
            "PATRONI_SUPERUSER_PASSWORD"
        );
    }

    #[test]
    fn the_message_names_the_variable_the_drift_and_the_fix() {
        let msg = rejection_message(
            "root",
            "PATRONI_SUPERUSER_PASSWORD",
            &["PATRONI_SUPERUSER_PASSWORD", "POSTGRES_PASSWORD"],
        );
        assert!(msg.starts_with(REJECTION_PREFIX));
        assert!(msg.contains("user \"root\""));
        assert!(msg.contains("the password in PATRONI_SUPERUSER_PASSWORD"));
        assert!(msg.contains("Variables that differ from the credentials this cluster runs with: PATRONI_SUPERUSER_PASSWORD, POSTGRES_PASSWORD."));
        assert!(msg.contains(
            "restore the previous value of the edited variable and redeploy this member"
        ));
    }

    #[test]
    fn the_message_without_drift_evidence_still_gives_the_fix() {
        let msg = rejection_message("root", "PATRONI_ETCD3_PASSWORD", &[]);
        assert!(msg.contains("the password in PATRONI_ETCD3_PASSWORD"));
        assert!(!msg.contains("Variables that differ"));
        assert!(msg.contains("restore the previous value"));
    }
}
