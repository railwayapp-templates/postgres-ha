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
//!
//! Also asserts `lc_messages: "C"` in DCS, independent of archiving — the
//! self-heal WAL-corruption watch (`self_heal.rs`'s `wal_corruption`
//! submodule) matches fixed English Postgres log strings, so a non-C locale
//! silently disables that signal. `bootstrap.dcs` only sets it for clusters
//! bootstrapped after that change shipped; this reconcile is what brings the
//! existing fleet's DCS up to date on next restart. `lc_messages` is
//! `PGC_SUSET` (reload-only, unlike `archive_mode`), so no `pending_restart`
//! concern here. Only patched in when currently *absent* from DCS — an
//! operator who has explicitly set a different `lc_messages` is left alone,
//! at the cost of that cluster's corruption signal staying disabled.

use super::Config;
use anyhow::{Context, Result};
use common::{Telemetry, TelemetryEvent};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;
use tracing::{info, warn};

const PATRONI_REST: &str = "http://localhost:8008";
const EXPECTED_ARCHIVE_MODE: &str = "on";
const EXPECTED_ARCHIVE_COMMAND: &str = "/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p";
const EXPECTED_LC_MESSAGES: &str = "C";
// Bounded poll before concluding Patroni's own dynamic-config sync missed
// this node, rather than just being a loop_wait cycle behind a patch we
// ourselves may have just issued moments ago. Only SUCCESSFUL reads count
// toward a divergence verdict: the span they cover must comfortably exceed
// Patroni's loop_wait (10s, yaml.rs) — Patroni only applies a fresh DCS
// patch on its next HA-loop tick, so a shorter observation window would
// routinely misread ordinary application latency as a missed sync and
// force via ALTER SYSTEM when Patroni was about to apply it anyway.
// Failed reads (postgres still coming up) neither count nor reset the
// tally — if they counted, a node whose socket appears late would burn
// the whole budget on connection errors and end up deciding off a single
// read with no absorption window at all. MAX_ATTEMPTS bounds the whole
// poll so it hands control back to the caller's retry-with-backoff loop
// instead of spinning here indefinitely.
const LIVE_CHECK_MISMATCH_READS: u32 = 8;
const LIVE_CHECK_MAX_ATTEMPTS: u32 = 30;
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

/// True when this node is a recover-from fork whose staged PITR replay has
/// not yet been stamped complete — the window where the post-disable
/// `restore_command` clear must NOT run. During staged replay the effective
/// `restore_command` is the recovery-source invocation patroni-runner itself
/// appended to postgresql.auto.conf; the clear loop would read it as stale
/// and, on a node Patroni leader-labels while postgres is still replaying
/// (the lock is acquired before promote completes), `ALTER SYSTEM` it away
/// mid-replay — severing the replay's WAL source short of the recovery
/// target, since the GUC is reloadable. Even on a non-leader the loop would
/// spend the whole replay cycling observe-stale → deadline → `Err` → retry.
///
/// Nothing is lost by skipping: a fork that doesn't itself archive has no
/// yaml-rendered or DCS-seeded `restore_command` to go stale, Patroni
/// sanitizes the staged auto.conf value at promote (`_sanitize_auto_conf`),
/// and the boot after replay stamps `.pitr_configured` so the clear then
/// verifies normally — instantly, on an already-empty GUC.
fn pitr_replay_pending(recover_from_configured: bool, replay_done_marker_present: bool) -> bool {
    recover_from_configured && !replay_done_marker_present
}

/// Live `archive_mode` / `archive_command` / `archive_timeout` GUC values
/// read directly from the running Postgres, bypassing Patroni and DCS
/// entirely. `archive_mode` is read alongside the other two specifically so
/// callers can tell a genuine divergence apart from Postgres's own
/// `archive_command` masking (see [`query_live_archive_gucs`]).
struct LiveArchiveGucs {
    archive_mode: String,
    archive_command: String,
    archive_timeout_secs: i64,
}

