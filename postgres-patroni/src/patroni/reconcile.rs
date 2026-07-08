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
            Err(e) => {
                warn!(error = %e, "Patroni REST unreachable");
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Current DCS-side view of the archive params this reconcile owns, read
/// from `GET /config`. Bundled so the decision logic (`compute_archive_reconcile_patch`)
/// stays a pure function testable without an HTTP mock.
struct CurrentDcsArchiveParams<'a> {
    archive_mode: Option<&'a str>,
    archive_command: Option<&'a str>,
    archive_timeout: Option<i64>,
    track_commit_timestamp: Option<&'a str>,
    restore_command: Option<&'a str>,
}

/// Pure decision: given env-driven intent and the current DCS params, returns
/// the `PATCH /config` body to send, or `None` if DCS already matches intent
/// (no-op — reconcile is called on every node startup, so this keeps it from
/// spamming a write every boot on a stable cluster).
///
/// `enabled=false` clears `archive_mode` / `archive_command` /
/// `archive_timeout` / `restore_command`; `track_commit_timestamp` is left in
/// place (harmless without archiving, no-op cost on inactive clusters). The
/// absence check must include `restore_command`: bootstrap.dcs seeds it on
/// archiving-born clusters, and if a prior disable's patch predates
/// `restore_command` existing (or only partially applied), checking just the
/// first three fields would short-circuit to "already absent" and leave
/// `restore_command` stuck in DCS forever — every standby spamming a failing
/// archive-get against creds that are gone.
fn compute_archive_reconcile_patch(
    enabled: bool,
    expected_archive_timeout: i64,
    current: &CurrentDcsArchiveParams,
) -> Option<Value> {
    if enabled {
        if current.archive_mode == Some(EXPECTED_ARCHIVE_MODE)
            && current.archive_command == Some(EXPECTED_ARCHIVE_COMMAND)
            && current.archive_timeout == Some(expected_archive_timeout)
            && current.track_commit_timestamp == Some("on")
        {
            return None;
        }

        Some(json!({
            "postgresql": {
                "parameters": {
                    "archive_mode": EXPECTED_ARCHIVE_MODE,
                    "archive_command": EXPECTED_ARCHIVE_COMMAND,
                    "archive_timeout": expected_archive_timeout,
                    "track_commit_timestamp": "on",
                }
            }
        }))
    } else {
        if current.archive_mode.is_none()
            && current.archive_command.is_none()
            && current.archive_timeout.is_none()
            && current.restore_command.is_none()
        {
            return None;
        }

        // null in PATCH /config removes the key from the merged DCS config.
        Some(json!({
            "postgresql": {
                "parameters": {
                    "archive_mode": Value::Null,
                    "archive_command": Value::Null,
                    "archive_timeout": Value::Null,
                    "restore_command": Value::Null,
                }
            }
        }))
    }
}

/// How long `ensure_restore_command_cleared` polls for the effective GUC to
/// read empty after an archive disable before giving up on this reconcile
/// pass. Generous relative to what it waits on (leader election settling,
/// Patroni applying the cleaned DCS config within a couple of loop_waits) but
/// bounded, because the caller's retry loop re-enters with backoff on failure.
const CLEAR_VERIFY_DEADLINE_SECS: u64 = 120;
const CLEAR_POLL_INTERVAL_SECS: u64 = 5;

/// One observation → next move in the post-disable `restore_command` clear
/// loop. Pure so the decision table is unit-testable without psql/HTTP mocks.
#[derive(Debug, PartialEq, Eq)]
enum ClearStep {
    /// `SHOW restore_command` reads empty — the clear is verified.
    Verified,
    /// Non-empty on the current leader: Patroni never reconciles recovery
    /// params on a primary, so issue the `ALTER SYSTEM` clear ourselves.
    ClearOnPrimary,
    /// Non-empty on a standby (or mid-election): Patroni rewrites standby
    /// recovery config from the now-clean DCS on its own — keep polling
    /// until verification.
    AwaitPatroni,
    /// Postgres isn't answering yet — keep polling.
    AwaitPostgres,
}

