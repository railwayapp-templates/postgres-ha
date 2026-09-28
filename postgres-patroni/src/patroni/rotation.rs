//! Move this member to a new cluster password while it runs:
//! `POST /credentials/rotate` on the health server.
//!
//! The cluster keeps one password. The control plane rotates it in a fixed
//! order: `ALTER ROLE` for the superuser, the replication user and the
//! application user on the primary, then etcd's `root` password, then this
//! route on the leader and on each replica in turn, then the variable. The
//! route's job is the member's own copies of that password: patroni.yml (the
//! superuser, replication, etcd3 and REST credentials), the credential pin on
//! the volume, Patroni's running configuration through `POST /reload`, and
//! the passwords the wrapper itself uses from here on.
//!
//! patroni.yml is the only source Patroni reads these passwords from: the
//! runner withholds the `PATRONI_*_PASSWORD` variables from Patroni's
//! environment (Patroni applies its environment over the file, which would
//! keep the old password alive across a reload). So a rewrite plus a reload
//! is a complete switch: Patroni re-reads the etcd credential and
//! re-authenticates, moves its REST authentication key, and rewrites its
//! pgpass and `primary_conninfo` on the next HA loop.
//!
//! The route refuses to switch a member to a password its roles do not
//! carry yet (409) by checking the candidate against the roles' verifiers in
//! `pg_authid` (see [`super::scram`]); a member that already runs on the new
//! password answers 200 without touching anything, so a retried call is
//! harmless.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_postgres::NoTls;
use tracing::{info, warn};

use super::config::{apply_one_password, restapi_auth_from_env, Credential};
use super::credential_pin::{read_credential_pin, write_credential_pin, PinnedCredentials};
use super::live_credentials;
use super::scram::{self, VerifierCheck};
use super::{generate_patroni_config, Config};
use crate::pgbackrest::read_wal_level;

/// Path of the route on the health server.
pub const ROUTE: &str = "/credentials/rotate";

const PATRONI_YML: &str = "/etc/patroni/patroni.yml";
const PG_SOCKET_DIR: &str = "/var/run/postgresql";
const VERIFIER_QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const RELOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the member may take to show a DCS contact after the reload.
/// Patroni applies a reload at the start of its next HA loop (`loop_wait`,
/// 10 s here) and re-authenticates to etcd on the request after that; with
/// `retry_timeout` at 17 s, two loops fit comfortably.
const DCS_SESSION_TIMEOUT: Duration = Duration::from_secs(90);
const DCS_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// One rotation at a time per process; a second caller waits and then sees
/// the member already on the new password.
static ROTATION_GUARD: Mutex<()> = Mutex::const_new(());

/// Request body.
#[derive(Debug, Deserialize)]
pub struct RotateRequest {
    pub password: String,
}

/// Response body on success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RotateSummary {
    /// `already`: the member was on this password; `rotated`: it moved.
    pub status: &'static str,
    /// Whether Patroni was asked to reload (false for `already`).
    pub reloaded: bool,
    /// Milliseconds from the reload until Patroni reported a DCS contact.
    pub dcs_seen_after_ms: Option<u64>,
}

/// Why a rotation was refused or failed, with the HTTP status it maps to.
#[derive(Debug)]
pub enum RotateError {
    /// Missing or wrong Basic credential, or no REST credential configured.
    Unauthorized,
    /// Empty or unusable request.
    BadRequest(String),
    /// A role still carries another password: `ALTER ROLE` has not landed on
    /// this member yet (or at all).
    RoleNotRotated(String),
    /// The verifier could not be read (Postgres not reachable on this member).
    Unverifiable(String),
    /// The switch was attempted and undone.
    Failed(String),
}