/// Query the live `archive_mode` / `archive_command` / `archive_timeout`
/// GUCs over the unix socket. `Err` here means "couldn't ask" (postgres not
/// up yet, socket not ready) — distinct from a successful query that reads
/// back an empty/wrong value, which is the actual defect this module exists
/// to catch.
///
/// `archive_mode` is read too, not just for its own sake: Postgres's
/// `show_archive_command()` GUC hook masks `archive_command` as the literal
/// string `"(disabled)"` — via `SHOW`, `current_setting()`, and
/// `pg_settings.setting` alike — whenever `archive_mode` isn't active.
/// `archive_mode` is `PGC_POSTMASTER` (restart-only; see this module's
/// header), so a node can easily be mid-`pending_restart` with DCS already
/// correct — a normal, expected state, not a defect. Without reading
/// `archive_mode` too, that masked `"(disabled)"` reading would be
/// indistinguishable from a real, concrete operator-set value and get
/// misreported as [`TelemetryEvent::ArchiveConfigDrifted`] on every ordinary
/// enable-PITR-on-an-existing-cluster boot.
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
            // with no suffix. archive_mode itself is never masked (only
            // archive_command is), so current_setting() is fine for it.
            "SELECT current_setting('archive_mode'), current_setting('archive_command'), \
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
    // trim_end_matches (not trim()) — the archive_command column is empty
    // exactly when this function's caller most needs a correct answer (a
    // broken archive_command), and generic trim() strips a leading tab
    // separator along with it, silently shifting a later column into an
    // earlier field and reporting "missing" instead of "empty".
    let mut fields = stdout.trim_end_matches('\n').splitn(3, '\t');
    let archive_mode = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing archive_mode field in psql output"))?
        .to_string();
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
        archive_mode,
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

/// Undo [`force_live_archive_gucs`]'s `postgresql.auto.conf` entries via
/// `ALTER SYSTEM RESET` + `pg_reload_conf()`. Without this, a node that
/// ever self-healed through the force path keeps archiving forever after
/// PITR is disabled: the disable flow only clears DCS, but `auto.conf`
/// outranks Patroni's rendered `postgresql.conf`, so the pinned
/// `archive_command` keeps firing with the S3 creds gone — the exact
/// silent-degradation failure mode this module's header describes.
/// Runs fine on standbys too — `ALTER SYSTEM` writes `auto.conf`
/// without WAL. Requires a live Postgres; callers gate on
/// [`archive_gucs_pinned_in_auto_conf`] so that requirement only
/// applies to nodes that actually carry a pin.
async fn reset_live_archive_gucs(superuser: &str) -> Result<()> {
    // Separate -c flags for the same reason as force_live_archive_gucs:
    // ALTER SYSTEM cannot run inside psql's implicit multi-statement
    // transaction block.
    let out = Command::new("psql")
        .args([
            "-U",
            superuser,
            "-h",
            "/var/run/postgresql",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            "ALTER SYSTEM RESET archive_command;",
            "-c",
            "ALTER SYSTEM RESET archive_timeout;",
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
            "failed to reset live archive GUCs: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    Ok(())
}

/// True when `postgresql.auto.conf` carries an archive GUC pin that
/// [`reset_live_archive_gucs`] would need to clear. Read from disk, not
/// via SQL, so the answer is available even while Postgres is down —
/// which matters on the disable path: it runs on every node startup,
/// including mid-clone replicas and nodes whose Postgres never comes up
/// this boot, and demanding a live Postgres just to learn "there was
/// never a pin" would leave the reconcile retry loop spinning (and
/// WARN-logging) for the whole window on the overwhelmingly common
/// pin-free node. When a pin IS found, the psql-based reset genuinely
/// must wait for Postgres — the caller's retry loop is that wait.
/// An absent file cannot carry a pin: `Ok(false)`. Any OTHER read failure
/// (permissions, I/O error) propagates as `Err` rather than silently
/// reading as "no pin" — on the disable path in particular, misreading an
/// unreadable-but-present `auto.conf` as pin-free would skip
/// [`reset_live_archive_gucs`] entirely and leave a stale `archive_command`
/// pin (pointed at now-gone S3 creds) armed, with no retry and no signal
/// that anything was skipped.
async fn archive_gucs_pinned_in_auto_conf(data_dir: &str) -> Result<bool> {
    let auto_conf = format!("{data_dir}/postgresql.auto.conf");
    match tokio::fs::read_to_string(&auto_conf).await {
        Ok(contents) => Ok(contents.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with("archive_command") || line.starts_with("archive_timeout")
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("read {auto_conf}")),
    }
}

/// Marker file recording that [`force_live_archive_gucs`] wrote the
/// `postgresql.auto.conf` pin on this volume. This is what lets a later
/// boot tell OUR pin apart from an operator's own `ALTER SYSTEM` edit —
/// inside auto.conf the two are indistinguishable, and only ours is fair
/// game to clear. Kept in the data dir deliberately: pg_basebackup copies
/// `postgresql.auto.conf` into fresh clones, so a clone taken from a
/// force-healed leader inherits the pin — keeping the sentinel beside it
/// means the pair travels (and gets cleaned up) together.
const FORCED_ARCHIVE_GUCS_SENTINEL: &str = ".railway_forced_archive_gucs";

fn forced_sentinel_path(data_dir: &str) -> String {
    format!("{data_dir}/{FORCED_ARCHIVE_GUCS_SENTINEL}")
}

/// True when a previous boot's [`force_live_archive_gucs`] marked the
/// auto.conf pin as ours.
async fn archive_gucs_forced_by_us(data_dir: &str) -> bool {
    tokio::fs::metadata(forced_sentinel_path(data_dir))
        .await
        .is_ok()
}

/// Record that the auto.conf pin about to be written is ours. Written
/// BEFORE the ALTER SYSTEM, not after: if this process dies between the
/// two, a sentinel without a pin is a harmless no-op on the next boot,
/// while a pin without a sentinel would read as an operator edit and
/// shadow env-driven config changes forever — the exact stale-pin drift
/// this marker exists to prevent.
async fn mark_archive_gucs_forced(data_dir: &str) -> Result<()> {
    let path = forced_sentinel_path(data_dir);
    tokio::fs::write(&path, b"")
        .await
        .with_context(|| format!("write forced-archive-GUCs sentinel {path}"))
}

/// Best-effort sentinel removal — a sentinel that outlives its pin only
/// costs a redundant (idempotent) `ALTER SYSTEM RESET` on a later boot,
/// so failure here is logged, not propagated.
async fn clear_forced_sentinel(data_dir: &str) {
    let path = forced_sentinel_path(data_dir);
    if let Err(e) = tokio::fs::remove_file(&path).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(error = %e, path, "failed to remove forced-archive-GUCs sentinel");
        }
    }
}

