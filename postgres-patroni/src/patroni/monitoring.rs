//! Patroni process monitoring
//!
//! Handles the monitoring loop, signal handling, and health check management.

use super::{check_health, self_heal, Config};
use common::{ConfigExt, Telemetry, TelemetryEvent};
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
/// Extra zero-progress dwell demanded, past the first positive WAL-too-old
/// verdict, before the startup gate wipes a replica on a cluster WITH a WAL
/// archive. There standbys have an archive fallback (`restore_command`, seeded
/// alongside the archive params in yaml.rs): leader-recycled WAL alone no
/// longer proves the replica is stuck, because recovery normally pulls the gap
/// from the S3 archive on its own — which registers as progress and resets the
/// gate before any probe fires. A verdict that survives this dwell with still
/// zero progress means the archive path is stalled too (a WAL gap in the
/// archive, a stale repo path, or an object-store outage that has outlasted
/// the dwell), and the wipe-and-reseed is the remaining move. Without an
/// archive the verdict is already final and fires immediately, exactly as
/// before.
///
/// The Stalled branch's immediate fire assumes `max_startup_timeout` (default
/// 1800, operator-overridable) is comfortably larger than this dwell — that's
/// what makes "reached Stalled" imply "the archive had longer than the dwell".
/// An operator setting max_startup_timeout below ~this value would let the
/// Stalled path wipe an archiving replica with less zero-progress time than
/// the Waiting path demands. Not enforced (both are independently
/// operator-overridable env vars), but `run_monitoring_loop` warns loudly at
/// startup on an archiving cluster where the relationship doesn't hold.
///
/// Scope: the dwell — like the reinitialize it gates — only governs replicas
/// still inside STARTUP monitoring (never became healthy this container
/// lifetime). A standby that reached consistency and runs wedged (streaming
/// refused because the leader recycled its WAL, archive unreachable) reads
/// as healthy here and exits the loop; `restore_command` itself is the
/// remedy for that state, kicking in as soon as the archive is reachable.
///
/// `WAL_ARCHIVE_STALL_CONFIRM_SECONDS` env-overrides this default — mirrors
/// `WAL_BACKUP_FULL_INTERVAL_SECONDS` in `backup_watcher.rs`: this dwell is a
/// real-time wall-clock wait inside the running monitoring loop (unlike the
/// pure `wal_reinit_confirmed` unit tests below, which exercise the arithmetic
/// without waiting), so the e2e harness needs a way to shrink it to seconds
/// instead of waiting out 300s per scenario.
const WAL_ARCHIVE_STALL_CONFIRM_SECS: u64 = 300;
/// Slack for the volume-usage progress signal: a poll-to-poll DROP larger
/// than this is a data-directory wipe (a reinitialize, or Patroni's
/// diverged-timeline cleanup) and rebaselines the high-water mark so the
/// follow-up re-clone registers as progress from its first laid byte.
/// Anything smaller is crash-cycle churn: a crash-looping startup (e.g. the
/// archive-get connectivity breaker FATALing recovery every trip window)
/// deletes and recreates small files on every cycle — postmaster.pid,
/// pgbackrest spool status, RECOVERY temp files — which oscillates statvfs
/// around a plateau with zero real progress. Judged poll-to-poll, the
/// climb-back half of each oscillation read as growth and reset the
/// zero-progress clock, so a wedged replica could never accrue the stall
/// time the WAL-too-old reinit gate below requires (observed: resets every
/// 10–25s at a byte-identical plateau, gate never armed). Judged against a
/// high-water mark, churn stays at-or-below the mark and only genuinely new
/// bytes count. 64 MiB is far above any plausible churn amplitude while any
/// pgdata small enough to be wiped without tripping the rebaseline re-clones
/// long before the progress gate's timeout could matter.
const VOLUME_WIPE_REBASELINE_BYTES: u64 = 64 * 1024 * 1024;

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
    //   1. Volume usage growing past its episode high-water mark —
    //      pg_basebackup laying data files down. This is the only signal
    //      available before Patroni's REST API answers. High-water, not
    //      poll-to-poll: see `volume_progressed`.
    //   2. WAL position (received/replayed LSN) advancing — covers the
    //      post-basebackup catch-up phase, where the replica streams + replays
    //      WAL with disk usage ~flat (segments recycled about as fast as they
    //      arrive) yet the LSN keeps climbing. Volume bytes alone would read
    //      that as "stalled" and wrongly kill a healthy catch-up.
    // A genuinely stalled startup — and a hung clone (dead replication socket) —
    // advances NEITHER signal, so it still exits for recovery after the timeout.
    let mut startup_elapsed = 0u64;
    let mut volume_baseline = volume_used_bytes(&config.data_dir);
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
    // `startup_elapsed` at the FIRST positive WAL-too-old verdict of the
    // current stall episode. On archiving clusters the destructive reinit is
    // deferred until the stall outlives WAL_ARCHIVE_STALL_CONFIRM_SECS past
    // this point (see `wal_reinit_confirmed`); resets with the other episode
    // state whenever progress lands.
    let mut wal_unrecoverable_since: Option<u64> = None;
    // Resolved once per loop entry (not per-tick): the dwell only matters at
    // the moment of the reinit decision, and re-reading the env every 5s tick
    // buys nothing since it can't change under a running process.
    let wal_archive_stall_confirm_secs =
        u64::env_parse("WAL_ARCHIVE_STALL_CONFIRM_SECONDS", WAL_ARCHIVE_STALL_CONFIRM_SECS);
    // The Stalled branch's immediate-fire assumption (see the const doc comment
    // above) only holds when max_startup_timeout comfortably exceeds the dwell.
    // Nothing else enforces that relationship, so surface it loudly rather than
    // let it silently undercut the dwell the Waiting branch just paid for.
    if archive_stall_dwell_exceeds_startup_timeout(
        config.wal_archive_bucket.is_some(),
        config.max_startup_timeout,
        wal_archive_stall_confirm_secs,
    ) {
        warn!(
            max_startup_timeout = config.max_startup_timeout,
            wal_archive_stall_confirm_secs,
            "max_startup_timeout is shorter than the WAL archive-stall confirmation dwell — the Stalled-branch reinit can fire with less zero-progress time than the Waiting branch requires; raise max_startup_timeout or WAL_ARCHIVE_STALL_CONFIRM_SECONDS to restore the intended margin"
        );
    }
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
                let volume_grew = volume_progressed(&mut volume_baseline, used);

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
                        // Progress also disarms the archive-stall confirmation:
                        // on archiving clusters it usually IS the archive
                        // fallback quietly fixing the WAL-too-old state.
                        wal_unrecoverable_since = None;
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
                        // stream-catch-up. This is the last-chance check for the
                        // narrow window where the leader was unreachable during
                        // every backed-off Waiting probe but is reachable now; any
                        // other stall falls through to the recovery exit and the
                        // next boot re-probes from scratch. Unlike the Waiting
                        // branch, a positive verdict fires immediately even on
                        // archiving clusters: reaching Stalled means zero progress
                        // for the full max_startup_timeout, so the archive
                        // fallback has already had far longer than
                        // WAL_ARCHIVE_STALL_CONFIRM_SECS to produce a single byte
                        // and didn't — the verdict is final here too.
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
                            volume_baseline = volume_used_bytes(&config.data_dir);
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
                        // segment). If even that is gone the node can never
                        // stream-catch-up, so reinitialize instead of waiting out
                        // the full max_startup_timeout — immediately on clusters
                        // without a WAL archive (streaming was the only source),
                        // and only after WAL_ARCHIVE_STALL_CONFIRM_SECS more of
                        // zero progress on archiving clusters, where standbys
                        // normally self-serve the gap from S3 via restore_command
                        // and a short object-store blip must not cost a full
                        // re-seed (see `wal_reinit_confirmed`). Because we compare
                        // an upper bound the probe is a SUFFICIENT condition: it
                        // only fires when the replica is provably unrecoverable
                        // by streaming, so it never wipes a node a restart could
                        // have fixed (it may instead MISS a genuine case in a
                        // narrow boundary band — a safe false negative that just
                        // restart-loops as today). See
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
                            if unrecoverable && wal_unrecoverable_since.is_none() {
                                wal_unrecoverable_since = Some(startup_elapsed);
                                if config.wal_archive_bucket.is_some() {
                                    warn!(
                                        confirm_dwell_secs = wal_archive_stall_confirm_secs,
                                        "WAL-too-old confirmed for streaming, but this cluster archives WAL and standbys self-serve missed segments via restore_command — deferring reinitialize until the zero-progress stall outlives the dwell"
                                    );
                                }
                            }
                            if unrecoverable
                                && wal_reinit_confirmed(
                                    config.wal_archive_bucket.is_some(),
                                    wal_unrecoverable_since,
                                    startup_elapsed,
                                    wal_archive_stall_confirm_secs,
                                )
                                && try_reinitialize_stalled_replica(config, telemetry, "wal_unrecoverable").await
                            {
                                reinit_attempted = true;
                                startup_elapsed = 0;
                                volume_baseline = volume_used_bytes(&config.data_dir);
                                last_xlog_pos = None;
                                wal_unrecoverable_since = None;
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
/// cycles restore_command/streaming retries — observed as "reinitialize in
/// progress" for 6+ minutes with zero data-wipe progress. The archive-get
/// wrapper's connectivity breaker usually prevents that state (it FATALs
/// startup when the archive ENDPOINT is dead), but it cannot help when the
/// archive answers and simply lacks the WAL this node needs — a gap in
/// archived history keeps returning honest misses and the eternal start
/// persists. Postgres dying on its own (via the breaker, or any other
/// crash) does NOT unstick Patroni's side either — observed directly: the
/// parked task sat idle for 90s+ after postmaster.pid had already vanished,
/// and only a fresh `POST /reinitialize` made Patroni log "Cancelling long
/// running task reinitialize" and move.
///
/// Watch for the wipe (pg_control vanishing is its first observable step);
/// if nothing has happened inside the park timeout, stop Postgres (a no-op
/// if it already died on its own) and re-issue the reinitialize so Patroni
/// cancels whatever it had parked and reschedules against the now-stopped
/// node. Safe by construction at this call site: the WAL-too-old verdict
/// proved the node cannot stream-catch-up, the archive dwell proved the
/// fallback is stalled, and Patroni has already ACCEPTED an instruction to
/// wipe this node — none of that changes across retries. Repeats up to
/// `REINIT_UNPARK_MAX_ATTEMPTS` times (a single preempt+reissue is not
/// guaranteed to land the first time) before giving up loudly.
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
/// lives in (e.g. moments after the archive-get connectivity breaker
/// FATALs startup), and a bare `kill(pid, 0)` liveness check would then
/// aim the shutdown signals at whatever process recycled the pid. comm is
/// the kernel-side name — the postmaster's is always `postgres` (process
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
/// crash-loop cycles (e.g. after the archive-get connectivity breaker
/// FATALs startup) that is the normal case and exactly what the caller
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

/// Gate for acting on a positive WAL-too-old verdict in the `Waiting` branch.
/// Without a WAL archive the verdict is final — streaming was the standby's
/// only WAL source — so fire immediately. With one, standbys also have
/// `restore_command`, and a *working* archive fallback surfaces as replay
/// progress that resets the whole stall episode before this gate is ever
/// consulted — so a positive verdict here still doesn't prove the node is
/// stuck until the zero-progress stall has also outlived `confirm_secs`
/// (normally `WAL_ARCHIVE_STALL_CONFIRM_SECS`, env-overridable for tests) past
/// the first positive verdict (`pending_since`, in startup-elapsed seconds).
/// Both clocks reset together on any progress, so the subtraction never spans
/// stall episodes. Pure and unit-tested.
fn wal_reinit_confirmed(
    has_archive_fallback: bool,
    pending_since: Option<u64>,
    startup_elapsed: u64,
    confirm_secs: u64,
) -> bool {
    if !has_archive_fallback {
        return true;
    }
    match pending_since {
        Some(t) => startup_elapsed.saturating_sub(t) >= confirm_secs,
        None => false,
    }
}

/// True when the Stalled branch's immediate-fire assumption (see
/// `WAL_ARCHIVE_STALL_CONFIRM_SECS`'s doc comment) doesn't hold: on an
/// archiving cluster, "reached Stalled" only implies "the archive fallback
/// had longer than the dwell" if `max_startup_timeout` is at least as large
/// as the dwell. Pure and unit-tested; the caller logs a warning when this
/// returns `true` rather than changing behavior — both knobs are legitimate,
/// independent operator overrides, so this is a footgun warning, not a gate.
fn archive_stall_dwell_exceeds_startup_timeout(
    has_archive_fallback: bool,
    max_startup_timeout: u64,
    confirm_secs: u64,
) -> bool {
    has_archive_fallback && max_startup_timeout < confirm_secs
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

/// Volume-growth progress judged against a high-water baseline rather than
/// the previous poll. Returns true — and raises the baseline — only when
/// `used` exceeds the highest usage seen so far, so the delete/recreate churn
/// of a crash-looping startup (statvfs dips, then climbs back to the same
/// plateau) never reads as progress. A drop larger than
/// `VOLUME_WIPE_REBASELINE_BYTES` is a wipe, not churn: rebaseline downward —
/// without claiming progress for the drop itself — so the subsequent
/// re-clone's growth counts from its first byte instead of being swallowed by
/// the pre-wipe mark (a clone must never be killed mid-stream for reading as
/// stalled). Pure and unit-tested.
fn volume_progressed(baseline: &mut u64, used: u64) -> bool {
    if used > *baseline {
        *baseline = used;
        return true;
    }
    if baseline.saturating_sub(used) > VOLUME_WIPE_REBASELINE_BYTES {
        *baseline = used;
    }
    false
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

    #[test]
    fn wal_reinit_fires_immediately_without_archive() {
        // Pre-archive behavior unchanged: streaming was the only WAL source,
        // so a positive verdict is final on its own. confirm_secs is
        // irrelevant on this branch — pass a nonzero value to prove it's
        // ignored, not just coincidentally zero.
        assert!(wal_reinit_confirmed(false, None, 30, WAL_ARCHIVE_STALL_CONFIRM_SECS));
        assert!(wal_reinit_confirmed(false, Some(30), 30, WAL_ARCHIVE_STALL_CONFIRM_SECS));
    }

    #[test]
    fn wal_reinit_waits_out_archive_stall_dwell() {
        // First positive verdict arms at t=30; on an archiving cluster the
        // wipe waits until the zero-progress stall outlives the dwell.
        assert!(!wal_reinit_confirmed(true, Some(30), 30, WAL_ARCHIVE_STALL_CONFIRM_SECS));
        assert!(!wal_reinit_confirmed(
            true,
            Some(30),
            30 + WAL_ARCHIVE_STALL_CONFIRM_SECS - 1,
            WAL_ARCHIVE_STALL_CONFIRM_SECS
        ));
        assert!(wal_reinit_confirmed(
            true,
            Some(30),
            30 + WAL_ARCHIVE_STALL_CONFIRM_SECS,
            WAL_ARCHIVE_STALL_CONFIRM_SECS
        ));
    }

    #[test]
    fn wal_reinit_never_fires_unarmed_on_archiving_cluster() {
        // Defensive: however long the stall, a verdict that was never armed
        // (progress reset the episode) doesn't wipe.
        assert!(!wal_reinit_confirmed(true, None, 9_999, WAL_ARCHIVE_STALL_CONFIRM_SECS));
    }

    #[test]
    fn wal_reinit_honors_a_shortened_confirm_secs() {
        // The e2e harness overrides WAL_ARCHIVE_STALL_CONFIRM_SECONDS to a
        // small value so it doesn't wait out the real 300s default; the gate
        // must key off whatever confirm_secs it's handed, not the constant.
        assert!(!wal_reinit_confirmed(true, Some(30), 34, 5));
        assert!(wal_reinit_confirmed(true, Some(30), 35, 5));
    }

    #[test]
    fn wal_reinit_confirm_secs_zero_fires_on_the_same_tick_it_arms() {
        // Boundary: confirm_secs=0 means "no dwell" — the verdict is
        // confirmed as soon as it's armed (pending_since == startup_elapsed).
        assert!(wal_reinit_confirmed(true, Some(30), 30, 0));
    }

    #[test]
    fn wal_reinit_pending_since_never_exceeds_startup_elapsed_in_practice() {
        // Defensive against the impossible case (pending_since set from a
        // future tick relative to startup_elapsed): saturating_sub floors at
        // 0 rather than underflowing/panicking, so this stays a safe "not
        // confirmed yet" instead of a crash.
        assert!(!wal_reinit_confirmed(true, Some(100), 50, 10));
    }

    #[test]
    fn dwell_warning_silent_without_archive_regardless_of_timeout() {
        // Non-archiving clusters never consult the dwell at all — no
        // relationship to warn about, even with a tiny max_startup_timeout.
        assert!(!archive_stall_dwell_exceeds_startup_timeout(false, 0, WAL_ARCHIVE_STALL_CONFIRM_SECS));
        assert!(!archive_stall_dwell_exceeds_startup_timeout(false, 9_999, WAL_ARCHIVE_STALL_CONFIRM_SECS));
    }

    #[test]
    fn dwell_warning_fires_when_archiving_and_timeout_shorter_than_dwell() {
        assert!(archive_stall_dwell_exceeds_startup_timeout(true, 299, 300));
        assert!(archive_stall_dwell_exceeds_startup_timeout(true, 0, 300));
    }

    #[test]
    fn dwell_warning_silent_when_timeout_meets_or_exceeds_dwell() {
        // Equal is fine: the Stalled branch's own zero-progress wait then
        // matches the dwell exactly, so the Waiting path's guarantee still
        // holds at the boundary.
        assert!(!archive_stall_dwell_exceeds_startup_timeout(true, 300, 300));
        assert!(!archive_stall_dwell_exceeds_startup_timeout(true, 1800, 300));
    }

    #[test]
    fn dwell_warning_honors_a_shortened_confirm_secs_override() {
        // Mirrors wal_reinit_honors_a_shortened_confirm_secs: the check must
        // key off whatever confirm_secs the env override resolved to, not
        // the WAL_ARCHIVE_STALL_CONFIRM_SECS constant.
        assert!(!archive_stall_dwell_exceeds_startup_timeout(true, 10, 5));
        assert!(archive_stall_dwell_exceeds_startup_timeout(true, 4, 5));
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
    fn volume_progress_requires_a_new_high_water_mark() {
        // Crash-cycle churn: statvfs dips below the plateau, then climbs back
        // to it. Neither half may read as progress, and the mark must hold.
        let mut baseline = 1_000_000u64;
        assert!(!volume_progressed(&mut baseline, 900_000));
        assert!(!volume_progressed(&mut baseline, 1_000_000));
        assert_eq!(baseline, 1_000_000);
        // Genuinely new bytes beyond the mark are progress and raise it.
        assert!(volume_progressed(&mut baseline, 1_000_001));
        assert_eq!(baseline, 1_000_001);
    }

    #[test]
    fn volume_wipe_rebaselines_without_claiming_progress() {
        let start = VOLUME_WIPE_REBASELINE_BYTES * 10;
        let mut baseline = start;
        let post_wipe = start - VOLUME_WIPE_REBASELINE_BYTES - 1;
        // The wipe itself is not progress, but it moves the baseline down…
        assert!(!volume_progressed(&mut baseline, post_wipe));
        assert_eq!(baseline, post_wipe);
        // …so the re-clone's very next bytes count as progress again, even
        // though they are far below the pre-wipe mark.
        assert!(volume_progressed(&mut baseline, post_wipe + 1));
    }

    #[test]
    fn volume_drop_within_slack_is_churn_not_a_wipe() {
        let start = VOLUME_WIPE_REBASELINE_BYTES * 10;
        let mut baseline = start;
        // A drop exactly at the slack boundary keeps the mark, so the
        // climb-back to the old plateau must not read as progress.
        assert!(!volume_progressed(
            &mut baseline,
            start - VOLUME_WIPE_REBASELINE_BYTES
        ));
        assert_eq!(baseline, start);
        assert!(!volume_progressed(&mut baseline, start));
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
