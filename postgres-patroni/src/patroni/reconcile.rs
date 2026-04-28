//! Reconcile env-driven pgBackRest archive config into Patroni DCS.
//!
//! `bootstrap.dcs.postgresql.parameters` in `patroni.yml` only seeds DCS at
//! first cluster init. After that, DCS in etcd is authoritative; Patroni
//! ignores `bootstrap.dcs` on subsequent restarts, on etcd state loss, and
//! after any operator `patronictl edit-config` that clobbers archive params.
//! Without this reconcile, the env-var enable/disable contract would be
//! broken on existing clusters:
//!
//! - Set `WAL_ARCHIVE_BUCKET` on an existing cluster, redeploy → DCS stays
//!   unarchived → cluster runs without archiving despite the env vars
//!   being present, looking enabled but archiving nothing.
//! - Unset `WAL_ARCHIVE_BUCKET` on a previously-enabled cluster, redeploy
//!   → DCS still has `archive_mode=on` and `archive_command=...` →
//!   archive-push wrapper fires but the S3 creds are gone → queue-max
//!   eventually trips and WAL is silently dropped; PITR coverage degrades
//!   invisibly. Or if pgbackrest.conf were absent, archive-push would
//!   fail synchronously and the wrapper's pg_wal-threshold drop would
//!   kick in (still keeps DB up, but louder than necessary).
//!
//! This runs once per node startup, after Patroni reports healthy, and uses
//! Patroni's REST API (`PATCH /config`) so it goes through the same
//! DCS-merge path Patroni itself uses. Idempotent: if DCS already matches
//! the env-driven intent, no write is issued. Safe to run on every node
//! concurrently — Patroni's optimistic concurrency in the config writer
//! handles the race.
//!
//! `archive_mode` is a `PGC_POSTMASTER` parameter — applying a change
//! requires a full Postgres restart, which Patroni flags as `pending_restart`
//! but does not perform automatically. We deliberately do NOT auto-issue
//! `POST /restart` here: each node firing its own restart on startup risks
//! 3-simultaneous-restart cluster flap. The dashboard PITR enable/disable
//! flow owns the rolling-restart choreography. This reconcile only ensures
//! DCS cannot drift from env-var intent.

use super::Config;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tracing::{info, warn};

const PATRONI_REST: &str = "http://localhost:8008";
const EXPECTED_ARCHIVE_MODE: &str = "on";
const EXPECTED_ARCHIVE_COMMAND: &str = "/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p";

/// Wait for Patroni's REST API to respond before reconciling. Patroni starts
/// shortly after `patroni-runner` spawns it, but there's a startup window
/// (config load, etcd connect, leader election) before `/config` answers.
async fn wait_for_patroni_rest(client: &reqwest::Client) -> Result<()> {
    let max_wait = Duration::from_secs(120);
    let poll_interval = Duration::from_secs(2);
    let start = std::time::Instant::now();

    loop {
        if start.elapsed() > max_wait {
            anyhow::bail!(
                "Timed out waiting {:?} for Patroni REST API at {PATRONI_REST}",
                max_wait
            );
        }

        match client.get(format!("{PATRONI_REST}/config")).send().await {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                warn!(status = %resp.status(), "Patroni REST not yet ready");
            }
            Err(_) => {
                // Connection refused / timeout while Patroni is still booting
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Reconcile DCS archive params with env-driven intent.
///
/// - `WAL_ARCHIVE_BUCKET` set: assert `archive_mode=on` /
///   `archive_command='/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p'` /
///   `archive_timeout=60` are present in DCS. Patches them in if missing.
/// - `WAL_ARCHIVE_BUCKET` unset: remove those keys from DCS so leftover
///   archive_mode from a previous enable doesn't keep firing
///   archive_command after disable.
/// - Idempotent: no patch issued when DCS already matches intent.
pub async fn reconcile_pgbackrest_archive_config(config: &Config) -> Result<()> {
    let enabled = config.wal_archive_bucket.is_some();
    let expected_archive_timeout = config.archive_timeout_secs;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build reqwest client")?;

    wait_for_patroni_rest(&client).await?;

    let current: Value = client
        .get(format!("{PATRONI_REST}/config"))
        .send()
        .await
        .context("GET /config")?
        .error_for_status()
        .context("GET /config status")?
        .json()
        .await
        .context("parse /config response")?;

    let params = current
        .get("postgresql")
        .and_then(|p| p.get("parameters"))
        .and_then(|p| p.as_object());

    let archive_mode = params
        .and_then(|m| m.get("archive_mode"))
        .and_then(|v| v.as_str());
    let archive_command = params
        .and_then(|m| m.get("archive_command"))
        .and_then(|v| v.as_str());
    let archive_timeout = params
        .and_then(|m| m.get("archive_timeout"))
        .and_then(|v| v.as_i64());

    if enabled {
        if archive_mode == Some(EXPECTED_ARCHIVE_MODE)
            && archive_command == Some(EXPECTED_ARCHIVE_COMMAND)
            && archive_timeout == Some(expected_archive_timeout)
        {
            info!("DCS archive config matches env-driven intent (PITR enabled)");
            return Ok(());
        }

        warn!(
            current_mode = ?archive_mode,
            current_command = ?archive_command,
            current_timeout = ?archive_timeout,
            "DCS archive config drifted from env-driven intent — re-asserting"
        );

        let patch = json!({
            "postgresql": {
                "parameters": {
                    "archive_mode": EXPECTED_ARCHIVE_MODE,
                    "archive_command": EXPECTED_ARCHIVE_COMMAND,
                    "archive_timeout": expected_archive_timeout,
                }
            }
        });

        send_patch(&client, &patch).await?;
        info!("DCS archive params patched in (PITR enabled)");
    } else {
        if archive_mode.is_none() && archive_command.is_none() && archive_timeout.is_none() {
            info!("DCS archive config already absent (PITR disabled)");
            return Ok(());
        }

        warn!(
            current_mode = ?archive_mode,
            current_command = ?archive_command,
            "DCS still has archive params but WAL_ARCHIVE_BUCKET is unset — clearing"
        );

        // null in PATCH /config removes the key from the merged DCS config.
        let patch = json!({
            "postgresql": {
                "parameters": {
                    "archive_mode": Value::Null,
                    "archive_command": Value::Null,
                    "archive_timeout": Value::Null,
                }
            }
        });

        send_patch(&client, &patch).await?;
        info!("DCS archive params cleared (PITR disabled)");
    }

    Ok(())
}

async fn send_patch(client: &reqwest::Client, patch: &Value) -> Result<()> {
    let resp = client
        .patch(format!("{PATRONI_REST}/config"))
        .json(patch)
        .send()
        .await
        .context("PATCH /config")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("PATCH /config failed: {status} {body}");
    }

    Ok(())
}