/// True when every FIELD that doesn't already match `expected_*` sits at
/// Postgres's own untouched baseline — `archive_command` empty,
/// `archive_timeout` `0` — rather than some other concrete value. Checked
/// per field, not as a joint "both blank" condition: the apply-race in
/// [`verify_and_heal_live_archive_config`]'s doc has only ever been
/// confirmed leaving `archive_command` blank while `archive_timeout` was
/// already correct (set by an earlier config pass, or never touched by
/// this specific race) — requiring *both* fields to be simultaneously
/// blank would misclassify that exact, documented case as "maybe
/// intentional" and leave it unfixed. Nobody chooses these baseline
/// values on purpose (an intentional `archive_timeout` edit is never
/// literally "off", and a hand-set `archive_command` is never the empty
/// string), so any mismatching field that ISN'T at this baseline is a
/// real, distinct value a human could plausibly have set —
/// [`verify_and_heal_live_archive_config`] only auto-corrects when every
/// mismatching field clears this bar, and reports otherwise.
fn looks_like_never_applied(live: &LiveArchiveGucs, expected_archive_timeout: i64) -> bool {
    let command_ok =
        live.archive_command == EXPECTED_ARCHIVE_COMMAND || live.archive_command.is_empty();
    let timeout_ok =
        live.archive_timeout_secs == expected_archive_timeout || live.archive_timeout_secs == 0;
    command_ok && timeout_ok
}

