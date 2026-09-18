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

const CONFIG: &str = "/etc/patroni/patroni.yml";
static ROTATION: Mutex<()> = Mutex::const_new(());
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rotation {
    operation: Operation,
    new_password: String,
    current_password: String,
}
#[derive(Clone, Deserialize, Serialize)]
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
    let Some(pin) = read_credential_pin(dir) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"credential pin unavailable"})),
        );
    };
    let expected = format!("railway:{}", pin.superuser_pass);
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("basic"))
        .and_then(|(_, token)| base64::engine::general_purpose::STANDARD.decode(token).ok());
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
            for role in [&user, &replication_user, &app_user] {
                connect(config, role, &request.new_password).await?;
            }
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
            if connect(config, &user, &request.new_password).await.is_err() {
                let mut client = connect(config, &user, &request.current_password).await?;
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
            connect(config, &user, &request.new_password).await?;
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
            for role in [&user, &replication_user, &app_user] {
                connect(config, role, &request.new_password).await?;
            }
            let pin = read_credential_pin(&data_dir).context("pin missing")?;
            anyhow::ensure!(
                pin.superuser_pass == request.new_password
                    && pin.repl_pass == request.new_password
                    && pin.app_pass == request.new_password,
                "pin differs"
            );
            if request.current_password != request.new_password {
                anyhow::ensure!(
                    connect(config, &user, &request.current_password)
                        .await
                        .err()
                        .and_then(|error| error.downcast::<tokio_postgres::Error>().ok())
                        .is_some_and(|error| error.code()
                            == Some(&tokio_postgres::error::SqlState::INVALID_PASSWORD)),
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
    Ok(json!({"version": 1, "leader": leader}))
}

#[cfg(test)]
mod tests {
    use super::*;
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