impl RotateError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Unauthorized => 401,
            Self::BadRequest(_) => 400,
            Self::RoleNotRotated(_) => 409,
            Self::Unverifiable(_) => 503,
            Self::Failed(_) => 500,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Unauthorized => {
                "the request must carry this member's current REST API credential".to_string()
            }
            Self::BadRequest(m) => m.clone(),
            Self::RoleNotRotated(role) => format!(
                "role \"{role}\" does not carry the requested password yet: run ALTER ROLE on the primary (and let it replicate) before rotating this member"
            ),
            Self::Unverifiable(m) => format!("could not read the roles' verifiers: {m}"),
            Self::Failed(m) => format!("rotation failed and was undone: {m}"),
        }
    }
}

/// Whether an `Authorization` header carries this member's current REST API
/// credential. A member with no REST credential at all refuses every call:
/// there is nothing to check the caller against.
pub fn authorize(authorization: Option<&str>) -> bool {
    let Some(expected) = restapi_auth_from_env() else {
        return false;
    };
    let Some(presented) = parse_basic(authorization) else {
        return false;
    };
    constant_time_eq(presented.username.as_bytes(), expected.username.as_bytes())
        && constant_time_eq(presented.password.as_bytes(), expected.password.as_bytes())
}

/// The credential in a `Basic` header, if well formed.
pub fn parse_basic(authorization: Option<&str>) -> Option<Credential> {
    let encoded = authorization?.trim().strip_prefix("Basic ")?;
    let decoded = BASE64.decode(encoded.trim()).ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (username, password) = decoded.split_once(':')?;
    Some(Credential {
        username: username.to_string(),
        password: password.to_string(),
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && openssl::memcmp::eq(a, b)
}

/// Where this member's Postgres and Patroni answer.
#[derive(Debug, Clone)]
pub struct MemberPorts {
    pub pg_port: u16,
    pub patroni_port: u16,
}

/// Rotate this member to `new_password`. See the module doc for the steps.
pub async fn rotate(new_password: &str, ports: &MemberPorts) -> Result<RotateSummary, RotateError> {
    if new_password.is_empty() {
        return Err(RotateError::BadRequest("password must not be empty".into()));
    }
    let _serialized = ROTATION_GUARD.lock().await;

    let live = live_credentials::snapshot().ok_or_else(|| {
        RotateError::Failed("the runner has not recorded this member's credentials yet".into())
    })?;
    if live.superuser_pass == new_password
        && live.repl_pass == new_password
        && live.app_pass == new_password
    {
        return Ok(RotateSummary {
            status: "already",
            reloaded: false,
            dcs_seen_after_ms: None,
        });
    }

    let mut current = Config::from_env().map_err(|e| RotateError::Failed(format!("{e:#}")))?;
    live_credentials::apply_to(&mut current, &live);
    let mut next = Config::from_env().map_err(|e| RotateError::Failed(format!("{e:#}")))?;
    live_credentials::apply_to(&mut next, &live);
    apply_one_password(&mut next, new_password);

    // The roles must already carry the password, on this member's copy of
    // the catalog: the primary after ALTER ROLE, a replica once it replayed.
    let mut roles = vec![current.superuser.clone(), current.repl_user.clone()];
    if current.app_user != current.superuser {
        roles.push(current.app_user.clone());
    }
    for role in &roles {
        let verifier = read_role_verifier(role, &current.superuser, ports.pg_port)
            .await
            .map_err(|e| RotateError::Unverifiable(format!("{e:#}")))?;
        let Some(verifier) = verifier else {
            return Err(RotateError::Unverifiable(format!(
                "role \"{role}\" has no password verifier in pg_authid"
            )));
        };
        match scram::verify(&verifier, role, new_password) {
            VerifierCheck::Matches => {}
            VerifierCheck::Differs => return Err(RotateError::RoleNotRotated(role.clone())),
            VerifierCheck::Unsupported(why) => {
                return Err(RotateError::Failed(format!(
                    "cannot check role \"{role}\": {why}"
                )))
            }
        }
    }

    let wal_level = match read_wal_level(&next.data_dir).as_deref() {
        Some("logical") => "logical",
        _ => "replica",
    };
    let previous_yaml = std::fs::read_to_string(PATRONI_YML)
        .map_err(|e| RotateError::Failed(format!("read {PATRONI_YML}: {e}")))?;
    let previous_pin = read_credential_pin(&next.data_dir);
    let old_rest = current.restapi_auth.clone();
    let new_rest = next.restapi_auth.clone();

    let switched = switch_files(&next, wal_level, new_password)
        .map_err(|e| RotateError::Failed(format!("{e:#}")));
    if let Err(e) = switched {
        restore_files(&next.data_dir, &previous_yaml, previous_pin.as_ref());
        return Err(e);
    }

    let reload_started = epoch_secs();
    let started = Instant::now();
    let outcome = async {
        reload_patroni(ports.patroni_port, old_rest.as_ref()).await?;
        wait_for_dcs_session(ports.patroni_port, reload_started).await
    }
    .await;

    match outcome {
        Ok(()) => {
            live_credentials::set_rotated(
                new_password,
                next.restapi_auth.is_some(),
                next.etcd_auth.is_some(),
            );
            info!(
                node = %next.name,
                roles = ?roles,
                "credentials rotated: patroni.yml, the credential pin and Patroni's running configuration carry the new password"
            );
            Ok(RotateSummary {
                status: "rotated",
                reloaded: true,
                dcs_seen_after_ms: Some(started.elapsed().as_millis() as u64),
            })
        }
        Err(e) => {
            warn!(error = %e, "credential rotation failed; restoring the previous patroni.yml and pin");
            restore_files(&next.data_dir, &previous_yaml, previous_pin.as_ref());
            // Patroni may already enforce the new REST password (the reload
            // landed but the DCS session did not come back in time), so try
            // both credentials for the reload that puts the old file back.
            let mut restored = false;
            for cred in [new_rest.as_ref(), old_rest.as_ref()] {
                if reload_patroni(ports.patroni_port, cred).await.is_ok() {
                    restored = true;
                    break;
                }
            }
            if !restored {
                warn!("Patroni did not accept the reload that restores the previous configuration");
            }
            Err(RotateError::Failed(format!("{e:#}")))
        }
    }
}

/// `pg_authid.rolpassword` for `role`, read over the local unix socket as the
/// superuser (peer/trust in pg_hba, so no password is needed to ask).
async fn read_role_verifier(role: &str, superuser: &str, pg_port: u16) -> Result<Option<String>> {
    let connection_string = format!(
        "host={PG_SOCKET_DIR} port={pg_port} user={superuser} dbname=postgres connect_timeout=5"
    );
    let (client, connection) = tokio::time::timeout(
        VERIFIER_QUERY_TIMEOUT,
        tokio_postgres::connect(&connection_string, NoTls),
    )
    .await
    .context("timed out connecting to PostgreSQL over the unix socket")?
    .context("connect to PostgreSQL over the unix socket")?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::debug!(error = %e, "verifier connection closed");
        }
    });
    let row = tokio::time::timeout(
        VERIFIER_QUERY_TIMEOUT,
        client.query_opt(
            "SELECT rolpassword FROM pg_authid WHERE rolname = $1",
            &[&role],
        ),
    )
    .await
    .context("timed out reading pg_authid")?
    .context("read pg_authid")?;
    Ok(row.and_then(|r| r.get::<_, Option<String>>(0)))
}