/// Verify the live Postgres GUCs actually match `expected_archive_timeout`,
/// self-healing the specific case Patroni's own sync is known to produce.
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
///
/// Only [`looks_like_never_applied`] divergence is auto-corrected. Any
/// other live value is reported via [`TelemetryEvent::ArchiveConfigDrifted`]
/// and left alone — it's indistinguishable from an operator's own
/// `ALTER SYSTEM` edit, and this module has no business silently
/// overwriting something a human may have set on purpose.
async fn verify_and_heal_live_archive_config(
    config: &Config,
    telemetry: &Telemetry,
    expected_archive_timeout: i64,
) -> Result<()> {
    let live = match poll_live_archive_gucs(
        || query_live_archive_gucs(&config.superuser),
        expected_archive_timeout,
        LIVE_CHECK_INTERVAL,
    )
    .await
    .context("verify live archive config")?
    {
        LivePoll::Applied => {
            info!("live archive_command/archive_timeout confirmed applied");
            return Ok(());
        }
        LivePoll::RestartPending => {
            info!(
                "live archive_mode isn't \"on\" yet — Patroni has a pending restart for this \
                 DCS change (archive_mode is restart-only); archive_command's live value can't \
                 be trusted until then, so skipping verification for this boot"
            );
            return Ok(());
        }
        LivePoll::Diverged(live) => live,
    };

    if looks_like_never_applied(&live, expected_archive_timeout) {
        warn!(
            live_archive_command = %live.archive_command,
            live_archive_timeout = live.archive_timeout_secs,
            "live archive config diverged from DCS despite DCS being correct — \
             Patroni's dynamic-config sync silently missed this node; forcing directly"
        );
        mark_archive_gucs_forced(&config.data_dir).await?;
        force_live_archive_gucs(&config.superuser, expected_archive_timeout).await?;
        info!(
            "live archive_command/archive_timeout corrected via ALTER SYSTEM + pg_reload_conf()"
        );
        telemetry.send(TelemetryEvent::ArchiveConfigForced {
            node: config.name.clone(),
            live_archive_command: live.archive_command,
            live_archive_timeout_secs: live.archive_timeout_secs,
        });
        return Ok(());
    }

    warn!(
        live_archive_command = %live.archive_command,
        live_archive_timeout = live.archive_timeout_secs,
        "live archive config diverged from DCS but doesn't look like the known apply-race \
         (not the untouched baseline) — reporting, not overwriting"
    );
    telemetry.send(TelemetryEvent::ArchiveConfigDrifted {
        node: config.name.clone(),
        live_archive_command_matches_expected: live.archive_command == EXPECTED_ARCHIVE_COMMAND,
        live_archive_timeout_secs: live.archive_timeout_secs,
        expected_archive_timeout_secs: expected_archive_timeout,
    });
    Ok(())
}

/// Verdict from [`poll_live_archive_gucs`]: the live values either matched
/// the expectation (`Applied`), `archive_mode` isn't live-`"on"` yet so
/// `archive_command`'s reading can't be trusted at all (`RestartPending`),
/// or the values persistently mismatched across the whole observation
/// window despite `archive_mode` being on (`Diverged`). "Couldn't read
/// enough to tell" is not a verdict — it propagates as `Err` instead.
enum LivePoll {
    Applied,
    RestartPending,
    Diverged(LiveArchiveGucs),
}

