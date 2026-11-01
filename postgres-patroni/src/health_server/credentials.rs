//! Local half of an ordered cluster rotation. SQL is executed in this
//! container; the control plane owns the etcd change and cluster ordering.
use super::HealthServerConfig;
#[cfg(test)]
use crate::patroni::write_credential_pin;
use crate::patroni::{
    clear_rotation_journal, pin_for_boot, read_credential_pin, read_credential_pin_copy,
    sync_credential_pin_copy, write_credential_pin_and_copy, write_rotation_journal,
    PinnedCredentials,
};
use crate::volume_root;
use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    Json,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::Write,
    os::unix::fs::OpenOptionsExt,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tokio_postgres::NoTls;
use tracing::{info, warn};

const CONFIG: &str = "/etc/patroni/patroni.yml";
static ROTATION: Mutex<()> = Mutex::const_new(());
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rotation {
    operation: Operation,
    new_password: String,
    current_password: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Preflight,
    Prepare,
    Database,
    Member,
    Verify,
}

fn load() -> Result<serde_yaml::Value> {
    Ok(serde_yaml::from_str(&std::fs::read_to_string(CONFIG)?)?)
}
fn text(value: &serde_yaml::Value) -> Result<&str> {
    value.as_str().context("missing Patroni configuration")
}
fn atomic_write(path: &str, body: &[u8]) -> Result<()> {
    let tmp = format!("{path}.rotation.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(body)?;
    file.sync_all()?;
    std::fs::rename(tmp, path)?;
    std::fs::File::open(
        std::path::Path::new(path)
            .parent()
            .context("missing parent")?,
    )?
    .sync_all()?;
    Ok(())
}
fn quote_identifier(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}
fn pgpass(s: &str) -> String {
    s.replace('\\', "\\\\").replace(':', "\\:")
}

async fn connect(
    config: &HealthServerConfig,
    user: &str,
    password: &str,
) -> Result<tokio_postgres::Client> {
    let (client, connection) = tokio_postgres::Config::new()
        .host("127.0.0.1")
        .port(config.pg_port)
        .user(user)
        .password(password)
        .dbname("postgres")
        .connect_timeout(Duration::from_secs(5))
        .connect(NoTls)
        .await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client.simple_query("SELECT 1").await?;
    Ok(client)
}

// A successful loopback login is not evidence of a password match: adopted
// datasets may retain a trust rule. Check the stored verifier with a superuser
// connection, without changing the customer's HBA policy.
async fn roles_match(
    client: &tokio_postgres::Client,
    roles: &[&str],
    password: &str,
) -> Result<bool> {
    use crate::patroni::scram::{verify, VerifierCheck};
    for role in roles {
        let row = client
            .query_opt(
                "SELECT rolpassword FROM pg_authid WHERE rolname = $1",
                &[role],
            )
            .await?
            .context("rotation role missing")?;
        let verifier: Option<String> = row.get(0);
        match verifier
            .as_deref()
            .map(|value| verify(value, role, password))
        {
            Some(VerifierCheck::Matches) => {}
            Some(VerifierCheck::Differs) | None => return Ok(false),
            Some(VerifierCheck::Unsupported(_)) => anyhow::bail!("unsupported password verifier"),
        }
    }
    Ok(true)
}

/// The user the control plane authenticates as; the password is the pinned
/// superuser password.
const ROTATION_USER: &str = "railway";

/// The decoded Basic credential (`user:password`) of a request, if any.
fn supplied_credential(headers: &HeaderMap) -> Option<Vec<u8>> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("basic"))
        .and_then(|(_, token)| base64::engine::general_purpose::STANDARD.decode(token).ok())
}

/// The password of a decoded `railway:<password>` credential.
fn supplied_password(credential: &[u8]) -> Option<&str> {
    std::str::from_utf8(credential)
        .ok()?
        .strip_prefix(ROTATION_USER)?
        .strip_prefix(':')
}

/// Whether an operation may establish a missing pin from the credential the
/// request presents. Only the read-only ends of a rotation qualify: Preflight,
/// where the password presented is by contract the one the roles carry, and
/// Verify, which proves the roles' state before anything is trusted. The
/// mutating steps keep refusing without a pin.
fn may_establish_pin(operation: &Operation) -> bool {
    matches!(operation, Operation::Preflight | Operation::Verify)
}

/// Whether `supplied` is `railway:<password>` for this `password`, in
/// constant time.
fn carries(supplied: Option<&[u8]>, password: &str) -> bool {
    let expected = format!("{ROTATION_USER}:{password}");
    supplied.is_some_and(|s| bool::from(s.ct_eq(expected.as_bytes())))
}