/// Write the new patroni.yml (atomically) and the new pin.
fn switch_files(next: &Config, wal_level: &str, new_password: &str) -> Result<()> {
    write_atomically(PATRONI_YML, &generate_patroni_config(next, wal_level))?;
    write_credential_pin(
        &next.data_dir,
        &PinnedCredentials {
            superuser_pass: new_password.to_string(),
            repl_pass: new_password.to_string(),
            app_pass: new_password.to_string(),
        },
    )
    .context("write the credential pin")
}

fn restore_files(data_dir: &str, previous_yaml: &str, previous_pin: Option<&PinnedCredentials>) {
    if let Err(e) = write_atomically(PATRONI_YML, previous_yaml) {
        warn!(error = %e, "could not restore {PATRONI_YML}");
    }
    match previous_pin {
        Some(pin) => {
            if let Err(e) = write_credential_pin(data_dir, pin) {
                warn!(error = %e, "could not restore the credential pin");
            }
        }
        None => {
            let _ = std::fs::remove_file(super::credential_pin::pin_path(data_dir));
        }
    }
}

fn write_atomically(path: &str, content: &str) -> Result<()> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("write {tmp}"))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {tmp} over {path}"))?;
    Ok(())
}

async fn reload_patroni(patroni_port: u16, cred: Option<&Credential>) -> Result<()> {
    let client = super::rest::client_for(cred, RELOAD_TIMEOUT)?;
    let response = client
        .post(format!("http://localhost:{patroni_port}/reload"))
        .send()
        .await
        .context("POST /reload")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("POST /reload answered {status}: {}", body.trim()));
    }
    Ok(())
}