/// Drive the bounded live-GUC poll. Generic over the read so the
/// only-successful-reads-count policy is unit-testable without a live
/// Postgres; production passes [`query_live_archive_gucs`].
///
/// A read showing `archive_mode` isn't `"on"` returns `RestartPending`
/// immediately, with no further polling: `archive_mode` is
/// `PGC_POSTMASTER`, so it cannot flip on this same running Postgres
/// process without a full restart — unlike `archive_command`'s apply race,
/// there's no "about to catch up" window to wait out, and Postgres masks
/// `archive_command` as `"(disabled)"` in this state anyway (see
/// [`query_live_archive_gucs`]), so further reads would tell us nothing new.
///
/// Otherwise, a single matching read returns `Applied` immediately.
/// `Diverged` needs [`LIVE_CHECK_MISMATCH_READS`] successful reads all
/// showing a mismatch — an interleaved read error neither counts toward
/// nor resets that tally (a connection blip says nothing about whether
/// Patroni applied the config, and the per-attempt sleeps still guarantee
/// the tally spans the intended observation window). If
/// [`LIVE_CHECK_MAX_ATTEMPTS`] runs out before either verdict, the last
/// read error propagates: deciding "diverged" off fewer reads would
/// re-open the exact no-absorption-window hole this poll exists to close,
/// and the caller's outer retry loop re-runs the whole idempotent pass
/// anyway.
async fn poll_live_archive_gucs<F, Fut>(
    mut read_live: F,
    expected_archive_timeout: i64,
    interval: Duration,
) -> Result<LivePoll>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<LiveArchiveGucs>>,
{
    let mut mismatch_reads = 0u32;
    let mut last_err = None;
    for attempt in 1..=LIVE_CHECK_MAX_ATTEMPTS {
        match read_live().await {
            Ok(live) if live.archive_mode != EXPECTED_ARCHIVE_MODE => {
                return Ok(LivePoll::RestartPending);
            }
            Ok(live)
                if live.archive_command == EXPECTED_ARCHIVE_COMMAND
                    && live.archive_timeout_secs == expected_archive_timeout =>
            {
                return Ok(LivePoll::Applied);
            }
            Ok(live) => {
                mismatch_reads += 1;
                if mismatch_reads >= LIVE_CHECK_MISMATCH_READS {
                    return Ok(LivePoll::Diverged(live));
                }
            }
            Err(e) => last_err = Some(e),
        }
        if attempt < LIVE_CHECK_MAX_ATTEMPTS {
            tokio::time::sleep(interval).await;
        }
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("live archive GUC poll exhausted its attempt budget")))
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
///   archive_command after disable, then clear a stale `restore_command`
///   GUC on the primary and verify it stuck (see
///   `ensure_restore_command_cleared` for why the order matters and why a
///   one-shot clear isn't enough). The clear is skipped while a staged PITR
///   replay is pending on a recover-from fork — the staged value IS the
///   replay's WAL source (see `pitr_replay_pending`). An auto.conf pin left
///   by a previous boot's `force_live_archive_gucs` is also reset, per node
///   and gated on our sentinel (see the disable branch).
/// - Idempotent: no patch issued when DCS already matches intent. Both
///   live-GUC follow-ups (the enable-path verify and the disable-path
///   clears) still run on a no-op patch: DCS is cluster-wide state settled
///   by whichever node reconciles first, while the live GUCs and auto.conf
///   are per-node — e.g. a leader elected after the disable patch already
///   landed can carry a stale `restore_command` with nothing left to
///   reconcile in DCS.
/// - Independent of archiving: also assert `lc_messages="C"` in DCS whenever
///   it is currently absent there (never overwrites an operator-set value).
///   See the module doc.
///
/// The DCS decision itself lives in `compute_archive_reconcile_patch`; this
/// function does the GET/PATCH I/O around it plus the live-GUC follow-ups.
pub async fn reconcile_pgbackrest_archive_config(
    config: &Config,
    telemetry: &Telemetry,
) -> Result<()> {
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

    let lc_messages = params
        .and_then(|m| m.get("lc_messages"))
        .and_then(|v| v.as_str());

    // Independent of archive_mode/enabled: only ever patches in when DCS has
    // no lc_messages at all, so an operator's explicit non-C choice is never
    // clobbered (that cluster's corruption signal just stays disabled).
    match lc_messages {
        None => {
            warn!(
                "DCS has no lc_messages set — pinning to \"C\" so the WAL-corruption self-heal signal (which matches fixed English log strings) can work on this cluster"
            );
            let lc_patch = json!({
                "postgresql": {
                    "parameters": {
                        "lc_messages": EXPECTED_LC_MESSAGES,
                    }
                }
            });
            send_patch(&client, &lc_patch).await?;
            info!("DCS lc_messages pinned to \"C\"");
        }
        Some(EXPECTED_LC_MESSAGES) => {
            info!("DCS lc_messages already \"C\"");
        }
        Some(other) => {
            info!(
                current_lc_messages = other,
                "DCS lc_messages set to a non-default value — leaving the operator's choice alone (WAL-corruption self-heal signal stays disabled on this cluster)"
            );
        }
    }

    if enabled {
        // A pin left by a previous boot's force is ours to manage: clear it
        // and let Patroni's rendered postgresql.conf speak before judging
        // the live values. In the common case (the apply-race didn't recur
        // this boot) the node returns to fully Patroni-managed config, so
        // an env-driven archive_timeout change propagates instead of being
        // shadowed by a stale pin carrying the OLD value — which the verify
        // below would misread as possibly-intentional operator drift and
        // refuse to touch. If the race DID recur, the verify re-forces and
        // re-marks. Gated on the sentinel so an operator's own ALTER SYSTEM
        // pin is never cleared while PITR is enabled.
        if archive_gucs_forced_by_us(&config.data_dir).await {
            if archive_gucs_pinned_in_auto_conf(&config.data_dir).await? {
                reset_live_archive_gucs(&config.superuser).await?;
                info!(
                    "cleared our previous ALTER SYSTEM archive-GUC pin — \
                     re-verifying against Patroni's own config"
                );
            }
            clear_forced_sentinel(&config.data_dir).await;
        }

        verify_and_heal_live_archive_config(config, telemetry, expected_archive_timeout).await?;
    } else {
        // Per-node auto.conf pin cleanup: DCS is cluster-wide state,
        // cleared once by whichever node reconciles first, but the
        // ALTER SYSTEM pin from force_live_archive_gucs lives in each
        // node's own postgresql.auto.conf — every node must clear its own
        // regardless of who won the DCS race. Gated on the sentinel, same
        // as the enable path: our pin never exists without one (written
        // before the force; pg_basebackup/pgBackRest copy the pair into
        // clones and backups together), so a pin without a sentinel is an
        // operator's own ALTER SYSTEM edit — theirs to keep even with PITR
        // off, and this branch runs on every boot of every cluster that
        // never enabled PITR at all.
        if archive_gucs_forced_by_us(&config.data_dir).await {
            if archive_gucs_pinned_in_auto_conf(&config.data_dir).await? {
                reset_live_archive_gucs(&config.superuser).await?;
            }
            // After the reset (not before): a failed reset propagates above
            // and keeps the sentinel for the retry. Cleared even when no
            // pin was found — an operator may have RESET the GUCs by hand,
            // and a stale sentinel would cause a pointless reset on a later
            // re-enable.
            clear_forced_sentinel(&config.data_dir).await;
        } else if archive_gucs_pinned_in_auto_conf(&config.data_dir).await? {
            warn!(
                "archive GUCs pinned in postgresql.auto.conf without our sentinel — \
                 operator-set, leaving in place (PITR disabled)"
            );
        }

        // `.pitr_configured` is stamped by the restore gate on the boot
        // after a staged replay completes (patroni_runner.rs owns the
        // marker lifecycle).
        let replay_done =
            Path::new(&format!("{}/.pitr_configured", config.data_dir)).exists();
        if pitr_replay_pending(config.wal_recover_from_bucket.is_some(), replay_done) {
            info!(
                "reconcile: staged PITR replay still pending — skipping the restore_command clear; the staged value is the replay's WAL source and Patroni sanitizes it at promote"
            );
        } else {
            // Strictly after the DCS patch: a GUC clear issued before it
            // loses the race with Patroni's next config write, which
            // sanitizes the auto.conf override away while postgresql.conf
            // still renders the stale line (see
            // `ensure_restore_command_cleared`).
            ensure_restore_command_cleared(&client).await?;
        }
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

    #[test]
    fn restore_clear_deferred_only_during_pending_fork_replay() {
        // The clear must stand down exactly while a recover-from fork's
        // staged replay is unstamped: the staged restore_command is the
        // replay's WAL source, and an ALTER on a leader-labeled
        // still-replaying node would sever it short of the target. A
        // stamped replay, or any node with no recover-from source at all
        // (the whole non-fork fleet), clears normally.
        assert!(pitr_replay_pending(true, false));
        assert!(!pitr_replay_pending(true, true));
        assert!(!pitr_replay_pending(false, false));
        assert!(!pitr_replay_pending(false, true));
    }

    fn gucs(archive_command: &str, archive_timeout_secs: i64) -> LiveArchiveGucs {
        gucs_with_mode(EXPECTED_ARCHIVE_MODE, archive_command, archive_timeout_secs)
    }

    fn gucs_with_mode(
        archive_mode: &str,
        archive_command: &str,
        archive_timeout_secs: i64,
    ) -> LiveArchiveGucs {
        LiveArchiveGucs {
            archive_mode: archive_mode.to_string(),
            archive_command: archive_command.to_string(),
            archive_timeout_secs,
        }
    }

    #[test]
    fn both_fields_blank_is_recognized() {
        assert!(looks_like_never_applied(&gucs("", 0), 60));
    }

    #[test]
    fn command_blank_timeout_already_correct_is_recognized() {
        // The actual documented/confirmed-in-practice shape of the race:
        // only archive_command went unapplied. Regression pin — an
        // earlier version of this check required BOTH fields blank and
        // misclassified exactly this as "maybe intentional", leaving a
        // genuinely broken node unfixed (caught by
        // t_ha_archive_config_live_reconcile_heals_after_restart).
        assert!(looks_like_never_applied(&gucs("", 60), 60));
    }

    #[test]
    fn timeout_blank_command_already_correct_is_recognized() {
        // Mirror of the above: archive_command already landed, only
        // archive_timeout is sitting at the untouched baseline.
        assert!(looks_like_never_applied(
            &gucs(EXPECTED_ARCHIVE_COMMAND, 0),
            60
        ));
    }

    #[test]
    fn nonempty_nonexpected_command_is_not_never_applied() {
        // A concrete, non-blank archive_command someone could have set on
        // purpose — even with the timeout at its blank baseline.
        assert!(!looks_like_never_applied(&gucs("/bin/true", 0), 60));
    }

    #[test]
    fn nonzero_nonexpected_timeout_is_not_never_applied() {
        // A concrete, non-blank, non-expected archive_timeout — even with
        // the command already correct.
        assert!(!looks_like_never_applied(
            &gucs(EXPECTED_ARCHIVE_COMMAND, 120),
            60
        ));
    }

    #[test]
    fn fully_matching_value_is_never_applied_vacuously() {
        // Not reached in practice — the caller's own early-return catches
        // a fully-matching pair before this function runs — but the
        // function's contract ("every mismatching field is at baseline")
        // holds vacuously when nothing mismatches.
        assert!(looks_like_never_applied(
            &gucs(EXPECTED_ARCHIVE_COMMAND, 60),
            60
        ));
    }

    /// Scripted stand-in for [`query_live_archive_gucs`]. Panics if the
    /// poll reads past the provided sequence — tests use that to pin
    /// exactly how many reads a verdict takes.
    fn scripted(
        outcomes: Vec<Result<LiveArchiveGucs>>,
    ) -> impl FnMut() -> std::future::Ready<Result<LiveArchiveGucs>> {
        let mut seq = outcomes.into_iter();
        move || std::future::ready(seq.next().expect("poll read past the scripted sequence"))
    }

    fn read_err() -> Result<LiveArchiveGucs> {
        Err(anyhow::anyhow!("socket not ready"))
    }

    #[tokio::test]
    async fn poll_applied_on_first_matching_read() {
        // scripted() panics on a second read, so this also pins that a
        // match short-circuits immediately.
        let verdict = poll_live_archive_gucs(
            scripted(vec![Ok(gucs(EXPECTED_ARCHIVE_COMMAND, 60))]),
            60,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert!(matches!(verdict, LivePoll::Applied));
    }

    #[tokio::test]
    async fn poll_diverges_after_exactly_the_required_mismatch_reads() {
        let outcomes = (0..LIVE_CHECK_MISMATCH_READS)
            .map(|_| Ok(gucs("", 60)))
            .collect();
        let verdict = poll_live_archive_gucs(scripted(outcomes), 60, Duration::ZERO)
            .await
            .unwrap();
        match verdict {
            LivePoll::Diverged(live) => assert_eq!(live.archive_command, ""),
            LivePoll::Applied => panic!("persistent mismatch must not read as Applied"),
            LivePoll::RestartPending => panic!("archive_mode is \"on\" in this scenario"),
        }
    }

    #[tokio::test]
    async fn poll_read_errors_dont_count_toward_divergence() {
        // Postgres unreachable for a while, then the live value reads
        // correct: the connection errors must not have accumulated into a
        // divergence verdict.
        let mut outcomes: Vec<Result<LiveArchiveGucs>> = (0..10).map(|_| read_err()).collect();
        outcomes.push(Ok(gucs(EXPECTED_ARCHIVE_COMMAND, 60)));
        let verdict = poll_live_archive_gucs(scripted(outcomes), 60, Duration::ZERO)
            .await
            .unwrap();
        assert!(matches!(verdict, LivePoll::Applied));
    }

    #[tokio::test]
    async fn poll_read_errors_dont_reset_the_mismatch_tally() {
        // Alternating blip/mismatch: the tally must survive the blips and
        // still reach a Diverged verdict within the attempt budget.
        let mut outcomes = Vec::new();
        for _ in 0..LIVE_CHECK_MISMATCH_READS {
            outcomes.push(read_err());
            outcomes.push(Ok(gucs("", 0)));
        }
        let verdict = poll_live_archive_gucs(scripted(outcomes), 60, Duration::ZERO)
            .await
            .unwrap();
        assert!(matches!(verdict, LivePoll::Diverged(_)));
    }

    #[tokio::test]
    async fn poll_single_late_mismatch_read_is_no_verdict() {
        // The no-absorption-window regression pin: connection errors eat
        // all but the last attempt, whose lone mismatched read must NOT
        // decide (Patroni may have been about to apply a just-issued
        // patch) — the poll errors back to the outer retry loop instead.
        let mut outcomes: Vec<Result<LiveArchiveGucs>> =
            (1..LIVE_CHECK_MAX_ATTEMPTS).map(|_| read_err()).collect();
        outcomes.push(Ok(gucs("", 0)));
        assert!(
            poll_live_archive_gucs(scripted(outcomes), 60, Duration::ZERO)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn poll_all_errors_propagates_an_error() {
        let outcomes = (0..LIVE_CHECK_MAX_ATTEMPTS).map(|_| read_err()).collect();
        assert!(
            poll_live_archive_gucs(scripted(outcomes), 60, Duration::ZERO)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn poll_match_mid_window_wins_over_earlier_mismatches() {
        // Patroni applies the config partway through the observation
        // window (the ordinary just-patched-DCS case): Applied, no force.
        let mut outcomes: Vec<Result<LiveArchiveGucs>> =
            (0..3).map(|_| Ok(gucs("", 60))).collect();
        outcomes.push(Ok(gucs(EXPECTED_ARCHIVE_COMMAND, 60)));
        let verdict = poll_live_archive_gucs(scripted(outcomes), 60, Duration::ZERO)
            .await
            .unwrap();
        assert!(matches!(verdict, LivePoll::Applied));
    }

    #[tokio::test]
    async fn poll_returns_restart_pending_when_archive_mode_off_and_stops_polling() {
        // archive_mode is PGC_POSTMASTER — it cannot change on a running
        // Postgres process without a full restart, so a single "off" read
        // is a stable, structural signal for the rest of this boot, not a
        // race to poll through. scripted() panics on a second read, pinning
        // that this returns immediately instead of accumulating toward a
        // Diverged verdict.
        let verdict = poll_live_archive_gucs(
            scripted(vec![Ok(gucs_with_mode("off", "", 0))]),
            60,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert!(matches!(verdict, LivePoll::RestartPending));
    }

    #[tokio::test]
    async fn poll_treats_masked_disabled_command_as_restart_pending_not_diverged() {
        // The exact false positive this fix closes: Postgres masks
        // archive_command as the literal "(disabled)" via current_setting()
        // whenever archive_mode is off (e.g. a Patroni-deferred restart
        // hasn't happened yet). Before reading archive_mode too, that
        // string was neither the empty baseline nor the expected command,
        // so a completely normal "restart not yet applied" boot would
        // misclassify as ArchiveConfigDrifted (operator edit).
        let verdict = poll_live_archive_gucs(
            scripted(vec![Ok(gucs_with_mode("off", "(disabled)", 60))]),
            60,
            Duration::ZERO,
        )
        .await
        .unwrap();
        assert!(matches!(verdict, LivePoll::RestartPending));
    }

    #[tokio::test]
    async fn forced_sentinel_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        assert!(!archive_gucs_forced_by_us(data_dir).await);
        mark_archive_gucs_forced(data_dir).await.unwrap();
        assert!(archive_gucs_forced_by_us(data_dir).await);
        clear_forced_sentinel(data_dir).await;
        assert!(!archive_gucs_forced_by_us(data_dir).await);
        // Clearing an already-absent sentinel is a quiet no-op.
        clear_forced_sentinel(data_dir).await;
    }

    #[tokio::test]
    async fn pinned_in_auto_conf_is_false_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        assert!(!archive_gucs_pinned_in_auto_conf(data_dir).await.unwrap());
    }

    #[tokio::test]
    async fn pinned_in_auto_conf_detects_an_archive_line() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        tokio::fs::write(
            format!("{data_dir}/postgresql.auto.conf"),
            "some_other_param = '1'\narchive_command = '/bin/true'\n",
        )
        .await
        .unwrap();
        assert!(archive_gucs_pinned_in_auto_conf(data_dir).await.unwrap());
    }

    #[tokio::test]
    async fn pinned_in_auto_conf_is_false_with_no_archive_lines() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        tokio::fs::write(
            format!("{data_dir}/postgresql.auto.conf"),
            "some_other_param = '1'\n",
        )
        .await
        .unwrap();
        assert!(!archive_gucs_pinned_in_auto_conf(data_dir).await.unwrap());
    }

    #[tokio::test]
    async fn pinned_in_auto_conf_propagates_non_notfound_errors() {
        // A directory can't be read_to_string'd as a file — this stands in
        // for "unreadable for a reason other than absence", which must
        // propagate as Err rather than silently reading as "no pin".
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        tokio::fs::create_dir(format!("{data_dir}/postgresql.auto.conf"))
            .await
            .unwrap();
        assert!(archive_gucs_pinned_in_auto_conf(data_dir).await.is_err());
    }
}
