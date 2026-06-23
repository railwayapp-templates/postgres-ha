//! Patroni process monitoring
//!
//! Handles the monitoring loop, signal handling, and health check management.

use super::{check_health, self_heal, Config};
use common::{Telemetry, TelemetryEvent};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
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
    // [`try_reinitialize_stalled_replica`]. We try a reinitialize once instead of
    // restarting into the same wall; if it doesn't take, the next boot re-decides
    // against the persistent per-hour cap rather than re-wiping a half-laid clone.
    let mut reinit_attempted = false;
    // Backoff schedule for the authoritative WAL-availability probe (see the
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
                        // No progress for the full stall timeout. A plain restart
                        // can't fix this — force a reinitialize so a fresh
                        // pg_basebackup gives the volume something to grow again,
                        // which the progress gate above protects to completion.
                        // Falls through to the recovery exit when we're the leader,
                        // the leader is unreachable, or the per-hour cap is exhausted.
                        if !reinit_attempted
                            && try_reinitialize_stalled_replica(config, telemetry, "stalled_startup").await
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
                        // Authoritative WAL-too-old check. Once we've gone
                        // WAL_PROBE_GRACE_SECS with no progress at all (so a
                        // healthy clone/catch-up is never probed) we ask the
                        // leader the exact question Postgres itself fails on:
                        // "do you still have the WAL segment this replica must
                        // resume from?" If it's been recycled (and standbys have
                        // no archive fallback), the node can never stream-catch-up
                        // — reinitialize now instead of waiting out the full
                        // max_startup_timeout. Probing backs off exponentially
                        // (see the schedule vars) so an already-struggling leader
                        // isn't queried every cycle; the *decision* is the
                        // WAL-availability fact, not a timer.
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
