//! Local half of an ordered cluster rotation. SQL is executed in this
//! container; the control plane owns the etcd change and cluster ordering.
use super::HealthServerConfig;
use crate::patroni::{read_credential_pin, write_credential_pin, PinnedCredentials};
use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    Json,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{io::Write, os::unix::fs::OpenOptionsExt, time::Duration};
use subtle::ConstantTimeEq;
use tokio::sync::Mutex;
use tokio_postgres::NoTls;
use tracing::info;

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

/// The pin a rotation request authenticates against.
///
/// A volume without a pin used to refuse every rotation with 503. That left
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
async fn resolve_pin<F, Fut>(
    data_dir: &str,
    operation: &Operation,
    credential: Option<&[u8]>,
    verify: F,
) -> Result<PinnedCredentials, StatusCode>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    if let Some(pin) = read_credential_pin(data_dir) {
        return Ok(pin);
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
    write_credential_pin(data_dir, &pin).map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
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
    let client = connect(config, user, &password).await?;
    roles_match(&client, &[user, replication_user, app_user], &password).await
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
        resolve_pin(dir, &request.operation, supplied.as_deref(), |password| {
            roles_carry(&config, &yaml, password)
        }),
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
    let expected = format!("{ROTATION_USER}:{}", pin.superuser_pass);
    if !supplied.is_some_and(|s| bool::from(s.as_slice().ct_eq(expected.as_bytes()))) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
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
    match tokio::time::timeout(Duration::from_secs(35), apply(&config, yaml, request)).await {
        Ok(Ok(value)) => (StatusCode::OK, Json(value)),
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"credential rotation could not be verified"})),
        ),
    }
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
            anyhow::ensure!(
                yaml["etcd3"]["username"].as_str() == Some("root"),
                "unsupported etcd identity"
            );
            for value in [
                &yaml["postgresql"]["authentication"]["superuser"]["password"],
                &yaml["postgresql"]["authentication"]["replication"]["password"],
                &yaml["postgresql"]["app_user"]["password"],
                &yaml["etcd3"]["password"],
                &yaml["restapi"]["authentication"]["password"],
            ] {
                anyhow::ensure!(
                    value.as_str() == Some(&request.new_password),
                    "cluster credentials are not coupled"
                );
            }
        }
        Operation::Prepare => {
            // Persist intent before any database or DCS mutation. Recovery can
            // finish adoption even while the control-plane worker is down.
            atomic_write(
                &format!("{data_dir}/.railway_rotation"),
                &serde_json::to_vec(&request)?,
            )?;
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
            let rest_user = text(&yaml["restapi"]["authentication"]["username"])?.to_string();
            let old_rest_password =
                text(&yaml["restapi"]["authentication"]["password"])?.to_string();
            for role in ["superuser", "replication", "rewind"] {
                if !yaml["postgresql"]["authentication"][role].is_null() {
                    yaml["postgresql"]["authentication"][role]["password"] =
                        request.new_password.clone().into();
                }
            }
            yaml["postgresql"]["app_user"]["password"] = request.new_password.clone().into();
            yaml["etcd3"]["password"] = request.new_password.clone().into();
            yaml["restapi"]["authentication"]["password"] = request.new_password.clone().into();
            if !yaml["ctl"]["authentication"].is_null() {
                yaml["ctl"]["authentication"]["password"] = request.new_password.clone().into();
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
            write_credential_pin(
                &data_dir,
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
            let pin = read_credential_pin(&data_dir).context("pin missing")?;
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
            match std::fs::remove_file(format!("{data_dir}/.railway_rotation")) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
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
            if read_credential_pin(dir).is_some_and(|p| p.superuser_pass == request.new_password)
                && yaml["etcd3"]["password"].as_str() == Some(&request.new_password)
            {
                return Ok::<(), anyhow::Error>(());
            }
            let user = text(&yaml["postgresql"]["authentication"]["superuser"]["username"])?;
            connect(&config, user, &request.new_password).await?;
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
        let pin = resolve_pin(dir, &Operation::Preflight, Some(&credential), |_| async {
            panic!("verifier must not run when a pin exists")
        })
        .await
        .unwrap();
        assert_eq!(pin, pin_of("pinned"));
        // The handler's constant-time comparison still rejects the wrong
        // credential afterwards; resolve_pin only says what to compare to.
    }

    #[tokio::test]
    async fn preflight_establishes_a_missing_pin_from_a_verified_credential() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap();
        assert_eq!(read_credential_pin(dir), None);
        let credential = basic("railway", "carried");
        let pin = resolve_pin(
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
    async fn verify_establishes_a_missing_pin_too() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap();
        let credential = basic("railway", "carried");
        let pin = resolve_pin(dir, &Operation::Verify, Some(&credential), |_| async {
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
            let outcome = resolve_pin(dir, &operation, Some(&credential), |_| async {
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
        let outcome = resolve_pin(dir, &Operation::Preflight, Some(&credential), |_| async {
            Ok(false)
        })
        .await;
        assert_eq!(outcome, Err(StatusCode::UNAUTHORIZED));
        assert_eq!(read_credential_pin(dir), None);

        // No credential, or one for another user: nothing to verify.
        assert_eq!(
            resolve_pin(dir, &Operation::Preflight, None, |_| async { Ok(true) }).await,
            Err(StatusCode::UNAUTHORIZED)
        );
        let other = basic("someone", "carried");
        assert_eq!(
            resolve_pin(dir, &Operation::Preflight, Some(&other), |_| async {
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
        let outcome = resolve_pin(dir, &Operation::Verify, Some(&credential), |_| async {
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
