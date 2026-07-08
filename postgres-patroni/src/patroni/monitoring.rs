//! Patroni process monitoring
//!
//! Handles the monitoring loop, signal handling, and health check management.

use super::{check_health, self_heal, Config};
use common::{Telemetry, TelemetryEvent};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::path::Path;
use std::time::Duration;
use tokio::process::Child;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::sleep;
use tracing::{error, info, warn};

/// Seconds of no startup progress before we begin asking the leader whether
/// the WAL the replica needs still exists. Below this we assume a normal
/// clone/catch-up is in flight and don't probe.
const WAL_PROBE_GRACE_SECS: u64 = 30;
/// Upper bound on the (exponentially backing-off) gap between WAL-availability
/// probes, so a long stall keeps the query rate on the leader bounded.
const WAL_PROBE_MAX_INTERVAL_SECS: u64 = 240;

/// Run the main monitoring loop for Patroni
///
/// This function handles:
/// - Startup grace period waiting
/// - Continuous health checking
/// - Signal handling (SIGTERM/SIGINT)
/// - Process death detection
pub async fn run_monitoring_loop(
    config: &Config,
    mut child: Child,
    telemetry: &Telemetry,
) -> anyhow::Result<()> {
    let patroni_pid = child
        .id()
        .ok_or_else(|| anyhow::anyhow!("Failed to get Patroni PID"))?;
    info!(pid = patroni_pid, "Patroni started");

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    // Wait for Patroni to initialize
    // We wait up to max_startup_timeout for Patroni to become healthy.
    // If it doesn't become healthy within that time, we exit(1) to trigger
    // container restart and recovery.
    info!(
        grace_period = config.startup_grace_period,
        max_timeout = config.max_startup_timeout,
        "Waiting for Patroni to initialize"
    );

    // `startup_elapsed` counts time WITHOUT progress, not wall-clock. A
    // pg_basebackup clone (or initdb) of a large primary keeps Patroni
    // "unhealthy" far longer than max_startup_timeout — a wall-clock kill would
    // SIGKILL the clone mid-stream, and since pg_basebackup writes
    // global/pg_control LAST that leaves a non-empty pgdata with no control
    // file, which Patroni then refuses to start ("system ID is invalid"),
    // wedging the replica permanently. So we only accrue toward the recovery
    // exit while the node is making NO progress, where progress is either of
    // two independent signals:
    //   1. Volume usage growing — pg_basebackup laying data files down. This is
    //      the only signal available before Patroni's REST API answers.
    //   2. WAL position (received/replayed LSN) advancing — covers the
    //      post-basebackup catch-up phase, where the replica streams + replays
    //      WAL with disk usage ~flat (segments recycled about as fast as they
    //      arrive) yet the LSN keeps climbing. Volume bytes alone would read
    //      that as "stalled" and wrongly kill a healthy catch-up.
    // A genuinely stalled startup — and a hung clone (dead replication socket) —
    // advances NEITHER signal, so it still exits for recovery after the timeout.
    let mut startup_elapsed = 0u64;
    let mut last_volume_used = volume_used_bytes(&config.data_dir);
    let mut last_xlog_pos: Option<i64> = None;
    // One forced reinitialize per container start. A replica that stalls before
    // ever becoming healthy is usually wedged in a way a restart can't fix — see
    // try_reinitialize_stalled_replica. We try a reinitialize once instead of
    // restarting into the same wall; if it doesn't take, the next boot re-decides
    // against the persistent per-hour cap rather than re-wiping a half-laid clone.
    let mut reinit_attempted = false;
    // Backoff schedule for the WAL-availability probe (see the
    // `Waiting` branch). The WAL-too-old fact is already true at the first
    // probe, so we front-load: probe at the grace mark, then double the gap
    // each time. A node stalled for some *other* reason (WAL still present)
    // is probed only a few times before the `max_startup_timeout` exit takes
    // over — so we never hammer an already-busy leader with a query every 30s
    // for the full timeout. Both reset whenever `startup_elapsed` does (a new
    // stall episode earns fresh, prompt detection).
    let mut wal_probe_interval = WAL_PROBE_GRACE_SECS;
    let mut next_wal_probe_at = WAL_PROBE_GRACE_SECS;
    // Short-timeout client for polling Patroni's local REST API for WAL
    // progress. Best-effort: if it can't be built we fall back to the
    // volume-usage signal alone.
    let patroni_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok();
    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM during startup");
                let _ = kill(Pid::from_raw(patroni_pid as i32), Signal::SIGTERM);
                let _ = child.wait().await;
                return Ok(());
            }
            _ = sigint.recv() => {
                info!("Received SIGINT during startup");
                let _ = kill(Pid::from_raw(patroni_pid as i32), Signal::SIGTERM);
                let _ = child.wait().await;
                return Ok(());
            }
            status = child.wait() => {
                error!("Patroni died during startup");
                telemetry.send(TelemetryEvent::ProcessDied {
                    node: config.name.clone(),
                    process: "patroni".to_string(),
                    exit_code: status.ok().and_then(|s| s.code()),
                });
                std::process::exit(1);
            }
            _ = sleep(Duration::from_secs(5)) => {
                let healthy = check_health(config.health_check_timeout).await;

                let used = volume_used_bytes(&config.data_dir);
                let volume_grew = used > last_volume_used;
                last_volume_used = used;

                // WAL position is the progress signal whole-volume bytes miss:
                // during catch-up the replica streams + replays WAL while disk
                // usage stays ~flat, but received/replayed LSN keeps advancing.
                // Absent during pg_basebackup (no xlog yet) — that phase is
                // covered by volume growth instead.
                let xlog_pos = match patroni_client.as_ref() {
                    Some(c) => fetch_patroni_xlog_position(c).await,
                    None => None,
                };
                let lsn_advanced = xlog_advanced(last_xlog_pos, xlog_pos);
                if xlog_pos.is_some() {
                    last_xlog_pos = xlog_pos;
                }

                let progressing = volume_grew || lsn_advanced;

                match classify_startup_tick(
                    healthy,
                    progressing,
                    startup_elapsed,
                    config.max_startup_timeout,
                ) {
                    StartupTick::Healthy => {
                        info!(elapsed_without_progress = startup_elapsed, "Patroni healthy, starting monitoring");
                        break;
                    }
                    StartupTick::Progressing => {
                        // Clone/initdb/catch-up advancing — never count it toward the kill.
                        if startup_elapsed > 0 {
                            info!(
                                volume_used_bytes = used,
                                volume_grew,
                                xlog_position = xlog_pos.unwrap_or(0),
                                lsn_advanced,
                                "Startup making progress (clone bytes landing or WAL replay advancing); resetting startup timeout"
                            );
                        }
                        startup_elapsed = 0;
                        // New stall episode (if one follows) earns a fresh,
                        // prompt probe schedule rather than the backed-off gap
                        // left over from the previous stall.
                        wal_probe_interval = WAL_PROBE_GRACE_SECS;
                        next_wal_probe_at = WAL_PROBE_GRACE_SECS;
                    }
                    StartupTick::Stalled => {
                        // No progress for the full stall timeout. We do NOT wipe
                        // pgdata on a stall we can't explain — a full re-clone of a
                        // large volume is expensive, and most stalls a restart can
                        // resolve. The destructive reinitialize fires only on the
                        // same WAL-too-old probe the Waiting branch uses, a
                        // SUFFICIENT condition: an upper bound on the segment the
                        // replica must resume from is older than the oldest WAL the
                        // leader retains, so it is *provably* too far behind to
                        // stream-catch-up (no archive fallback). This is the last-
                        // chance check for the narrow window where the leader was
                        // unreachable during every backed-off Waiting probe but is
                        // reachable now; any other stall falls through to the
                        // recovery exit and the next boot re-probes from scratch.
                        let unrecoverable = match patroni_client.as_ref() {
                            Some(c) => self_heal::confirm_wal_unrecoverable(c, config).await,
                            None => false,
                        };
                        if !reinit_attempted
                            && unrecoverable
                            && try_reinitialize_stalled_replica(config, telemetry, "wal_unrecoverable").await
                        {
                            reinit_attempted = true;
                            startup_elapsed = 0;
                            // Rebaseline progress signals: the reinit wipes pgdata
                            // (volume shrinks, WAL cursor disappears) before the
                            // fresh clone starts laying bytes down.
                            last_volume_used = volume_used_bytes(&config.data_dir);
                            last_xlog_pos = None;
                            continue;
                        }

                        error!(
                            elapsed_without_progress = startup_elapsed,
                            max = config.max_startup_timeout,
                            "Patroni not healthy, and neither volume usage nor WAL position advanced within timeout - exiting for recovery"
                        );
                        telemetry.send(TelemetryEvent::HealthCheckFailed {
                            node: config.name.clone(),
                            consecutive_failures: (startup_elapsed / 5) as u32,
                            max_failures: (config.max_startup_timeout / 5) as u32,
                        });
                        let _ = kill(Pid::from_raw(patroni_pid as i32), Signal::SIGTERM);
                        sleep(Duration::from_secs(2)).await;
                        let _ = kill(Pid::from_raw(patroni_pid as i32), Signal::SIGKILL);
                        std::process::exit(1);
                    }
                    StartupTick::Waiting => {
                        startup_elapsed += 5;
                        // WAL-too-old probe. Once we've gone WAL_PROBE_GRACE_SECS
                        // with no progress at all (so a healthy clone/catch-up is
                        // never probed) we ask the leader whether it still retains
                        // WAL as old as an UPPER BOUND on the segment this replica
                        // must stream from (the successor of its newest local WAL
                        // segment). If even that is gone (and standbys have no
                        // archive fallback), the node can never stream-catch-up, so
                        // reinitialize now instead of waiting out the full
                        // max_startup_timeout. Because we compare an upper bound the
                        // probe is a SUFFICIENT condition: it only fires when the
                        // replica is provably unrecoverable, so it never wipes a
                        // node a restart could have fixed (it may instead MISS a
                        // genuine case in a narrow boundary band — a safe false
                        // negative that just restart-loops as today). See
                        // `confirm_wal_unrecoverable`. Probing backs off
                        // exponentially (see the schedule vars) so an already-
                        // struggling leader isn't queried every cycle.
                        // `confirm_wal_unrecoverable` returns false on any
                        // uncertainty, so we never wipe on a maybe.
                        if !reinit_attempted && startup_elapsed >= next_wal_probe_at {
                            // Schedule the next probe before running this one,
                            // doubling the gap up to the cap — so even a slow
                            // query can't re-enter early, and a long stall keeps
                            // the leader's query load bounded.
                            wal_probe_interval =
                                (wal_probe_interval * 2).min(WAL_PROBE_MAX_INTERVAL_SECS);
                            next_wal_probe_at = startup_elapsed + wal_probe_interval;

                            let unrecoverable = match patroni_client.as_ref() {
                                Some(c) => self_heal::confirm_wal_unrecoverable(c, config).await,
                                None => false,
                            };
                            if unrecoverable
                                && try_reinitialize_stalled_replica(config, telemetry, "wal_unrecoverable").await
                            {
                                reinit_attempted = true;
                                startup_elapsed = 0;
                                last_volume_used = volume_used_bytes(&config.data_dir);
                                last_xlog_pos = None;
                                continue;
                            }
                        }
                        if startup_elapsed >= config.startup_grace_period && startup_elapsed % 30 == 0 {
                            warn!(
                                elapsed_without_progress = startup_elapsed,
                                max = config.max_startup_timeout,
                                "Still waiting for Patroni to become healthy (no volume-usage progress)"
                            );
                        }
                    }
                }
            }
        }
    }

    // Main health monitoring loop
    let mut failures = 0u32;
    info!(
        interval = config.health_check_interval,
        max_failures = config.max_failures,
        "Health monitoring active"
    );

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM");
                let _ = kill(Pid::from_raw(patroni_pid as i32), Signal::SIGTERM);
                let _ = child.wait().await;
                return Ok(());
            }
            _ = sigint.recv() => {
                info!("Received SIGINT");
                let _ = kill(Pid::from_raw(patroni_pid as i32), Signal::SIGTERM);
                let _ = child.wait().await;
                return Ok(());
            }
            status = child.wait() => {
                error!("Patroni process died unexpectedly");
                telemetry.send(TelemetryEvent::ProcessDied {
                    node: config.name.clone(),
                    process: "patroni".to_string(),
                    exit_code: status.ok().and_then(|s| s.code()),
                });
                std::process::exit(1);
            }
            _ = sleep(Duration::from_secs(config.health_check_interval)) => {
                if check_health(config.health_check_timeout).await {
                    if failures > 0 {
                        info!(previous_failures = failures, "Patroni recovered");
                    }
                    failures = 0;
                } else {
                    failures += 1;
                    warn!(failures, max = config.max_failures, "Health check failed");

                    if failures >= config.max_failures {
                        error!(failures, "Patroni unresponsive - exiting");
                        telemetry.send(TelemetryEvent::HealthCheckFailed {
                            node: config.name.clone(),
                            consecutive_failures: failures,
                            max_failures: config.max_failures,
                        });
                        let _ = kill(Pid::from_raw(patroni_pid as i32), Signal::SIGTERM);
                        sleep(Duration::from_secs(2)).await;
                        let _ = kill(Pid::from_raw(patroni_pid as i32), Signal::SIGKILL);
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

/// Force a Patroni reinitialize on a replica whose startup stalled before it
/// ever became healthy — the case the in-process self-heal watcher misses
/// because this gate keeps restarting the container before the watcher's
/// crash-loop signal can accrue, and the node never reaches the healthy state
/// its timeline-divergence path requires (see [`super::self_heal`]). Returns
/// true when a reinitialize was issued — the caller then keeps the container
/// alive and lets the fresh clone proceed (the progress gate above protects
/// it). Returns false to fall back to the recovery exit: we're the leader, the
/// leader is unreachable, the per-hour reinit cap is exhausted, or the local
/// Patroni REST didn't answer.
async fn try_reinitialize_stalled_replica(
    config: &Config,
    telemetry: &Telemetry,
    reason: &str,
) -> bool {
    // Honor the operator kill switch: when self-heal is disabled, fall back to
    // the recovery exit instead of wiping pgdata behind the operator's back.
    if self_heal::disabled() {
        warn!("startup self-heal: SELF_HEAL_DISABLED=1, deferring to restart");
        return false;
    }

    let Some(client) = self_heal::http_client() else {
        return false;
    };
    // Require a role from Patroni's local REST before acting (None = REST didn't
    // answer → defer to restart). We never wipe a confirmed leader; an Unknown
    // role (a joining/uninitialized node) is treated as actionable, because a
    // fresh clone is the right recovery for one genuinely stalled with a
    // reachable leader — see [`self_heal::should_reinit_stalled`].
    let Some(role) = self_heal::local_role(&client).await else {
        warn!("startup self-heal: local Patroni role unknown, deferring to restart");
        return false;
    };
    let leader_reachable = self_heal::is_leader_reachable(&client, 3).await;
    let cap = self_heal::reinit_attempt_cap();
    let attempts = self_heal::recent_reinit_attempts();
    // Unlike the watcher we deliberately do NOT apply `action_backoff_secs` here:
    // the caller's one-reinit-per-container-start guard plus the shared per-hour
    // cap already bound the rate, and a stalled startup shouldn't wait out a
    // 10-minute backoff before its first clone attempt.
    if !self_heal::should_reinit_stalled(role, leader_reachable, attempts, cap) {
        warn!(
            ?role,
            leader_reachable,
            attempts,
            cap,
            "startup self-heal: conditions not met (leader, unreachable leader, or cap reached); restarting for recovery"
        );
        return false;
    }

    // Count before issuing — shared per-hour budget with the watcher — so a
    // wedged Patroni REST can't drive unbounded reinitializes.
    self_heal::record_reinit_attempt();
    let attempt = attempts + 1;
    match self_heal::force_reinitialize(&client).await {
        Ok(()) => {
            warn!(
                reason,
                attempt, "startup self-heal: forced reinitialize on stalled replica"
            );
            telemetry.send(TelemetryEvent::SelfHealReinitTriggered {
                node: config.name.clone(),
                reason: reason.to_string(),
                attempt,
            });
            ensure_reinitialize_unparked(config).await;
            true
        }
        Err(e) => {
            warn!(
                reason,
                error = %e,
                attempt,
                "startup self-heal: reinitialize request failed; restarting for recovery"
            );
            telemetry.send(TelemetryEvent::SelfHealReinitRequestFailed {
                node: config.name.clone(),
                reason: reason.to_string(),
                attempt,
                error: e.to_string(),
            });
            false
        }
    }
}

/// How long an ACCEPTED force-reinitialize may sit with no visible effect
/// (pg_control still present) before we conclude Patroni has parked it and
/// preempt Postgres ourselves. Long enough for Patroni's normal
/// stop→wipe→clone sequencing on a loaded node; far below the
/// max_startup_timeout cliff this path exists to beat. The RETRY window is
/// shorter: after the first preempt+reissue we're confirming that action
/// worked, not rediscovering the park from scratch.
const REINIT_PARK_TIMEOUT_SECS: u64 = 90;
const REINIT_PARK_RETRY_WAIT_SECS: u64 = 30;
const REINIT_PARK_POLL_SECS: u64 = 2;
/// Timeout for the re-issued `POST /reinitialize` specifically — NOT the
/// shared 5s `self_heal::http_client()`. Patroni cancelling the existing
/// parked task and scheduling a fresh one is itself slow (observed: our 5s
/// client errored out while Patroni's own log showed "Cancelling long
/// running task reinitialize" landing several seconds later) — a timeout
/// here reads as "request failed" when the request actually succeeded.
const REINIT_REISSUE_TIMEOUT_SECS: u64 = 20;
/// Total preempt+reissue cycles before giving up and letting the outer
/// recovery-exit path (container restart, next boot re-decides) take over.
const REINIT_UNPARK_MAX_ATTEMPTS: u32 = 3;

/// Patroni parks an accepted force-reinitialize behind a postgres that never
/// leaves "starting": the reinitialize must stop postgres before it can wipe,
/// but the in-flight start action never completes while the startup process
/// cycles retries for WAL it can never obtain — observed as "reinitialize in
/// progress" for 6+ minutes with zero data-wipe progress. Postgres dying on
/// its own (a crash, an operator kill) does NOT unstick Patroni's side
/// either — observed directly: the parked task sat idle for 90s+ after
/// postmaster.pid had already vanished, and only a fresh
/// `POST /reinitialize` made Patroni log "Cancelling long running task
/// reinitialize" and move.
///
/// Watch for the wipe (pg_control vanishing is its first observable step);
/// if nothing has happened inside the park timeout, stop Postgres (a no-op
/// if it already died on its own) and re-issue the reinitialize so Patroni
/// cancels whatever it had parked and reschedules against the now-stopped
/// node. Safe by construction at this call site: the WAL-too-old verdict
/// proved the node cannot stream-catch-up, and Patroni has already ACCEPTED
/// an instruction to wipe this node — none of that changes across retries.
/// Repeats up to `REINIT_UNPARK_MAX_ATTEMPTS` times (a single
/// preempt+reissue is not guaranteed to land the first time) before giving
/// up loudly.
async fn ensure_reinitialize_unparked(config: &Config) {
    let pg_control = format!("{}/global/pg_control", config.data_dir);

    if wait_for_wipe(&pg_control, REINIT_PARK_TIMEOUT_SECS).await {
        return;
    }

    for attempt in 1..=REINIT_UNPARK_MAX_ATTEMPTS {
        warn!(
            attempt,
            park_timeout_secs = REINIT_PARK_TIMEOUT_SECS,
            "startup self-heal: reinitialize accepted but no data wipe within the park timeout — preempting postgres so Patroni can act"
        );
        // Signal the postmaster directly rather than shelling out to
        // pg_ctl: Debian's postgresql-common wraps psql/pg_controldata onto
        // PATH but deliberately NOT pg_ctl, so a bare pg_ctl spawn fails
        // instantly in this process. SIGINT = fast shutdown, SIGQUIT =
        // immediate. A no-op (logged) if postgres already died on its own.
        stop_postgres_directly(&config.data_dir).await;

        let reissue_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(REINIT_REISSUE_TIMEOUT_SECS))
            .build();
        match reissue_client {
            Ok(c) => match self_heal::force_reinitialize(&c).await {
                Ok(()) => info!(attempt, "startup self-heal: re-issued reinitialize after preempting the wedged postgres"),
                Err(e) => warn!(attempt, error = %e, "startup self-heal: reinitialize re-issue failed even with the longer timeout"),
            },
            Err(e) => warn!(attempt, error = %e, "startup self-heal: failed to build the re-issue HTTP client"),
        }

        if wait_for_wipe(&pg_control, REINIT_PARK_RETRY_WAIT_SECS).await {
            return;
        }
    }

    warn!(
        attempts = REINIT_UNPARK_MAX_ATTEMPTS,
        "startup self-heal: reinitialize still parked after all unpark attempts — leaving it to the recovery exit"
    );
}

/// Poll for `pg_control`'s disappearance (the wipe's first observable step)
/// for up to `timeout_secs`. Returns `true` once it's gone (unparked —
/// nothing more to do), `false` if the window elapsed with it still present
/// (still parked — caller should act).
async fn wait_for_wipe(pg_control: &str, timeout_secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    while tokio::time::Instant::now() < deadline {
        if !Path::new(pg_control).exists() {
            return true;
        }
        sleep(Duration::from_secs(REINIT_PARK_POLL_SECS)).await;
    }
    false
}

/// True when `pid` is alive AND its `/proc/<pid>/comm` reads `postgres`.
/// The pidfile pid can go stale in exactly the crash-loop gaps this path
/// lives in, and a bare `kill(pid, 0)` liveness check would then aim the
/// shutdown signals at whatever process recycled the pid. comm is the
/// kernel-side name — the postmaster's is always `postgres` (process
/// retitling only touches argv) — and callers re-check it on every poll,
/// so an exit-and-recycle mid-wait reads as "stopped" instead of keeping
/// the SIGQUIT escalation pointed at an innocent process.
fn pid_is_live_postmaster(pid: Pid) -> bool {
    if kill(pid, None).is_err() {
        return false;
    }
    std::fs::read_to_string(format!("/proc/{}/comm", pid.as_raw()))
        .map(|comm| comm.trim() == "postgres")
        .unwrap_or(false)
}

/// Stop the local postmaster by pid-file signal: SIGINT (fast shutdown),
/// escalating to SIGQUIT (immediate) if it hasn't exited within the fast
/// window. Returns silently when there is no postmaster to stop — between
/// crash-loop cycles that is the normal case and exactly what the caller
/// wants. A stale pidfile counts as "no postmaster": the pid must read as
/// a live `postgres` process (`pid_is_live_postmaster`) to be signaled at
/// all, re-verified between signals. Only invoked on a node already
/// sentenced to wipe-and-reseed.
async fn stop_postgres_directly(data_dir: &str) {
    let pidfile = format!("{data_dir}/postmaster.pid");
    let pid = std::fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.lines().next().and_then(|l| l.trim().parse::<i32>().ok()));
    let Some(pid) = pid else {
        info!("startup self-heal: no postmaster.pid — postgres already down");
        return;
    };
    let pid = Pid::from_raw(pid);
    if !pid_is_live_postmaster(pid) {
        info!("startup self-heal: postmaster.pid is stale (pid gone or not a postgres process) — postgres already down");
        return;
    }
    let _ = kill(pid, Signal::SIGINT);
    for _ in 0..15 {
        if !pid_is_live_postmaster(pid) {
            info!("startup self-heal: postgres stopped after fast-shutdown signal");
            return;
        }
        sleep(Duration::from_secs(2)).await;
    }
    warn!("startup self-heal: fast shutdown did not complete; escalating to immediate");
    let _ = kill(pid, Signal::SIGQUIT);
    for _ in 0..8 {
        if !pid_is_live_postmaster(pid) {
            info!("startup self-heal: postgres stopped after immediate-shutdown signal");
            return;
        }
        sleep(Duration::from_secs(2)).await;
    }
    warn!("startup self-heal: postmaster survived both shutdown signals; leaving it to the recovery exit");
}

/// Outcome of one 5s startup poll. Pure so the progress-gated timeout is
/// unit-testable without spinning Patroni.
#[derive(Debug, PartialEq, Eq)]
enum StartupTick {
    /// Patroni reports healthy — leave the startup wait.
    Healthy,
    /// Not healthy yet but pgdata is growing — a clone/initdb is in flight, so
    /// the no-progress clock resets (never kill a clone mid-stream).
    Progressing,
    /// Not healthy and no progress for `max_startup_timeout` — exit for recovery.
    Stalled,
    /// Not healthy, no progress yet, still under the timeout — keep waiting.
    Waiting,
}

fn classify_startup_tick(
    healthy: bool,
    progressing: bool,
    elapsed_without_progress: u64,
    max_timeout: u64,
) -> StartupTick {
    if healthy {
        StartupTick::Healthy
    } else if progressing {
        StartupTick::Progressing
    } else if elapsed_without_progress >= max_timeout {
        StartupTick::Stalled
    } else {
        StartupTick::Waiting
    }
}

/// Used bytes on the filesystem holding `path` (O(1) `statvfs`, no tree walk).
/// The startup progress signal for the pg_basebackup phase: a clone in flight
/// grows the volume as data files land, and it costs one syscall instead of
/// stat'ing every relation file every 5s. (The catch-up phase, where disk
/// usage holds flat while WAL replays, is covered by [`xlog_advanced`]
/// instead.) Best-effort: a missing path or `statvfs` error returns 0 (the
/// next successful read then registers as growth, i.e. progress).
fn volume_used_bytes(path: &str) -> u64 {
    use nix::sys::statvfs::statvfs;

    statvfs(std::path::Path::new(path))
        .ok()
        .and_then(|s| {
            let frag = s.fragment_size() as u64;
            let used_blocks = (s.blocks() as u64).checked_sub(s.blocks_free() as u64)?;
            used_blocks.checked_mul(frag)
        })
        .unwrap_or(0)
}

/// True when the replica's WAL position advanced between two startup polls —
/// progress that whole-volume byte growth misses during catch-up (WAL streamed
/// in ≈ recycled out holds disk usage flat while received/replayed LSN climbs).
/// Pure/unit-tested. A missing baseline or current reading (node still in
/// pg_basebackup with no xlog yet, or the REST API not answering) is NOT
/// advancement, so it never masks a hung clone — that path falls back to the
/// volume-usage signal, which a dead clone also leaves flat → still stalls.
fn xlog_advanced(last: Option<i64>, current: Option<i64>) -> bool {
    matches!((last, current), (Some(l), Some(c)) if c > l)
}

/// Best-effort read of this node's furthest WAL position (max of received /
/// replayed location) from the local Patroni REST API (`/patroni`, which
/// answers 200 on the leader and 503 on replicas with the same body shape).
/// None during pg_basebackup (no xlog block yet) or on any transport/parse
/// error.
async fn fetch_patroni_xlog_position(client: &reqwest::Client) -> Option<i64> {
    let resp = client
        .get("http://localhost:8008/patroni")
        .send()
        .await
        .ok()?;
    let body = resp.text().await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let xlog = v.get("xlog")?;
    let received = xlog.get("received_location").and_then(|x| x.as_i64());
    let replayed = xlog.get("replayed_location").and_then(|x| x.as_i64());
    match (received, replayed) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_for_wipe_true_when_pg_control_absent() {
        // Contract pin: TRUE means "wipe under way — nothing to unpark",
        // and ensure_reinitialize_unparked RETURNS on true. This polarity
        // was once inverted, which silently disabled the entire park-watch
        // (no preempt, no warn, reinitialize parked forever) while every
        // individual log line still looked normal — cost a red CI run and
        // hours of live-instrumented debugging to find.
        let missing = format!(
            "{}/wait_for_wipe_absent_{}/pg_control",
            std::env::temp_dir().display(),
            std::process::id()
        );
        assert!(wait_for_wipe(&missing, 1).await);
    }

    #[tokio::test]
    async fn wait_for_wipe_false_while_pg_control_survives_the_window() {
        let dir = std::env::temp_dir().join(format!("wait_for_wipe_present_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("pg_control");
        std::fs::write(&f, "x").unwrap();
        assert!(!wait_for_wipe(f.to_str().unwrap(), 1).await);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn healthy_breaks_regardless_of_progress_or_elapsed() {
        assert_eq!(
            classify_startup_tick(true, false, 9_999, 300),
            StartupTick::Healthy
        );
        assert_eq!(
            classify_startup_tick(true, true, 0, 300),
            StartupTick::Healthy
        );
    }

    #[test]
    fn progress_resets_even_past_the_timeout() {
        // A large clone can run far past max_startup_timeout in wall-clock; as
        // long as it's progressing it must never be classified Stalled.
        assert_eq!(
            classify_startup_tick(false, true, 100_000, 300),
            StartupTick::Progressing
        );
    }

    #[test]
    fn no_progress_past_timeout_stalls() {
        assert_eq!(
            classify_startup_tick(false, false, 300, 300),
            StartupTick::Stalled
        );
        assert_eq!(
            classify_startup_tick(false, false, 305, 300),
            StartupTick::Stalled
        );
    }

    #[test]
    fn no_progress_under_timeout_waits() {
        assert_eq!(
            classify_startup_tick(false, false, 0, 300),
            StartupTick::Waiting
        );
        assert_eq!(
            classify_startup_tick(false, false, 295, 300),
            StartupTick::Waiting
        );
    }

    #[test]
    fn xlog_advance_only_on_a_higher_position_with_a_baseline() {
        // Catch-up: LSN climbed since last poll → progress.
        assert!(xlog_advanced(Some(1_000), Some(2_000)));
        // Frozen LSN (stalled replay) → no progress.
        assert!(!xlog_advanced(Some(2_000), Some(2_000)));
        // Regressed/garbage reading → not progress.
        assert!(!xlog_advanced(Some(2_000), Some(1_000)));
        // First reading (no baseline yet) → not yet progress; sets the baseline.
        assert!(!xlog_advanced(None, Some(2_000)));
        // No current reading (pg_basebackup, no xlog / REST down) → defer to the
        // volume signal; never counts as WAL progress on its own.
        assert!(!xlog_advanced(Some(2_000), None));
        assert!(!xlog_advanced(None, None));
    }

    #[test]
    fn volume_used_bytes_missing_path_is_zero() {
        assert_eq!(volume_used_bytes("/nonexistent/path/for/test"), 0);
    }

    #[test]
    fn volume_used_bytes_reports_nonzero_for_real_fs() {
        // statvfs of any existing path resolves to its filesystem; a live fs
        // always has some blocks in use, so this is the "progress signal works"
        // smoke test without needing to spin Patroni.
        let dir = std::env::temp_dir();
        assert!(volume_used_bytes(dir.to_str().unwrap()) > 0);
    }
}
