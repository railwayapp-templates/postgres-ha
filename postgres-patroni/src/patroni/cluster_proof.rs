//! Ask the running cluster which password its roles carry.
//!
//! A member pins a password only once the cluster has vouched for it: an
//! edited variable alone never alters a dataset credential. The witness is
//! the elected leader, found through the DCS, whose stored verifiers for the
//! superuser, replication and app roles are held against the password. etcd
//! is a second witness where it authenticates; on a cluster created before
//! the etcd root password it accepts anyone and the leader alone decides.
//!
//! Two callers: the boot (a rotation staged before a restart, or a member
//! that was not running while the cluster rotated) and a running member that
//! booted on its pin because the cluster had not answered yet.

use super::config::Credential;
use super::etcd_kv::etcd_range;
use super::etcd_preflight::{probe_etcd_credential, EtcdAuthProbe};
use super::scram::{verify, VerifierCheck};
use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use std::time::Duration;

/// Where the cluster lives, and the roles a rotation keeps on one password.
#[derive(Debug, Clone, Copy)]
pub struct Cluster<'a> {
    pub etcd_hosts: &'a str,
    pub scope: &'a str,
    pub superuser: &'a str,
    pub replication_user: &'a str,
    pub app_user: &'a str,
}

/// What the leader said about the passwords it was asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaderAnswer {
    /// Its superuser, replication and app roles all carry this one.
    Carries(String),
    /// It answered about every one of them, and its roles carry none.
    CarriesNone,
    /// No leader answered: the DCS names none, or the one it names cannot be
    /// reached right now.
    Silent,
}

/// What the cluster as a whole said about one password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterAnswer {
    /// The leader's roles carry it (and etcd accepted it, where it checks).
    Carries,
    /// etcd refused it, or the leader's roles do not carry it.
    Refuses,
    /// Nothing answered yet.
    Silent,
}

/// Hold `candidates`, in order, against the leader's roles. `dcs` is the
/// credential the DCS is read with, `None` where etcd does not authenticate.
/// Never logs a connection error: some drivers include the credential.
pub async fn ask_leader(
    cluster: &Cluster<'_>,
    dcs: Option<&Credential>,
    candidates: &[String],
) -> LeaderAnswer {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    else {
        return LeaderAnswer::Silent;
    };
    for host in cluster.etcd_hosts.split(',') {
        let Ok(leader) = leader_endpoint(&client, host, cluster.scope, dcs).await else {
            continue;
        };
        let mut answers = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let answer = roles_carry(&leader, cluster, candidate).await.ok();
            if answer == Some(true) {
                return LeaderAnswer::Carries(candidate.clone());
            }
            answers.push(answer);
        }
        if carries_none(&answers) {
            return LeaderAnswer::CarriesNone;
        }
    }
    LeaderAnswer::Silent
}

/// Ask etcd, then the leader, about one password.
pub async fn ask_cluster(cluster: &Cluster<'_>, password: &str) -> ClusterAnswer {
    let credential = Credential {
        username: "root".into(),
        password: password.to_string(),
    };
    let dcs = match probe_etcd_credential(cluster.etcd_hosts, &credential, Duration::from_secs(2))
        .await
    {
        EtcdAuthProbe::Accepted => Some(credential),
        EtcdAuthProbe::NotEnabled => None,
        EtcdAuthProbe::Rejected => return ClusterAnswer::Refuses,
        EtcdAuthProbe::Inconclusive => return ClusterAnswer::Silent,
    };
    match ask_leader(cluster, dcs.as_ref(), &[password.to_string()]).await {
        LeaderAnswer::Carries(_) => ClusterAnswer::Carries,
        LeaderAnswer::CarriesNone => ClusterAnswer::Refuses,
        LeaderAnswer::Silent => ClusterAnswer::Silent,
    }
}

/// Whether one leader's answers amount to "none of these": it answered about
/// every candidate (`None` is a question it could not be asked) and said no.
fn carries_none(answers: &[Option<bool>]) -> bool {
    !answers.is_empty() && answers.iter().all(|answer| *answer == Some(false))
}

/// The leader's Postgres endpoint, as Patroni publishes it in the DCS.
async fn leader_endpoint(
    client: &reqwest::Client,
    host: &str,
    scope: &str,
    dcs: Option<&Credential>,
) -> Result<reqwest::Url> {
    let leader_key = BASE64.encode(format!("/service/{scope}/leader"));
    let response = etcd_range(client, host, &leader_key, dcs).await?;
    let value: serde_json::Value = response.error_for_status()?.json().await?;
    let leader = String::from_utf8(
        BASE64.decode(value["kvs"][0]["value"].as_str().context("leader absent")?)?,
    )?;
    let member_key = BASE64.encode(format!("/service/{scope}/members/{leader}"));
    let response = etcd_range(client, host, &member_key, dcs).await?;
    let value: serde_json::Value = response.error_for_status()?.json().await?;
    let member: serde_json::Value = serde_json::from_slice(
        &BASE64.decode(value["kvs"][0]["value"].as_str().context("member absent")?)?,
    )?;
    Ok(reqwest::Url::parse(
        member["conn_url"]
            .as_str()
            .context("primary endpoint absent")?,
    )?)
}

/// Whether the leader's three roles carry `password`. `Ok(false)` is the
/// leader's own answer (it refused the login, or a stored verifier differs);
/// an error is a question that could not be asked.
async fn roles_carry(leader: &reqwest::Url, cluster: &Cluster<'_>, password: &str) -> Result<bool> {
    let connected = tokio_postgres::Config::new()
        .host(leader.host_str().context("missing host")?)
        .port(leader.port().unwrap_or(5432))
        .user(cluster.superuser)
        .password(password)
        .dbname("postgres")
        .connect_timeout(Duration::from_secs(3))
        .connect(tokio_postgres::NoTls)
        .await;
    let (sql, connection) = match connected {
        Ok(pair) => pair,
        Err(e) if password_refused(&e) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    tokio::spawn(async move {
        let _ = connection.await;
    });
    anyhow::ensure!(
        !sql.query_one("SELECT pg_is_in_recovery()", &[])
            .await?
            .get::<_, bool>(0),
        "leader changed"
    );
    for user in [
        cluster.superuser,
        cluster.replication_user,
        cluster.app_user,
    ] {
        let row = sql
            .query_one(
                "SELECT rolpassword FROM pg_authid WHERE rolname = $1",
                &[&user],
            )
            .await?;
        let verifier: Option<String> = row.get(0);
        match verifier
            .as_deref()
            .map(|value| verify(value, user, password))
        {
            Some(VerifierCheck::Matches) => {}
            Some(VerifierCheck::Differs) | None => return Ok(false),
            Some(VerifierCheck::Unsupported(_)) => anyhow::bail!("unsupported password verifier"),
        }
    }
    Ok(true)
}

/// Whether Postgres refused the login because of the password (SQLSTATE
/// 28P01). A missing pg_hba entry is 28000 and says nothing about it.
fn password_refused(error: &tokio_postgres::Error) -> bool {
    error
        .as_db_error()
        .is_some_and(|db| *db.code() == tokio_postgres::error::SqlState::INVALID_PASSWORD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leader_carries_none_only_when_it_answered_about_every_password() {
        assert!(carries_none(&[Some(false)]));
        assert!(carries_none(&[Some(false), Some(false)]));
        // One question could not be asked (a timeout, a verifier this image
        // cannot read): the password behind it may be the one.
        assert!(!carries_none(&[Some(false), None]));
        assert!(!carries_none(&[None]));
        // Nothing was asked.
        assert!(!carries_none(&[]));
    }
}