/// The pin a rotation request authenticates against.
///
/// PGDATA's pin, normally. The volume's copy of it (see
/// [`crate::patroni::CREDENTIAL_PIN_COPY_FILE`]) is consulted in two cases:
///
/// - PGDATA has no pin and the request carries the copy's password: PGDATA
///   was wiped (a reinitialize in progress) and the copy is what this member
///   was pinned to. The request authenticates against it; nothing is written
///   into PGDATA, which the clone needs empty. The operation then fails on
///   the database being down, which the control plane retries, instead of
///   "credential pin unavailable", which it reads as a member whose image
///   never pinned and redeploys mid-clone.
/// - PGDATA has a pin, the copy differs, and the request carries the copy's
///   password rather than PGDATA's: a re-seed from the archive restored the
///   pin of the last backup, from before a rotation this member already
///   went through (the copy is written at every pin change, the backup is
///   not). Once the roles are proven to carry the copy's password, PGDATA is
///   re-pinned to it and the request proceeds; a preflight would otherwise
///   answer 401, which the control plane reports as the variable no longer
///   matching the cluster. Roles that carry neither, or a verifier that
///   cannot run, leave PGDATA's pin in force: the request is then refused as
///   today, never left without a pin.
///
/// A volume without any pin used to refuse every rotation with 503. That left
/// the clusters that never got one stranded: replicas cloned from a leader
/// that booted unpinned (the adoption gap fixed in the runner) carry neither
/// pin nor bootstrap marker, so no later boot adopts one — only a re-clone
/// would. For [`may_establish_pin`] operations the pin is instead established
/// from the credential the request presents, once `verify` has proven, from
/// the roles' stored verifiers, that the superuser, replication and app roles
/// all carry that password. The coupled pin is then exactly what
/// post_bootstrap would have written. A credential the roles do not carry is
/// unauthorized, the same answer a pinned node gives; a verifier that cannot
/// run (Postgres down) leaves the node unavailable, never unauthorized.
///
/// Reads only, apart from the re-pin above: the volume's copy is brought up
/// to date by [`admit`], after the request has authenticated.
async fn resolve_pin<F, Fut>(
    data_dir: &str,
    volume_root: &str,
    operation: &Operation,
    credential: Option<&[u8]>,
    verify: F,
) -> Result<PinnedCredentials, StatusCode>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    let copy = read_credential_pin_copy(volume_root);
    if let Some(pin) = read_credential_pin(data_dir) {
        let Some(copy) = copy.filter(|copy| *copy != pin) else {
            return Ok(pin);
        };
        if carries(credential, &pin.superuser_pass) || !carries(credential, &copy.superuser_pass) {
            return Ok(pin);
        }
        return match verify(copy.superuser_pass.clone()).await {
            Ok(true) => {
                write_credential_pin_and_copy(data_dir, volume_root, &copy)
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                info!(
                    data_dir,
                    operation = ?operation,
                    "PGDATA's credential pin is behind the volume's copy (re-seeded from a backup \
                     older than a rotation this member went through); re-pinned to the copy, \
                     which the roles were verified to carry"
                );
                Ok(copy)
            }
            Ok(false) | Err(_) => Ok(pin),
        };
    }
    if let Some(copy) = copy {
        if carries(credential, &copy.superuser_pass) {
            info!(
                data_dir,
                operation = ?operation,
                "PGDATA carries no credential pin (wiped, or a clone in progress); the request \
                 authenticates against the volume's copy"
            );
            return Ok(copy);
        }
    }
    if !may_establish_pin(operation) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let password = credential
        .and_then(supplied_password)
        .ok_or(StatusCode::UNAUTHORIZED)?;
    match verify(password.to_string()).await {
        Ok(true) => {}
        Ok(false) => return Err(StatusCode::UNAUTHORIZED),
        Err(_) => return Err(StatusCode::SERVICE_UNAVAILABLE),
    }
    let pin = PinnedCredentials {
        superuser_pass: password.to_string(),
        repl_pass: password.to_string(),
        app_pass: password.to_string(),
    };
    write_credential_pin_and_copy(data_dir, volume_root, &pin)
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    info!(
        data_dir,
        operation = ?operation,
        "no credential pin on this volume — established it from the rotation credential \
         the roles were verified to carry"
    );
    Ok(pin)
}

/// Prove, from the stored verifiers, that every rotation role carries
/// `password`: the `verify` of [`resolve_pin`] in production.
async fn roles_carry(
    config: &HealthServerConfig,
    yaml: &serde_yaml::Value,
    password: String,
) -> Result<bool> {
    let user = text(&yaml["postgresql"]["authentication"]["superuser"]["username"])?;
    let replication_user = text(&yaml["postgresql"]["authentication"]["replication"]["username"])?;
    let app_user = text(&yaml["postgresql"]["app_user"]["username"])?;
    // Loopback is `trust` on the rendered pg_hba, so the login below normally
    // succeeds whatever the password and the verifiers decide. On a tightened
    // pg_hba the server itself refuses a wrong password: that is the same
    // "roles do not carry it" answer, not an outage.
    let client = match connect(config, user, &password).await {
        Ok(client) => client,
        Err(e) if is_auth_failure(&e) => return Ok(false),
        Err(e) => return Err(e),
    };
    roles_match(&client, &[user, replication_user, app_user], &password).await
}

/// Whether a connection error is Postgres refusing the credential
/// (SQLSTATE 28P01 invalid_password or 28000 invalid_authorization_specification).
fn is_auth_failure(error: &anyhow::Error) -> bool {
    use tokio_postgres::error::SqlState;
    error
        .downcast_ref::<tokio_postgres::Error>()
        .and_then(|e| e.as_db_error())
        .is_some_and(|db| {
            *db.code() == SqlState::INVALID_PASSWORD
                || *db.code() == SqlState::INVALID_AUTHORIZATION_SPECIFICATION
        })
}

/// Whether this member's Patroni talks to etcd with a credential. Clusters
/// from before the etcd root password carry no `etcd3.password`; nothing in
/// a rotation touches their DCS.
fn etcd_authenticated(yaml: &serde_yaml::Value) -> bool {
    yaml["etcd3"]["password"]
        .as_str()
        .is_some_and(|p| !p.is_empty())
}

/// Where this member keeps its REST credential. A member that enforces REST
/// authentication (`PATRONI_RESTAPI_PASSWORD` set) renders it under
/// `restapi.authentication`; one that only sends it, as every cluster from
/// before that variable does, renders it under `ctl.authentication`. Rotation
/// updates the block the member has and never adds one: adding
/// `restapi.authentication` would start enforcing on the next reload.
fn rest_auth_section(yaml: &serde_yaml::Value) -> Option<&'static str> {
    ["restapi", "ctl"].into_iter().find(|section| {
        yaml[section]["authentication"]["password"]
            .as_str()
            .is_some()
    })
}

