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
use tokio::process::Command;
use tracing::{info, warn};

const PATRONI_REST: &str = "http://localhost:8008";
const EXPECTED_ARCHIVE_MODE: &str = "on";
pub(crate) const EXPECTED_ARCHIVE_COMMAND: &str =
    "/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p";
// Bounded poll before concluding Patroni's own dynamic-config sync missed
// this node, rather than just being a poll_wait cycle behind a patch we
// ourselves may have just issued moments ago.
const LIVE_CHECK_ATTEMPTS: u32 = 5;
const LIVE_CHECK_INTERVAL: Duration = Duration::from_secs(2);

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
            Err(e) => {
                warn!(error = %e, "Patroni REST unreachable");
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Live `archive_command` / `archive_timeout` GUC values read directly from
/// the running Postgres, bypassing Patroni and DCS entirely.
struct LiveArchiveGucs {
    archive_command: String,
    archive_timeout_secs: i64,
}

/// Query the live `archive_command` / `archive_timeout` GUCs over the unix
/// socket. `Err` here means "couldn't ask" (postgres not up yet, socket not
/// ready) — distinct from a successful query that reads back an empty/wrong
/// value, which is the actual defect this module exists to catch.
async fn query_live_archive_gucs(superuser: &str) -> Result<LiveArchiveGucs> {
    let out = Command::new("psql")
        .args([
            "-U",
            superuser,
            "-h",
            "/var/run/postgresql",
            "-tAXq",
            "-F",
            "\t",
            "-c",
            // pg_settings.setting (not current_setting()/SHOW) for
            // archive_timeout: current_setting() applies "nice" unit
            // formatting (e.g. "1min" instead of "60") for GUC_UNIT_S
            // parameters, which a bare ::int cast rejects. pg_settings.setting
            // is always the raw value in the GUC's base unit (seconds here),
            // with no suffix.
            "SELECT current_setting('archive_command'), \
             (SELECT setting FROM pg_catalog.pg_settings WHERE name = 'archive_timeout')::int",
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await
        .context("spawn psql")?;

    if !out.status.success() {
        anyhow::bail!(
            "live archive GUC query failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    // trim_end_matches (not trim()) — the first column is empty exactly
    // when this function's caller most needs a correct answer (a broken
    // archive_command), and generic trim() strips the leading tab
    // separator along with it, silently shifting the second column into
    // the first field and reporting "missing" instead of "empty".
    let mut fields = stdout.trim_end_matches('\n').splitn(2, '\t');
    let archive_command = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing archive_command field in psql output"))?
        .to_string();
    let archive_timeout_secs = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing archive_timeout field in psql output"))?
        .parse::<i64>()
        .context("parse archive_timeout")?;

    Ok(LiveArchiveGucs {
        archive_command,
        archive_timeout_secs,
    })
}

/// Force `archive_command` + `archive_timeout` directly via `ALTER SYSTEM`
/// + `pg_reload_conf()`, bypassing Patroni's own dynamic-config application
/// entirely. Only called once we've already proven Patroni's own path
/// silently failed to apply values DCS already agrees on — going back
/// through Patroni's `/config` PATCH would just re-enter the exact
/// mechanism that already failed to take effect. Both GUCs are
/// `PGC_SIGHUP` (reload-only, no restart), and `ALTER SYSTEM` writes to
/// `postgresql.auto.conf`, which always takes priority over Patroni's
/// rendered `postgresql.conf` — the correction survives any later
/// Patroni-driven re-render.
async fn force_live_archive_gucs(superuser: &str, archive_timeout_secs: i64) -> Result<()> {
    // ALTER SYSTEM cannot run inside a transaction block, and psql wraps a
    // single `-c` argument containing multiple `;`-separated statements in
    // one implicit transaction — each statement needs its own `-c` flag so
    // it runs as its own simple-query round trip.
    let alter_command_sql =
        format!("ALTER SYSTEM SET archive_command = '{EXPECTED_ARCHIVE_COMMAND}';");
    let alter_timeout_sql = format!("ALTER SYSTEM SET archive_timeout = {archive_timeout_secs};");
    let out = Command::new("psql")
        .args([
            "-U",
            superuser,
            "-h",
            "/var/run/postgresql",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            &alter_command_sql,
            "-c",
            &alter_timeout_sql,
            "-c",
            "SELECT pg_reload_conf();",
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await
        .context("spawn psql")?;

    if !out.status.success() {
        anyhow::bail!(
            "failed to force live archive GUCs: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    Ok(())
}

/// Verify the live Postgres GUCs actually match `expected_archive_timeout`,
/// self-healing if Patroni's own sync silently missed this node.
///
/// DCS being correct is necessary but not sufficient. Patroni's dynamic
/// config sync has a startup race: a node's first `set_dynamic_configuration`
/// call can land while its own Postgres isn't yet in the `RUNNING` state
/// (e.g. mid-basebackup on a freshly-joining replica) — Patroni's config
/// writer skips the actual `postgresql.conf` write + reload when that
/// happens, but still marks that DCS config *version* as "seen" internally,
/// and never revisits an already-seen version even though it was never
/// truly applied. DCS then reads as correct forever while the live GUC
/// stays empty. Confirmed happening in practice: a freshly-booted replica
/// with a provably-correct DCS config and a live `archive_command` of ''.
/// If that replica is later promoted, WAL archiving silently never starts.
async fn verify_and_heal_live_archive_config(
    superuser: &str,
    expected_archive_timeout: i64,
) -> Result<()> {
    let mut last_err = None;
    for attempt in 1..=LIVE_CHECK_ATTEMPTS {
        match query_live_archive_gucs(superuser).await {
            Ok(live)
                if live.archive_command == EXPECTED_ARCHIVE_COMMAND
                    && live.archive_timeout_secs == expected_archive_timeout =>
            {
                info!("live archive_command/archive_timeout confirmed applied");
                return Ok(());
            }
            Ok(live) => {
                if attempt < LIVE_CHECK_ATTEMPTS {
                    tokio::time::sleep(LIVE_CHECK_INTERVAL).await;
                    continue;
                }
                warn!(
                    live_archive_command = %live.archive_command,
                    live_archive_timeout = live.archive_timeout_secs,
                    "live archive config diverged from DCS despite DCS being correct — \
                     Patroni's dynamic-config sync silently missed this node; forcing directly"
                );
                force_live_archive_gucs(superuser, expected_archive_timeout).await?;
                info!("live archive_command/archive_timeout corrected via ALTER SYSTEM + pg_reload_conf()");
                return Ok(());
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(LIVE_CHECK_INTERVAL).await;
            }
        }
    }
    // Every attempt failed to even query (postgres unreachable this whole
    // window) — propagate so the caller's retry-with-backoff loop tries
    // again later, same as any other transient failure in this module.
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("live archive GUC query never succeeded")))
        .context("verify live archive config")
}

/// Reconcile DCS archive params with env-driven intent.
///
/// - `WAL_ARCHIVE_BUCKET` set: assert `archive_mode=on` /
///   `archive_command='/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p'` /
///   `archive_timeout=60` are present in DCS. Patches them in if missing.
///   Then verifies the values actually took effect on the live Postgres
///   (see `verify_and_heal_live_archive_config`) — DCS-correct is not
///   proof-of-application.
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
    let track_commit_timestamp = params
        .and_then(|m| m.get("track_commit_timestamp"))
        .and_then(|v| v.as_str());

    if enabled {
        let dcs_matches = archive_mode == Some(EXPECTED_ARCHIVE_MODE)
            && archive_command == Some(EXPECTED_ARCHIVE_COMMAND)
            && archive_timeout == Some(expected_archive_timeout)
            && track_commit_timestamp == Some("on");

        if dcs_matches {
            info!("DCS archive config matches env-driven intent (PITR enabled)");
        } else {
            warn!(
                current_mode = ?archive_mode,
                current_command = ?archive_command,
                current_timeout = ?archive_timeout,
                current_track_commit_timestamp = ?track_commit_timestamp,
                "DCS archive config drifted from env-driven intent — re-asserting"
            );

            let patch = json!({
                "postgresql": {
                    "parameters": {
                        "archive_mode": EXPECTED_ARCHIVE_MODE,
                        "archive_command": EXPECTED_ARCHIVE_COMMAND,
                        "archive_timeout": expected_archive_timeout,
                        "track_commit_timestamp": "on",
                    }
                }
            });

            send_patch(&client, &patch).await?;
            info!("DCS archive params patched in (PITR enabled)");
        }

        verify_and_heal_live_archive_config(&config.superuser, expected_archive_timeout).await?;
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
        // track_commit_timestamp is left in place: it's harmless when set
        // without archiving, and a no-op cost on inactive clusters.
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