fn next_clear_step(shown: Option<&str>, is_leader: bool) -> ClearStep {
    match shown {
        Some("") => ClearStep::Verified,
        Some(_) if is_leader => ClearStep::ClearOnPrimary,
        Some(_) => ClearStep::AwaitPatroni,
        None => ClearStep::AwaitPostgres,
    }
}

/// `SHOW restore_command` via the local socket. `None` when Postgres isn't
/// answering (still starting, mid-clone); `Some(value)` otherwise, with the
/// empty string meaning the GUC is clear.
async fn show_restore_command() -> Option<String> {
    let out = Command::new("psql")
        .args([
            "-U",
            "postgres",
            "-h",
            "/var/run/postgresql",
            "-tAXq",
            "-c",
            "SHOW restore_command",
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn local_node_is_leader(client: &reqwest::Client) -> bool {
    client
        .get(format!("{PATRONI_REST}/leader"))
        .send()
        .await
        .map(|r| r.status() == 200)
        .unwrap_or(false)
}

/// Clear a stale `restore_command` GUC after archiving is disabled, and keep
/// at it until `SHOW restore_command` actually reads empty.
///
/// Why the primary needs help at all: `archive_mode`/`archive_command`/
/// `archive_timeout` get reconciled on every role generically once the DCS
/// disable patch lands (Patroni's usual config-diff-and-apply path).
/// `restore_command` doesn't: it belongs to the recovery-parameter family
/// Patroni only writes/clears via its standby-specific code path
/// (`_adjust_recovery_parameters` → `build_recovery_params`), which never
/// runs for a primary. A node that is (or becomes) leader while carrying an
/// old enabled-archiving config keeps the wrapper invocation in its live
/// `postgresql.conf` — confirmed surviving even a full process restart.
/// Functionally inert (a running primary never executes `restore_command`),
/// but `SHOW` on it right after a disable is misleading, and a later scenario
/// that runs recovery on a still-primary-labeled node (crash restart before
/// promotion settles, certain rewind paths) would pick it up.
///
/// Why a verify loop instead of a one-shot `ALTER SYSTEM`: Patroni sanitizes
/// recovery parameters out of `postgresql.auto.conf` whenever it writes
/// postgres config (`ConfigHandler._sanitize_auto_conf`), so an
/// `ALTER SYSTEM SET restore_command = ''` that races one of Patroni's own
/// config writes gets deleted and the still-rendered `postgresql.conf` line
/// becomes effective again — observed in e2e as the GUC flipping back to the
/// wrapper ~100 ms after a "successful" clear that ran before the DCS patch.
/// The caller therefore invokes this strictly AFTER the disable patch has
/// landed (so Patroni's subsequent config rewrites no longer carry the line),
/// and this loop re-issues the clear until a `SHOW` confirms it stuck.
///
/// Role is re-checked every iteration rather than probed once at boot: on an
/// all-node redeploy (how disables roll out) reconcile typically runs while
/// the election is still settling, and the eventual leader would slip past a
/// single boot-time `/leader` probe — every node would report "not leader"
/// and nobody would clear. Standbys converge without our help (Patroni
/// rewrites their recovery config from the now-clean DCS), so a non-empty
/// value on a non-leader just keeps polling toward verification.
///
/// Returns `Ok` once verified empty. If Postgres never answers within the
/// window and no stale value was ever observed (e.g. a replica mid-clone on a
/// cluster that never archived), also `Ok` — there is nothing observed to
/// fix, and failing would warn-spam every clone through the caller's retry
/// loop. A stale value that survives the whole window is an `Err` so the
/// caller's retry loop keeps pushing with backoff.
async fn ensure_restore_command_cleared(client: &reqwest::Client) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(CLEAR_VERIFY_DEADLINE_SECS);
    let mut observed_stale = false;
    loop {
        let shown = show_restore_command().await;
        let is_leader = match shown.as_deref() {
            Some(v) if !v.is_empty() => local_node_is_leader(client).await,
            _ => false,
        };
        match next_clear_step(shown.as_deref(), is_leader) {
            ClearStep::Verified => {
                if observed_stale {
                    info!("reconcile: restore_command verified empty after archive disable");
                }
                return Ok(());
            }
            ClearStep::ClearOnPrimary => {
                observed_stale = true;
                alter_system_clear_restore_command().await;
            }
            ClearStep::AwaitPatroni => {
                observed_stale = true;
            }
            ClearStep::AwaitPostgres => {}
        }
        if tokio::time::Instant::now() >= deadline {
            if observed_stale {
                anyhow::bail!(
                    "restore_command still set {CLEAR_VERIFY_DEADLINE_SECS}s after the archive-disable reconcile"
                );
            }
            info!("reconcile: postgres unreachable for the whole restore_command verify window with no stale value observed — skipping");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(CLEAR_POLL_INTERVAL_SECS)).await;
    }
}

/// Issue `ALTER SYSTEM SET restore_command = ''` + `pg_reload_conf()` on the
/// local Postgres. An explicit empty string (not `RESET`, which only deletes
/// the `auto.conf` override and would fall back to a still-stale
/// `postgresql.conf` line) is what clears the effective GUC. Best-effort per
/// call — `ensure_restore_command_cleared` owns verification and retry, so a
/// role flip mid-flight ("cannot execute ALTER SYSTEM SET in a read-only
/// transaction") just surfaces as a warn and the next iteration re-decides.
async fn alter_system_clear_restore_command() {
    let alter = Command::new("psql")
        .args([
            "-U",
            "postgres",
            "-h",
            "/var/run/postgresql",
            "-v",
            "ON_ERROR_STOP=1",
            "-tAXq",
            "-c",
            "ALTER SYSTEM SET restore_command = ''",
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await;
    let alter = match alter {
        Ok(o) => o,
        Err(e) => {
            warn!(error = %e, "reconcile: failed to spawn psql clearing restore_command on primary");
            return;
        }
    };
    if !alter.status.success() {
        // /leader reported 200 just before this ran, so a failure here is
        // either a genuine problem or a role change mid-flight — the verify
        // loop re-decides on its next iteration either way.
        warn!(
            stderr = %String::from_utf8_lossy(&alter.stderr),
            "reconcile: ALTER SYSTEM clearing restore_command on primary failed"
        );
        return;
    }

    let reload = Command::new("psql")
        .args([
            "-U",
            "postgres",
            "-h",
            "/var/run/postgresql",
            "-tAXq",
            "-c",
            "SELECT pg_reload_conf()",
        ])
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .output()
        .await;
    match reload {
        Ok(o) if o.status.success() => {
            info!("reconcile: issued ALTER SYSTEM clearing restore_command on primary — verification follows");
        }
        Ok(o) => warn!(
            stderr = %String::from_utf8_lossy(&o.stderr),
            "reconcile: pg_reload_conf after clearing restore_command failed"
        ),
        Err(e) => warn!(error = %e, "reconcile: failed to spawn psql reloading config after clearing restore_command"),
    }
}

/// Reconcile DCS archive params with env-driven intent.
///
/// - `WAL_ARCHIVE_BUCKET` set: assert `archive_mode=on` /
///   `archive_command='/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p'` /
///   `archive_timeout=60` are present in DCS. Patches them in if missing.
/// - `WAL_ARCHIVE_BUCKET` unset: remove those keys from DCS so leftover
///   archive_mode from a previous enable doesn't keep firing
///   archive_command after disable, then clear a stale `restore_command`
///   GUC on the primary and verify it stuck (see
///   `ensure_restore_command_cleared` for why the order matters and why a
///   one-shot clear isn't enough).
/// - Idempotent: no patch issued when DCS already matches intent. The
///   disable-path GUC verification still runs on a no-op patch — a leader
///   elected after the patch already landed can carry the stale value with
///   nothing left to reconcile in DCS.
///
/// The decision itself lives in `compute_archive_reconcile_patch`; this
/// function only does the GET/PATCH I/O around it.
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

    let current_params = CurrentDcsArchiveParams {
        archive_mode: params
            .and_then(|m| m.get("archive_mode"))
            .and_then(|v| v.as_str()),
        archive_command: params
            .and_then(|m| m.get("archive_command"))
            .and_then(|v| v.as_str()),
        archive_timeout: params
            .and_then(|m| m.get("archive_timeout"))
            .and_then(|v| v.as_i64()),
        track_commit_timestamp: params
            .and_then(|m| m.get("track_commit_timestamp"))
            .and_then(|v| v.as_str()),
        restore_command: params
            .and_then(|m| m.get("restore_command"))
            .and_then(|v| v.as_str()),
    };

    match compute_archive_reconcile_patch(enabled, expected_archive_timeout, &current_params) {
        Some(patch) => {
            warn!(
                enabled,
                current_mode = ?current_params.archive_mode,
                current_command = ?current_params.archive_command,
                current_timeout = ?current_params.archive_timeout,
                current_track_commit_timestamp = ?current_params.track_commit_timestamp,
                current_restore_command = ?current_params.restore_command,
                "DCS archive config drifted from env-driven intent — reconciling"
            );
            send_patch(&client, &patch).await?;
            info!(enabled, "DCS archive params reconciled");
        }
        None => {
            info!(
                enabled,
                "DCS archive config already matches env-driven intent"
            );
        }
    }

    if !enabled {
        // Strictly after the DCS patch: a GUC clear issued before it loses
        // the race with Patroni's next config write, which sanitizes the
        // auto.conf override away while postgresql.conf still renders the
        // stale line (see `ensure_restore_command_cleared`).
        ensure_restore_command_cleared(&client).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: CurrentDcsArchiveParams<'static> = CurrentDcsArchiveParams {
        archive_mode: None,
        archive_command: None,
        archive_timeout: None,
        track_commit_timestamp: None,
        restore_command: None,
    };

    const MATCHING: CurrentDcsArchiveParams<'static> = CurrentDcsArchiveParams {
        archive_mode: Some(EXPECTED_ARCHIVE_MODE),
        archive_command: Some(EXPECTED_ARCHIVE_COMMAND),
        archive_timeout: Some(60),
        track_commit_timestamp: Some("on"),
        restore_command: Some("/usr/local/bin/pgbackrest-archive-get-wrapper.sh %f %p"),
    };

    #[test]
    fn enabled_and_matching_is_noop() {
        assert!(compute_archive_reconcile_patch(true, 60, &MATCHING).is_none());
    }

    #[test]
    fn enabled_but_fully_absent_patches_in_all_four_fields() {
        let patch = compute_archive_reconcile_patch(true, 60, &EMPTY)
            .expect("drifted DCS must produce a patch");
        let params = &patch["postgresql"]["parameters"];
        assert_eq!(params["archive_mode"], EXPECTED_ARCHIVE_MODE);
        assert_eq!(params["archive_command"], EXPECTED_ARCHIVE_COMMAND);
        assert_eq!(params["archive_timeout"], 60);
        assert_eq!(params["track_commit_timestamp"], "on");
        // Enable-path patch never touches restore_command — yaml.rs seeds it
        // via bootstrap.dcs on first init; this reconcile only asserts the
        // three PGC_POSTMASTER params + track_commit_timestamp.
        assert!(params.get("restore_command").is_none());
    }

    #[test]
    fn enabled_but_only_track_commit_timestamp_missing_repatches_everything() {
        let current = CurrentDcsArchiveParams {
            track_commit_timestamp: None,
            ..MATCHING
        };
        let patch = compute_archive_reconcile_patch(true, 60, &current)
            .expect("partial drift must still produce a patch");
        assert_eq!(patch["postgresql"]["parameters"]["track_commit_timestamp"], "on");
    }

    #[test]
    fn enabled_but_archive_timeout_mismatched_repatches() {
        // Operator raised POSTGRES_ARCHIVE_TIMEOUT; DCS still has the old value.
        let current = CurrentDcsArchiveParams {
            archive_timeout: Some(60),
            ..MATCHING
        };
        let patch = compute_archive_reconcile_patch(true, 120, &current)
            .expect("timeout mismatch must produce a patch");
        assert_eq!(patch["postgresql"]["parameters"]["archive_timeout"], 120);
    }

    #[test]
    fn disabled_and_fully_absent_is_noop() {
        assert!(compute_archive_reconcile_patch(false, 60, &EMPTY).is_none());
    }

    #[test]
    fn disabled_with_only_restore_command_stale_still_clears_it() {
        // Regression test for the exact gap this PR closes: before
        // restore_command was added to the absence check, a DCS state with
        // archive_mode/archive_command/archive_timeout already cleared (e.g.
        // by an older image's disable patch) but restore_command still set
        // would short-circuit to "already absent" and leave restore_command
        // stuck — every standby spamming a failing archive-get against creds
        // that are gone. It must NOT be treated as already-disabled.
        let current = CurrentDcsArchiveParams {
            restore_command: Some("/usr/local/bin/pgbackrest-archive-get-wrapper.sh %f %p"),
            ..EMPTY
        };
        let patch = compute_archive_reconcile_patch(false, 60, &current)
            .expect("restore_command-only drift must still produce a clearing patch");
        let params = &patch["postgresql"]["parameters"];
        assert!(params["restore_command"].is_null());
        assert!(params["archive_mode"].is_null());
        assert!(params["archive_command"].is_null());
        assert!(params["archive_timeout"].is_null());
    }

    #[test]
    fn disabled_with_stale_archive_mode_clears_all_four_including_restore_command() {
        // Even when restore_command specifically is already absent, any
        // other leftover archive param must still clear restore_command too
        // — the clearing patch is all-or-nothing, never partial.
        let current = CurrentDcsArchiveParams {
            archive_mode: Some(EXPECTED_ARCHIVE_MODE),
            ..EMPTY
        };
        let patch = compute_archive_reconcile_patch(false, 60, &current)
            .expect("stale archive_mode must produce a clearing patch");
        assert!(patch["postgresql"]["parameters"]["restore_command"].is_null());
    }

    #[test]
    fn disabled_leaves_track_commit_timestamp_out_of_the_patch() {
        // track_commit_timestamp is deliberately left in DCS on disable —
        // harmless without archiving, and clearing it would force yet
        // another PGC_POSTMASTER pending_restart for no operational benefit.
        let current = CurrentDcsArchiveParams {
            archive_mode: Some(EXPECTED_ARCHIVE_MODE),
            track_commit_timestamp: Some("on"),
            ..EMPTY
        };
        let patch = compute_archive_reconcile_patch(false, 60, &current).unwrap();
        assert!(patch["postgresql"]["parameters"]
            .get("track_commit_timestamp")
            .is_none());
    }

    const STALE_GUC: &str = "/usr/local/bin/pgbackrest-archive-get-wrapper.sh %f %p";

    #[test]
    fn clear_step_verified_on_empty_show_regardless_of_role() {
        assert_eq!(next_clear_step(Some(""), false), ClearStep::Verified);
        assert_eq!(next_clear_step(Some(""), true), ClearStep::Verified);
    }

    #[test]
    fn clear_step_alters_only_on_the_leader() {
        // Patroni never reconciles recovery params on a primary, so a stale
        // value there is ours to clear; the same value on a standby is
        // Patroni's to rewrite from the now-clean DCS — we only wait.
        assert_eq!(next_clear_step(Some(STALE_GUC), true), ClearStep::ClearOnPrimary);
        assert_eq!(next_clear_step(Some(STALE_GUC), false), ClearStep::AwaitPatroni);
    }

    #[test]
    fn clear_step_waits_for_postgres_when_show_unanswered() {
        // Regression guard on the election race: an unanswerable SHOW (still
        // starting, mid-clone) must poll again — never short-circuit to
        // Verified, which is how a one-shot boot-time check missed the
        // eventual leader when all nodes redeployed together.
        assert_eq!(next_clear_step(None, false), ClearStep::AwaitPostgres);
        assert_eq!(next_clear_step(None, true), ClearStep::AwaitPostgres);
    }
}