/// The credential fields a rotation keeps in lockstep, as this member renders
/// them: the Postgres roles, the REST credential wherever it lives, and the
/// etcd root password only when Patroni authenticates to etcd.
fn coupled_credentials(yaml: &serde_yaml::Value) -> Vec<&serde_yaml::Value> {
    let mut coupled = vec![
        &yaml["postgresql"]["authentication"]["superuser"]["password"],
        &yaml["postgresql"]["authentication"]["replication"]["password"],
        &yaml["postgresql"]["app_user"]["password"],
    ];
    if let Some(section) = rest_auth_section(yaml) {
        coupled.push(&yaml[section]["authentication"]["password"]);
    }
    if etcd_authenticated(yaml) {
        coupled.push(&yaml["etcd3"]["password"]);
    }
    coupled
}

pub async fn rotate(
    State(config): State<HealthServerConfig>,
    headers: HeaderMap,
    Json(request): Json<Rotation>,
) -> (StatusCode, Json<Value>) {
    let _lock = ROTATION.lock().await;
    let yaml = match load() {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"configuration unavailable"})),
            )
        }
    };
    let dir = yaml["postgresql"]["data_dir"].as_str().unwrap_or("");
    let supplied = supplied_credential(&headers);
    let resolved = tokio::time::timeout(
        Duration::from_secs(10),
        resolve_pin(
            dir,
            &volume_root(),
            &request.operation,
            supplied.as_deref(),
            |password| roles_carry(&config, &yaml, password),
        ),
    )
    .await;
    let pin = match resolved {
        Ok(Ok(pin)) => pin,
        Ok(Err(StatusCode::UNAUTHORIZED)) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"unauthorized"})),
            )
        }
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"credential pin unavailable"})),
            )
        }
    };
    if admit(supplied.as_deref(), &pin, &volume_root()).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        );
    }
    if needs_pgdata_pin(&request.operation) && read_credential_pin(dir).is_none() {
        warn!(
            operation = ?request.operation,
            "credential operation refused: PGDATA carries no pin (wiped, or a clone in progress)"
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"error":"credential rotation could not be verified: PGDATA is being re-cloned"}),
            ),
        );
    }
    if request.new_password.is_empty()
        || request.new_password.len() > 1024
        || request.new_password.contains(['\0', '\n', '\r'])
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid password"})),
        );
    }
    let operation = format!("{:?}", request.operation).to_lowercase();
    let secrets = [
        request.new_password.clone(),
        request.current_password.clone(),
    ];
    match tokio::time::timeout(Duration::from_secs(35), apply(&config, yaml, request)).await {
        Ok(Ok(value)) => (StatusCode::OK, Json(value)),
        Ok(Err(error)) => {
            let reason = refusal_reason(&error, &secrets);
            warn!(operation, reason, "credential operation refused");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(
                    json!({"error": format!("credential rotation could not be verified: {reason}")}),
                ),
            )
        }
        Err(_) => {
            warn!(operation, "credential operation timed out");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"credential rotation could not be verified: timed out"})),
            )
        }
    }
}

/// Whether an operation writes into PGDATA before it touches the database.
/// `prepare` records the journal first and connects after; on a PGDATA that
/// was wiped for a clone in progress (the request then authenticated against
/// the volume's copy) that file would make the directory non-empty and
/// `pg_basebackup` refuse it. Every other operation connects first, and the
/// database being down refuses it before it writes anything. Refused as
/// "could not be verified", which the control plane retries, like a member
/// that is down.
fn needs_pgdata_pin(operation: &Operation) -> bool {
    matches!(operation, Operation::Prepare)
}

/// Authenticate the request against `pin` and, once it has, bring the volume's
/// copy of the pin up to date. A replica's pin arrives with its clone, and the
/// volume's copy only on its next boot; a rotation must not stage a journal
/// on a member whose copy is missing, or the boot after a wipe mid-rotation
/// has the journal and no pin to reconcile it against. After authentication
/// only: a request that cannot present the pin writes nothing.
fn admit(
    supplied: Option<&[u8]>,
    pin: &PinnedCredentials,
    volume_root: &str,
) -> Result<(), StatusCode> {
    if !carries(supplied, &pin.superuser_pass) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    sync_credential_pin_copy(volume_root, pin);
    Ok(())
}

/// One short line naming why an operation was refused, so the control plane
/// and the logs can say which check failed. The chain's outermost message is
/// the check (`role password differs`, `cluster credentials are not coupled`);
/// a driver error below it names the server's complaint. Never a credential:
/// every password the request carried is cut out before the line leaves.
fn refusal_reason(error: &anyhow::Error, secrets: &[String]) -> String {
    let mut reason = error
        .chain()
        .map(|e| e.to_string().replace(['\n', '\r'], " "))
        .filter(|m| !m.trim().is_empty())
        .collect::<Vec<_>>()
        .join(": ");
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        reason = reason.replace(secret.as_str(), "<redacted>");
    }
    reason.chars().take(200).collect()
}