/// Wait until Patroni reports a DCS contact made after the reload with the
/// member running. `dcs_last_seen` is the epoch second of Patroni's last
/// successful DCS call, so a value past `reload_started` proves the HA loop
/// ran on the new configuration and etcd took the credential it carries.
async fn wait_for_dcs_session(patroni_port: u16, reload_started: u64) -> Result<()> {
    let client = super::rest::client_for(None, Duration::from_secs(5))?;
    let deadline = Instant::now() + DCS_SESSION_TIMEOUT;
    let mut last = String::from("no answer yet");
    while Instant::now() < deadline {
        match client
            .get(format!("http://localhost:{patroni_port}/patroni"))
            .send()
            .await
        {
            Ok(response) => {
                let body = response.text().await.unwrap_or_default();
                match dcs_session_after(&body, reload_started) {
                    Ok(true) => return Ok(()),
                    Ok(false) => last = body.chars().take(300).collect(),
                    Err(e) => last = e.to_string(),
                }
            }
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(DCS_POLL_INTERVAL).await;
    }
    Err(anyhow!(
        "Patroni did not report a DCS contact after the reload within {}s (last: {last})",
        DCS_SESSION_TIMEOUT.as_secs()
    ))
}

/// Whether a `GET /patroni` body shows the member running with a DCS contact
/// strictly after `reload_started` (same-second contacts may predate it).
pub fn dcs_session_after(body: &str, reload_started: u64) -> Result<bool> {
    let value: serde_json::Value = serde_json::from_str(body).context("parse /patroni")?;
    let state = value.get("state").and_then(|s| s.as_str()).unwrap_or("");
    let seen = value
        .get("dcs_last_seen")
        .and_then(|s| s.as_u64())
        .unwrap_or(0);
    Ok(state == "running" && seen > reload_started)
}

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether patroni.yml exists where the route rewrites it (for the health
/// server's startup log).
pub fn patroni_yml_present() -> bool {
    Path::new(PATRONI_YML).exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patroni::rest::test_support::ENV_LOCK;

    fn basic(user: &str, pass: &str) -> String {
        format!("Basic {}", BASE64.encode(format!("{user}:{pass}")))
    }

    #[test]
    fn parse_basic_reads_user_and_password() {
        let cred = parse_basic(Some(&basic("postgres", "p:w"))).unwrap();
        assert_eq!(cred.username, "postgres");
        assert_eq!(cred.password, "p:w");
        assert!(parse_basic(None).is_none());
        assert!(parse_basic(Some("Bearer x")).is_none());
        assert!(parse_basic(Some("Basic !!!")).is_none());
    }

    #[test]
    fn authorize_compares_against_the_members_rest_credential() {
        let _lock = ENV_LOCK.lock().unwrap();
        live_credentials::reset();
        std::env::set_var("PATRONI_SUPERUSER_USERNAME", "postgres");
        std::env::set_var("PATRONI_SUPERUSER_PASSWORD", "su");
        std::env::set_var("PATRONI_RESTAPI_PASSWORD", "rest");
        assert!(authorize(Some(&basic("postgres", "rest"))));
        assert!(!authorize(Some(&basic("postgres", "su"))));
        assert!(!authorize(Some(&basic("someone", "rest"))));
        assert!(!authorize(None));
        std::env::remove_var("PATRONI_RESTAPI_PASSWORD");
        std::env::remove_var("PATRONI_SUPERUSER_PASSWORD");
        std::env::remove_var("PATRONI_SUPERUSER_USERNAME");
    }

    #[test]
    fn authorize_refuses_everything_without_a_configured_credential() {
        let _lock = ENV_LOCK.lock().unwrap();
        live_credentials::reset();
        std::env::remove_var("PATRONI_RESTAPI_PASSWORD");
        std::env::remove_var("PATRONI_SUPERUSER_PASSWORD");
        assert!(!authorize(Some(&basic("postgres", ""))));
    }

    #[test]
    fn a_rotation_moves_the_rest_credential_the_route_checks() {
        let _lock = ENV_LOCK.lock().unwrap();
        live_credentials::reset();
        std::env::set_var("PATRONI_SUPERUSER_USERNAME", "postgres");
        std::env::set_var("PATRONI_SUPERUSER_PASSWORD", "old");
        std::env::set_var("PATRONI_RESTAPI_PASSWORD", "old");
        assert!(authorize(Some(&basic("postgres", "old"))));
        live_credentials::set_rotated("new", true, true);
        assert!(authorize(Some(&basic("postgres", "new"))));
        assert!(!authorize(Some(&basic("postgres", "old"))));
        live_credentials::reset();
        std::env::remove_var("PATRONI_RESTAPI_PASSWORD");
        std::env::remove_var("PATRONI_SUPERUSER_PASSWORD");
        std::env::remove_var("PATRONI_SUPERUSER_USERNAME");
    }

    #[test]
    fn dcs_session_needs_running_and_a_contact_after_the_reload() {
        let body = r#"{"state":"running","role":"replica","dcs_last_seen":1000}"#;
        assert!(dcs_session_after(body, 999).unwrap());
        assert!(!dcs_session_after(body, 1000).unwrap());
        assert!(!dcs_session_after(body, 1001).unwrap());
        let stopped = r#"{"state":"stopped","dcs_last_seen":2000}"#;
        assert!(!dcs_session_after(stopped, 999).unwrap());
        assert!(!dcs_session_after(r#"{"state":"running"}"#, 0).unwrap());
        assert!(dcs_session_after("not json", 0).is_err());
    }

    #[test]
    fn error_statuses_follow_the_contract() {
        assert_eq!(RotateError::Unauthorized.status(), 401);
        assert_eq!(RotateError::BadRequest("x".into()).status(), 400);
        assert_eq!(
            RotateError::RoleNotRotated("replicator".into()).status(),
            409
        );
        assert_eq!(RotateError::Unverifiable("x".into()).status(), 503);
        assert_eq!(RotateError::Failed("x".into()).status(), 500);
        assert!(RotateError::RoleNotRotated("replicator".into())
            .message()
            .contains("ALTER ROLE"));
    }

    fn config_at(data_dir: &str) -> Config {
        use crate::patroni::RestapiAddressSource;
        Config {
            scope: "s".into(),
            name: "n".into(),
            connect_address: "n".into(),
            restapi_connect_address: "n:8008".into(),
            restapi_address_source: RestapiAddressSource::PrivateDomain,
            etcd_hosts: "etcd:2379".into(),
            etcd_auth: Some(Credential {
                username: "root".into(),
                password: "old".into(),
            }),
            restapi_auth: Some(Credential {
                username: "postgres".into(),
                password: "old".into(),
            }),
            restapi_auth_enforced: true,
            superuser: "postgres".into(),
            superuser_pass: "old".into(),
            repl_user: "replicator".into(),
            repl_pass: "old-repl".into(),
            app_user: "app".into(),
            app_pass: "old-app".into(),
            app_db: "railway".into(),
            data_dir: data_dir.to_string(),
            certs_dir: format!("{data_dir}/certs"),
            ttl: "45".into(),
            loop_wait: "10".into(),
            retry_timeout: "17".into(),
            health_check_interval: 5,
            health_check_timeout: 5,
            max_failures: 3,
            startup_grace_period: 60,
            max_startup_timeout: 1800,
            adopt_existing_data: false,
            wait_for_leader: false,
            synchronous_mode: false,
            failsafe_mode: true,
            wal_archive_bucket: None,
            wal_recover_from_bucket: None,
            pitr_target_time: None,
            pitr_target_xid: None,
            archive_timeout_secs: 60,
            basebackup_max_rate: "20M".into(),
            max_slot_wal_keep_size: "512000MB".into(),
        }
    }

    #[test]
    fn one_password_reaches_every_credential_in_the_rendered_config() {
        let mut config = config_at("/tmp/pgdata");
        apply_one_password(&mut config, "new-pw");
        assert_eq!(config.superuser_pass, "new-pw");
        assert_eq!(config.repl_pass, "new-pw");
        assert_eq!(config.app_pass, "new-pw");
        assert_eq!(config.etcd_auth.as_ref().unwrap().password, "new-pw");
        assert_eq!(config.etcd_auth.as_ref().unwrap().username, "root");
        assert_eq!(config.restapi_auth.as_ref().unwrap().password, "new-pw");
        assert_eq!(config.restapi_auth.as_ref().unwrap().username, "postgres");

        let yaml = generate_patroni_config(&config, "replica");
        // restapi, etcd3, replication, superuser, rewind and app.
        assert_eq!(yaml.matches("\"new-pw\"").count(), 6, "{yaml}");
        assert!(!yaml.contains("old"), "{yaml}");
    }

    #[test]
    fn switching_files_writes_the_pin_with_the_one_password() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap().to_string();
        let mut config = config_at(&data_dir);
        apply_one_password(&mut config, "new-pw");
        write_credential_pin(
            &data_dir,
            &PinnedCredentials {
                superuser_pass: "new-pw".into(),
                repl_pass: "new-pw".into(),
                app_pass: "new-pw".into(),
            },
        )
        .unwrap();
        let pin = read_credential_pin(&data_dir).unwrap();
        assert_eq!(pin.superuser_pass, "new-pw");
        assert_eq!(pin.repl_pass, "new-pw");
        assert_eq!(pin.app_pass, "new-pw");
        // Restoring the previous pin puts the old values back.
        restore_files_pin_only(
            &data_dir,
            Some(&PinnedCredentials {
                superuser_pass: "old".into(),
                repl_pass: "old-repl".into(),
                app_pass: "old-app".into(),
            }),
        );
        assert_eq!(
            read_credential_pin(&data_dir).unwrap().repl_pass,
            "old-repl"
        );
        restore_files_pin_only(&data_dir, None);
        assert!(read_credential_pin(&data_dir).is_none());
    }

    fn restore_files_pin_only(data_dir: &str, previous_pin: Option<&PinnedCredentials>) {
        match previous_pin {
            Some(pin) => write_credential_pin(data_dir, pin).unwrap(),
            None => {
                let _ = std::fs::remove_file(super::super::credential_pin::pin_path(data_dir));
            }
        }
    }

    #[tokio::test]
    async fn an_empty_password_is_refused_before_anything_is_touched() {
        let ports = MemberPorts {
            pg_port: 5432,
            patroni_port: 8008,
        };
        match rotate("", &ports).await {
            Err(RotateError::BadRequest(_)) => {}
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn the_same_password_is_already_rotated() {
        let _lock = ENV_LOCK.lock().unwrap();
        live_credentials::reset();
        live_credentials::set_rotated("same", false, false);
        let ports = MemberPorts {
            pg_port: 5432,
            patroni_port: 8008,
        };
        let summary = rotate("same", &ports).await.unwrap();
        assert_eq!(summary.status, "already");
        assert!(!summary.reloaded);
        live_credentials::reset();
    }
}