async fn apply(
    config: &HealthServerConfig,
    mut yaml: serde_yaml::Value,
    request: Rotation,
) -> Result<Value> {
    let user = text(&yaml["postgresql"]["authentication"]["superuser"]["username"])?.to_string();
    let replication_user =
        text(&yaml["postgresql"]["authentication"]["replication"]["username"])?.to_string();
    let app_user = text(&yaml["postgresql"]["app_user"]["username"])?.to_string();
    let data_dir = text(&yaml["postgresql"]["data_dir"])?.to_string();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let patroni = format!("http://127.0.0.1:{}", config.patroni_port);
    match request.operation {
        Operation::Preflight => {
            let client = connect(config, &user, &request.new_password).await?;
            anyhow::ensure!(
                roles_match(
                    &client,
                    &[&user, &replication_user, &app_user],
                    &request.new_password
                )
                .await?,
                "role password differs"
            );
            let dynamic: Value = http
                .get(format!("{patroni}/config"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::ensure!(
                dynamic["failsafe_mode"] == true && dynamic["pause"] != true,
                "failsafe disabled or maintenance active"
            );
            // A cluster whose etcd runs without authentication (templates
            // before the etcd root password) has no DCS credential to couple
            // or rotate; the control plane leaves its etcd alone.
            if etcd_authenticated(&yaml) {
                anyhow::ensure!(
                    yaml["etcd3"]["username"].as_str() == Some("root"),
                    "unsupported etcd identity"
                );
            }
            for value in coupled_credentials(&yaml) {
                anyhow::ensure!(
                    value.as_str() == Some(&request.new_password),
                    "cluster credentials are not coupled"
                );
            }
        }
        Operation::Prepare => {
            // Persist intent before any database or DCS mutation. Recovery can
            // finish adoption even while the control-plane worker is down, and
            // the volume's copy outlives a wipe of PGDATA (a reinitialize
            // mid-rotation), so the boot after it still knows the rotation.
            write_rotation_journal(&data_dir, &volume_root(), &serde_json::to_vec(&request)?)?;
        }
        Operation::Database => {
            let mut client = match connect(config, &user, &request.new_password).await {
                Ok(client) => client,
                Err(_) => connect(config, &user, &request.current_password).await?,
            };
            if !roles_match(
                &client,
                &[&user, &replication_user, &app_user],
                &request.new_password,
            )
            .await?
            {
                let transaction = client.transaction().await?;
                anyhow::ensure!(
                    !transaction
                        .query_one("SELECT pg_is_in_recovery()", &[])
                        .await?
                        .get::<_, bool>(0),
                    "not primary"
                );
                transaction
                    .batch_execute("SET LOCAL standard_conforming_strings = on")
                    .await?;
                for role in [&user, &replication_user, &app_user] {
                    transaction
                        .batch_execute(&format!(
                            "ALTER ROLE {} PASSWORD {}",
                            quote_identifier(role),
                            quote_literal(&request.new_password)
                        ))
                        .await?;
                }
                transaction.commit().await?;
            }
        }
        Operation::Member => {
            // Wait for the role transaction to replay before changing a replica.
            let client = connect(config, &user, &request.new_password).await?;
            anyhow::ensure!(
                roles_match(
                    &client,
                    &[&user, &replication_user, &app_user],
                    &request.new_password
                )
                .await?,
                "role password not replicated"
            );
            // Without an enforced REST credential Patroni accepts any
            // Authorization on /reload; the superuser name and an empty
            // password are then only what the request carries.
            let rest = rest_auth_section(&yaml);
            let rest_user = match rest {
                Some(section) => text(&yaml[section]["authentication"]["username"])?.to_string(),
                None => user.clone(),
            };
            let old_rest_password = match rest {
                Some(section) => text(&yaml[section]["authentication"]["password"])?.to_string(),
                None => String::new(),
            };
            for role in ["superuser", "replication", "rewind"] {
                if !yaml["postgresql"]["authentication"][role].is_null() {
                    yaml["postgresql"]["authentication"][role]["password"] =
                        request.new_password.clone().into();
                }
            }
            yaml["postgresql"]["app_user"]["password"] = request.new_password.clone().into();
            if etcd_authenticated(&yaml) {
                yaml["etcd3"]["password"] = request.new_password.clone().into();
            }
            for section in ["restapi", "ctl"] {
                if !yaml[section]["authentication"].is_null() {
                    yaml[section]["authentication"]["password"] =
                        request.new_password.clone().into();
                }
            }
            let passfile = text(&yaml["postgresql"]["pgpass"])?;
            let entries = [&replication_user, &user, &app_user]
                .map(|u| format!("*:*:*:{}:{}\n", pgpass(u), pgpass(&request.new_password)))
                .join("");
            atomic_write(passfile, entries.as_bytes())?;
            atomic_write(CONFIG, serde_yaml::to_string(&yaml)?.as_bytes())?;
            // A retry after the file rename can see the new YAML while Patroni
            // still enforces the previous credential. Both are request facts.
            let mut reloaded = false;
            for password in [
                &old_rest_password,
                &request.current_password,
                &request.new_password,
            ] {
                let response = http
                    .post(format!("{patroni}/reload"))
                    .basic_auth(&rest_user, Some(password))
                    .send()
                    .await?;
                if response.status().is_success() {
                    reloaded = true;
                    break;
                }
            }
            anyhow::ensure!(reloaded, "reload refused");
            let reload_started = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            // /reload queues SIGHUP processing. Wait for the new REST auth to
            // be live before persisting success and moving to the next member.
            loop {
                if http
                    .post(format!("{patroni}/reload"))
                    .basic_auth(&rest_user, Some(&request.new_password))
                    .send()
                    .await?
                    .status()
                    .is_success()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            // Authentication alone is not proof Patroni reconnected to DCS.
            // Wait for its next successful lease/cluster refresh before letting
            // the coordinator advance to another member.
            loop {
                let state: Value = http
                    .get(format!("{patroni}/patroni"))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                if state["dcs_last_seen"]
                    .as_u64()
                    .is_some_and(|seen| seen > reload_started)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            write_credential_pin_and_copy(
                &data_dir,
                &volume_root(),
                &PinnedCredentials {
                    superuser_pass: request.new_password.clone(),
                    repl_pass: request.new_password.clone(),
                    app_pass: request.new_password.clone(),
                },
            )?;
        }
        Operation::Verify => {
            let client = connect(config, &user, &request.new_password).await?;
            anyhow::ensure!(
                roles_match(
                    &client,
                    &[&user, &replication_user, &app_user],
                    &request.new_password
                )
                .await?,
                "role password differs"
            );
            // PGDATA's pin, or the volume's copy while PGDATA is being
            // re-cloned (the request authenticated against that same copy).
            let (pin, _) = pin_for_boot(&data_dir, &volume_root()).context("pin missing")?;
            anyhow::ensure!(
                pin.superuser_pass == request.new_password
                    && pin.repl_pass == request.new_password
                    && pin.app_pass == request.new_password,
                "pin differs"
            );
            if request.current_password != request.new_password {
                anyhow::ensure!(
                    !roles_match(&client, &[&user], &request.current_password).await?,
                    "previous password still accepted"
                );
            }
            // A replica that carries the password but is not streaming from
            // the leader, on the leader's timeline, has not finished the
            // rotation: its walreceiver is still presenting the previous
            // credential, or it lost the leader at the restart that applied
            // the new one. Left alone it falls further behind until the
            // self-heal reinitializes it, long after the rotation reported
            // DONE. Say so now, within the request's budget: the coordinator
            // re-applies the member step and verifies again.
            if client
                .query_one("SELECT pg_is_in_recovery()", &[])
                .await?
                .get::<_, bool>(0)
            {
                wait_for_streaming(&http, &patroni, STREAMING_BUDGET).await?;
            }
            clear_rotation_journal(&data_dir, &volume_root())?;
        }
    }
    // During DATABASE the YAML/pin still holds the old value, but SQL already
    // holds the new one. Use the request's desired password for this verdict.
    let client = match connect(config, &user, &request.new_password).await {
        Ok(client) => client,
        Err(_) if matches!(request.operation, Operation::Prepare) => {
            connect(config, &user, &request.current_password).await?
        }
        Err(error) => return Err(error),
    };
    let leader = !client
        .query_one("SELECT pg_is_in_recovery()", &[])
        .await?
        .get::<_, bool>(0);
    Ok(json!({"version": 1, "leader": leader, "capabilities": ["catalog_verifier"]}))
}

/// How long `verify` waits for a replica to stream from the leader on the
/// leader's timeline. Well inside the request's 35 s budget: a replica whose
/// member step just reloaded its credentials reconnects within seconds, and
/// one that does not is reported, not waited for.
const STREAMING_BUDGET: Duration = Duration::from_secs(20);

/// What Patroni says about this replica's replication: `/patroni` must report
/// `replication_state: streaming` and the replica's timeline must be the
/// leader's (`/cluster`). A cluster that names no leader timeline cannot be
/// held against the replica; the streaming state alone decides then.
fn streaming_from_leader(local: &Value, cluster: &Value) -> std::result::Result<(), String> {
    match local["replication_state"].as_str() {
        Some("streaming") => {}
        Some(state) => return Err(format!("replica not streaming ({state})")),
        None => return Err("replica not streaming (no replication state)".to_string()),
    }
    let leader_timeline = cluster["members"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|member| member["role"] == "leader")
        .and_then(|member| member["timeline"].as_i64());
    match (local["timeline"].as_i64(), leader_timeline) {
        (Some(local), Some(leader)) if local != leader => Err(format!(
            "replica on timeline {local}, leader on timeline {leader}"
        )),
        _ => Ok(()),
    }
}

/// Wait for [`streaming_from_leader`] to hold, polling Patroni once a second
/// until `budget` runs out; the last reason is the error.
async fn wait_for_streaming(http: &reqwest::Client, patroni: &str, budget: Duration) -> Result<()> {
    let deadline = Instant::now() + budget;
    loop {
        // /patroni answers 503 on a replica with the same body; read it either way.
        let local: Value = http
            .get(format!("{patroni}/patroni"))
            .send()
            .await?
            .json()
            .await?;
        let cluster: Value = http
            .get(format!("{patroni}/cluster"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        match streaming_from_leader(&local, &cluster) {
            Ok(()) => return Ok(()),
            Err(reason) if Instant::now() >= deadline => anyhow::bail!("{reason}"),
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

/// The pending credential is private on-disk intent, never progress or logs.
pub(super) fn pending_password(data_dir: &str) -> Option<String> {
    let request: Rotation =
        serde_json::from_slice(&std::fs::read(format!("{data_dir}/.railway_rotation")).ok()?)
            .ok()?;
    Some(request.new_password)
}

/// Failsafe preserves leadership while etcd sessions are invalidated. Finish
/// the local reload after observing BOTH committed roles and DCS credentials;
/// never infer a credential change merely from an edited environment variable.
pub(super) async fn reconcile(config: HealthServerConfig) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _lock = ROTATION.lock().await;
        let attempt = async {
            let yaml = load()?;
            let dir = text(&yaml["postgresql"]["data_dir"])?;
            let journal = format!("{dir}/.railway_rotation");
            anyhow::ensure!(
                std::fs::metadata(&journal)?
                    .modified()?
                    .elapsed()?
                    .as_secs()
                    >= 30,
                "coordinator owns the normal ordered phase"
            );
            let mut request: Rotation =
                serde_json::from_slice(&std::fs::read(format!("{dir}/.railway_rotation"))?)?;
            let etcd_done = !etcd_authenticated(&yaml)
                || yaml["etcd3"]["password"].as_str() == Some(&request.new_password);
            if read_credential_pin(dir).is_some_and(|p| p.superuser_pass == request.new_password)
                && etcd_done
            {
                return Ok::<(), anyhow::Error>(());
            }
            let user = text(&yaml["postgresql"]["authentication"]["superuser"]["username"])?;
            connect(&config, user, &request.new_password).await?;
            if etcd_authenticated(&yaml) {
                let hosts = text(&yaml["etcd3"]["hosts"])?;
                let credential = crate::patroni::Credential {
                    username: "root".into(),
                    password: request.new_password.clone(),
                };
                anyhow::ensure!(
                    crate::patroni::etcd_preflight::probe_etcd_credential(
                        hosts,
                        &credential,
                        Duration::from_secs(2)
                    )
                    .await
                        == crate::patroni::etcd_preflight::EtcdAuthProbe::Accepted,
                    "DCS not committed"
                );
            }
            request.operation = Operation::Member;
            apply(&config, yaml, request).await?;
            Ok(())
        };
        // Expected while waiting for the coordinator; do not log credentials
        // or churn health status because a prepare has not committed yet.
        let _ = tokio::time::timeout(Duration::from_secs(20), attempt).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires an isolated local PostgreSQL server with loopback trust"]
    async fn live_rotation_with_loopback_trust() {
        let mut config = HealthServerConfig::from_env();
        config.pg_port = std::env::var("ROTATION_TEST_PG_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let client = connect(&config, "postgres", "never-set").await.unwrap();
        client.batch_execute("CREATE ROLE rotation_replica LOGIN; CREATE ROLE rotation_app LOGIN; ALTER ROLE postgres PASSWORD 'rotation-old-local'; ALTER ROLE rotation_replica PASSWORD 'rotation-old-local'; ALTER ROLE rotation_app PASSWORD 'rotation-old-local';").await.unwrap();
        let yaml: serde_yaml::Value = serde_yaml::from_str("postgresql:\n  data_dir: /unused\n  authentication:\n    superuser:\n      username: postgres\n    replication:\n      username: rotation_replica\n  app_user:\n    username: rotation_app\n").unwrap();
        for (previous, target) in [
            ("rotation-old-local", "rotation-new-local"),
            ("rotation-new-local", "rotation-old-local"),
        ] {
            assert!(!roles_match(
                &client,
                &["postgres", "rotation_replica", "rotation_app"],
                target
            )
            .await
            .unwrap());
            for _ in 0..2 {
                apply(
                    &config,
                    yaml.clone(),
                    Rotation {
                        operation: Operation::Database,
                        new_password: target.into(),
                        current_password: previous.into(),
                    },
                )
                .await
                .unwrap();
            }
            assert!(roles_match(
                &client,
                &["postgres", "rotation_replica", "rotation_app"],
                target
            )
            .await
            .unwrap());
            assert!(!roles_match(&client, &["postgres"], previous).await.unwrap());
        }
    }
    fn pin_of(password: &str) -> PinnedCredentials {
        PinnedCredentials {
            superuser_pass: password.into(),
            repl_pass: password.into(),
            app_pass: password.into(),
        }
    }

    #[test]
    fn verify_holds_a_replica_to_streaming_on_the_leaders_timeline() {
        let cluster = json!({"members": [
            {"name": "pg-1", "role": "leader", "state": "running", "timeline": 2},
            {"name": "pg-2", "role": "replica", "state": "running", "timeline": 2},
        ]});
        let streaming = json!({"role": "replica", "replication_state": "streaming", "timeline": 2});
        assert_eq!(streaming_from_leader(&streaming, &cluster), Ok(()));
        // The precursor of the mid-rotation reinitialize: every member on the
        // new password, two replicas sitting a timeline behind, not streaming.
        let behind =
            json!({"role": "replica", "replication_state": "in archive recovery", "timeline": 1});
        assert_eq!(
            streaming_from_leader(&behind, &cluster),
            Err("replica not streaming (in archive recovery)".to_string())
        );
        let stuck = json!({"role": "replica", "replication_state": "streaming", "timeline": 1});
        assert_eq!(
            streaming_from_leader(&stuck, &cluster),
            Err("replica on timeline 1, leader on timeline 2".to_string())
        );
        let silent = json!({"role": "replica", "timeline": 2});
        assert_eq!(
            streaming_from_leader(&silent, &cluster),
            Err("replica not streaming (no replication state)".to_string())
        );
        // No leader in the cluster view (an election in flight): the
        // streaming state is all that can be held against the replica.
        let leaderless = json!({"members": [{"name": "pg-2", "role": "replica", "timeline": 1}]});
        assert_eq!(streaming_from_leader(&streaming, &leaderless), Ok(()));
    }
    fn basic(user: &str, password: &str) -> Vec<u8> {
        format!("{user}:{password}").into_bytes()
    }
    fn basic_header(user: &str, password: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let token = base64::engine::general_purpose::STANDARD.encode(basic(user, password));
        headers.insert(
            header::AUTHORIZATION,
            format!("Basic {token}").parse().unwrap(),
        );
        headers
    }

    #[test]
    fn supplied_credential_decodes_basic_only() {
        let headers = basic_header("railway", "p:w:d");
        let credential = supplied_credential(&headers).unwrap();
        assert_eq!(credential, basic("railway", "p:w:d"));
        // The password keeps every colon after the user's.
        assert_eq!(supplied_password(&credential), Some("p:w:d"));
        assert_eq!(supplied_password(b"someone:else"), None);
        assert_eq!(supplied_password(b"railway"), None);
        assert_eq!(supplied_password(b"railway-ish:pw"), None);
        let mut bearer = HeaderMap::new();
        bearer.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(supplied_credential(&bearer), None);
        assert_eq!(supplied_credential(&HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn an_existing_pin_is_returned_without_verifying_anything() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap();
        write_credential_pin(dir, &pin_of("pinned")).unwrap();
        let credential = basic("railway", "wrong");
        let pin = resolve_pin(
            dir,
            dir,
            &Operation::Preflight,
            Some(&credential),
            |_| async { panic!("verifier must not run when a pin exists") },
        )
        .await
        .unwrap();
        assert_eq!(pin, pin_of("pinned"));
        // The handler's constant-time comparison still rejects the wrong
        // credential afterwards; resolve_pin only says what to compare to.
    }

    const ENFORCED: &str = "restapi:\n  authentication:\n    username: \"postgres\"\n    password: \"pw\"\netcd3:\n  username: \"root\"\n  password: \"pw\"\npostgresql:\n  authentication:\n    superuser: {username: postgres, password: \"pw\"}\n    replication: {username: postgres, password: \"pw\"}\n  app_user: {username: postgres, password: \"pw\"}\n";
    const LEGACY: &str = "restapi:\n  listen: \":::8008\"\nctl:\n  authentication:\n    username: \"postgres\"\n    password: \"pw\"\netcd3:\n  username: \"root\"\n  password: \"pw\"\npostgresql:\n  authentication:\n    superuser: {username: postgres, password: \"pw\"}\n    replication: {username: postgres, password: \"pw\"}\n  app_user: {username: postgres, password: \"pw\"}\n";

    #[test]
    fn the_rest_credential_is_coupled_wherever_this_member_keeps_it() {
        let enforced: serde_yaml::Value = serde_yaml::from_str(ENFORCED).unwrap();
        let legacy: serde_yaml::Value = serde_yaml::from_str(LEGACY).unwrap();
        assert_eq!(rest_auth_section(&enforced), Some("restapi"));
        assert_eq!(rest_auth_section(&legacy), Some("ctl"));
        for yaml in [&enforced, &legacy] {
            let coupled = coupled_credentials(yaml);
            assert_eq!(coupled.len(), 5, "roles, rest and etcd");
            assert!(coupled.iter().all(|v| v.as_str() == Some("pw")));
        }
        // A member that enforces nothing and sends nothing has no REST field
        // to keep in step; the roles and etcd still are.
        let mut bare = legacy.clone();
        bare["ctl"] = serde_yaml::Value::Null;
        assert_eq!(rest_auth_section(&bare), None);
        assert_eq!(coupled_credentials(&bare).len(), 4);
    }

    #[test]
    fn a_refusal_names_the_check_and_the_cause_but_never_a_password() {
        let secret = "hunter2-very-secret";
        let error = anyhow::anyhow!("db error: FATAL: password {secret} rejected\nsecond line")
            .context("role password differs");
        let reason = refusal_reason(&error, &[secret.to_string(), String::new()]);
        assert_eq!(
            reason,
            "role password differs: db error: FATAL: password <redacted> rejected second line"
        );
        let long = anyhow::anyhow!("{}", "x".repeat(500));
        assert_eq!(refusal_reason(&long, &[]).len(), 200);
    }

    #[tokio::test]
    async fn preflight_establishes_a_missing_pin_from_a_verified_credential() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap();
        assert_eq!(read_credential_pin(dir), None);
        let credential = basic("railway", "carried");
        let pin = resolve_pin(
            dir,
            dir,
            &Operation::Preflight,
            Some(&credential),
            |password| async move {
                assert_eq!(password, "carried");
                Ok(true)
            },
        )
        .await
        .unwrap();
        // Coupled, exactly as post_bootstrap would have pinned it, and on disk
        // for every later request and boot.
        assert_eq!(pin, pin_of("carried"));
        assert_eq!(read_credential_pin(dir), Some(pin_of("carried")));
    }

    #[tokio::test]
    async fn a_rotation_request_leaves_the_volume_copy_current_once_it_has_authenticated() {
        // The pin came with the clone; this member never rebooted since, so
        // the volume has no copy of it yet.
        let pgdata = tempfile::tempdir().unwrap();
        let volume = tempfile::tempdir().unwrap();
        let (dir, root) = (
            pgdata.path().to_str().unwrap(),
            volume.path().to_str().unwrap(),
        );
        write_credential_pin(dir, &pin_of("cloned")).unwrap();
        assert_eq!(crate::patroni::read_credential_pin_copy(root), None);
        let credential = basic("railway", "cloned");
        let pin = resolve_pin(
            dir,
            root,
            &Operation::Prepare,
            Some(&credential),
            |_| async { panic!("verifier must not run when a pin exists") },
        )
        .await
        .unwrap();
        assert_eq!(pin, pin_of("cloned"));
        // Resolving writes nothing: an unauthenticated request must not
        // make this member touch its volume.
        assert_eq!(crate::patroni::read_credential_pin_copy(root), None);
        let wrong = basic("railway", "wrong");
        assert_eq!(
            admit(Some(&wrong), &pin, root),
            Err(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(crate::patroni::read_credential_pin_copy(root), None);
        assert_eq!(admit(Some(&credential), &pin, root), Ok(()));
        assert_eq!(
            crate::patroni::read_credential_pin_copy(root),
            Some(pin_of("cloned"))
        );
    }

    #[tokio::test]
    async fn a_wiped_pgdata_authenticates_against_the_volume_copy_without_writing_into_it() {
        // A reinitialize mid-rotation: PGDATA is empty, the clone is in
        // progress, Postgres is down. The copy is what this member was
        // pinned to; the request that carries it proceeds (and then fails on
        // the database, which the control plane retries) instead of
        // "credential pin unavailable", which the control plane reads as a
        // member that never pinned and redeploys mid-clone.
        let pgdata = tempfile::tempdir().unwrap();
        let volume = tempfile::tempdir().unwrap();
        let (dir, root) = (
            pgdata.path().to_str().unwrap(),
            volume.path().to_str().unwrap(),
        );
        crate::patroni::write_credential_pin_copy(root, &pin_of("pinned")).unwrap();
        let credential = basic("railway", "pinned");
        for operation in [
            Operation::Preflight,
            Operation::Prepare,
            Operation::Database,
            Operation::Member,
            Operation::Verify,
        ] {
            let pin = resolve_pin(dir, root, &operation, Some(&credential), |_| async {
                anyhow::bail!("connection refused")
            })
            .await
            .unwrap();
            assert_eq!(pin, pin_of("pinned"));
        }
        assert_eq!(read_credential_pin(dir), None);
        assert!(std::fs::read_dir(dir).unwrap().next().is_none());
        // Of the five, only prepare writes into PGDATA before it connects:
        // that one is refused while PGDATA has no pin, the others reach the
        // database and are refused by its absence.
        assert!(needs_pgdata_pin(&Operation::Prepare));
        for operation in [
            Operation::Preflight,
            Operation::Database,
            Operation::Member,
            Operation::Verify,
        ] {
            assert!(!needs_pgdata_pin(&operation), "{operation:?}");
        }
        // A request that carries something else is what it was before the
        // copy existed: a preflight verifies it (here: the database is down,
        // so the node is unavailable), a mutating step is refused.
        let other = basic("railway", "other");
        assert_eq!(
            resolve_pin(dir, root, &Operation::Preflight, Some(&other), |_| async {
                anyhow::bail!("connection refused")
            })
            .await,
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(
            resolve_pin(dir, root, &Operation::Member, Some(&other), |_| async {
                panic!("verifier must not run for a mutating step")
            })
            .await,
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
    }

    #[tokio::test]
    async fn a_pin_restored_from_an_old_backup_is_repinned_to_the_copy_the_roles_carry() {
        // The member was re-seeded from the archive after a rotation: the
        // backup's PGDATA carries the pin from before it, the volume's copy
        // the one the rotation moved to. The control plane presents the
        // current password (the copy's).
        let pgdata = tempfile::tempdir().unwrap();
        let volume = tempfile::tempdir().unwrap();
        let (dir, root) = (
            pgdata.path().to_str().unwrap(),
            volume.path().to_str().unwrap(),
        );
        write_credential_pin(dir, &pin_of("backup")).unwrap();
        crate::patroni::write_credential_pin_copy(root, &pin_of("current")).unwrap();
        let current = basic("railway", "current");
        // The roles carry the copy's password: PGDATA is re-pinned to it.
        let verified = std::cell::Cell::new(None);
        let pin = resolve_pin(
            dir,
            root,
            &Operation::Preflight,
            Some(&current),
            |password| {
                verified.set(Some(password));
                async { Ok(true) }
            },
        )
        .await
        .unwrap();
        assert_eq!(verified.take().as_deref(), Some("current"));
        assert_eq!(pin, pin_of("current"));
        assert_eq!(read_credential_pin(dir), Some(pin_of("current")));
        assert_eq!(
            crate::patroni::read_credential_pin_copy(root),
            Some(pin_of("current"))
        );

        // Roles that do not carry the copy's password, or a verifier that
        // cannot run: PGDATA's pin stands, and the request is refused by it
        // (401, as before the copy existed), never left without a pin.
        write_credential_pin(dir, &pin_of("backup")).unwrap();
        for outcome in [Ok(false), Err(anyhow::anyhow!("connection refused"))] {
            let pin = resolve_pin(dir, root, &Operation::Member, Some(&current), |_| async {
                outcome
            })
            .await
            .unwrap();
            assert_eq!(pin, pin_of("backup"));
            assert_eq!(
                admit(Some(&current), &pin, root),
                Err(StatusCode::UNAUTHORIZED)
            );
            assert_eq!(read_credential_pin(dir), Some(pin_of("backup")));
        }
        // A request carrying PGDATA's own password never consults the copy.
        let backup = basic("railway", "backup");
        let pin = resolve_pin(dir, root, &Operation::Preflight, Some(&backup), |_| async {
            panic!("verifier must not run when the request carries PGDATA's pin")
        })
        .await
        .unwrap();
        assert_eq!(pin, pin_of("backup"));
    }

    #[tokio::test]
    async fn verify_establishes_a_missing_pin_too() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap();
        let credential = basic("railway", "carried");
        let pin = resolve_pin(dir, dir, &Operation::Verify, Some(&credential), |_| async {
            Ok(true)
        })
        .await
        .unwrap();
        assert_eq!(pin, pin_of("carried"));
        assert_eq!(read_credential_pin(dir), Some(pin_of("carried")));
    }

    #[tokio::test]
    async fn mutating_operations_keep_refusing_without_a_pin() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap();
        let credential = basic("railway", "carried");
        for operation in [Operation::Prepare, Operation::Database, Operation::Member] {
            let outcome = resolve_pin(dir, dir, &operation, Some(&credential), |_| async {
                panic!("verifier must not run for {operation:?}")
            })
            .await;
            assert_eq!(
                outcome,
                Err(StatusCode::SERVICE_UNAVAILABLE),
                "{operation:?}"
            );
        }
        assert_eq!(read_credential_pin(dir), None);
    }

    #[tokio::test]
    async fn a_credential_the_roles_do_not_carry_is_unauthorized_and_pins_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap();
        let credential = basic("railway", "edited-variable");
        let outcome = resolve_pin(
            dir,
            dir,
            &Operation::Preflight,
            Some(&credential),
            |_| async { Ok(false) },
        )
        .await;
        assert_eq!(outcome, Err(StatusCode::UNAUTHORIZED));
        assert_eq!(read_credential_pin(dir), None);

        // No credential, or one for another user: nothing to verify.
        assert_eq!(
            resolve_pin(dir, dir, &Operation::Preflight, None, |_| async {
                Ok(true)
            })
            .await,
            Err(StatusCode::UNAUTHORIZED)
        );
        let other = basic("someone", "carried");
        assert_eq!(
            resolve_pin(dir, dir, &Operation::Preflight, Some(&other), |_| async {
                Ok(true)
            })
            .await,
            Err(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(read_credential_pin(dir), None);
    }

    #[tokio::test]
    async fn a_verifier_that_cannot_run_is_unavailable_not_unauthorized() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap();
        let credential = basic("railway", "carried");
        let outcome = resolve_pin(dir, dir, &Operation::Verify, Some(&credential), |_| async {
            anyhow::bail!("connection refused")
        })
        .await;
        assert_eq!(outcome, Err(StatusCode::SERVICE_UNAVAILABLE));
        assert_eq!(read_credential_pin(dir), None);
    }

    #[test]
    fn quotes_sql_and_pgpass_without_interpolation() {
        assert_eq!(quote_identifier("odd\"role"), "\"odd\"\"role\"");
        assert_eq!(quote_literal("a'b"), "'a''b'");
        assert_eq!(pgpass("a:b\\c"), "a\\:b\\\\c");
    }
    #[test]
    fn atomic_files_are_private_and_complete() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pgpass");
        atomic_write(path.to_str().unwrap(), b"secret").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"secret");
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
