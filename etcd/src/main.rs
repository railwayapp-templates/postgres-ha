//! etcd bootstrap wrapper with leader-based startup and learner mode
//!
//! Bootstraps etcd cluster using single-node + learner pattern to avoid deadlocks:
//! 1. Leader (alphabetically first) bootstraps single-node cluster
//! 2. Other nodes wait, then join as learners (non-voting)
//! 3. Learners promote to voting members once healthy
//!
//! Recovery: If leader loses volume, it detects existing cluster and joins as learner

mod bootstrap;
mod cluster;
mod config;

use anyhow::{Context, Result};
use common::{init_logging, Telemetry, TelemetryEvent};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tokio::time::sleep;
use tracing::{error, info, warn};

use bootstrap::{
    bootstrap_as_follower, bootstrap_as_leader, bootstrap_marker_present, check_existing_cluster,
    clean_stale_data, defrag_loop, local_liveness_watchdog, monitor_and_mark_bootstrap,
};
use cluster::{clear_directory, has_local_data, start_etcd};
use config::{get_bootstrap_leader, Config};

/// etcd panics on startup when a peer's commit index exceeds this member's last
/// log entry ("tocommit(N) is out of range [lastIndex(M)]" / "raft log corrupted,
/// truncated, or lost") — the local raft WAL lost entries it had acked to the
/// cluster, typically from an ungraceful kill mid-fsync. The process exits
/// non-zero and re-reads the same corrupt WAL on every restart, so retrying the
/// same data dir loops forever; the only recovery is to wipe and re-clone.
fn is_raft_corruption_line(line: &str) -> bool {
    (line.contains("tocommit(") && line.contains("is out of range [lastIndex("))
        || line.contains("Was the raft log corrupted, truncated, or lost?")
}

/// etcd refuses to serve when the local data dir carries a member identity the
/// cluster has removed ("the member has been permanently removed from the
/// cluster" / "data-dir used by this member must be removed"). The removal is
/// recorded in the peers' raft state, so restarting on the same dir fails
/// identically every time — the same forever-loop as WAL corruption, with the
/// same recovery (wipe, then re-join as a fresh learner). Observed 2026-08-26
/// on onyx-staging/etcd-2 and onyx-prod/etcd-3: exit 1 every retry while the
/// platform's sick-etcd self-heal redeployed the same volume to no effect.
fn is_removed_member_line(line: &str) -> bool {
    line.contains("the member has been permanently removed from the cluster")
        || line.contains("data-dir used by this member must be removed")
}

/// Whether an etcd exit ends supervision as a clean shutdown.
///
/// The exit CODE alone cannot decide this: etcd's graceful stop after
/// discovering its member was removed exits 0 whenever etcdmain's `<-stopped`
/// branch wins the shutdown race (`osutil.Exit(0)`, etcdmain/etcd.go) and 1
/// when a listener error wins — nondeterministic per start, both shapes
/// observed on the same node. Taking a 0-exit refusal as a clean exit ends
/// supervision with the data dir still orphaned: the container dies as a
/// SUCCESS zombie that no in-wrapper recovery ever reaches. A run that hit an
/// unrecoverable-data-dir line is therefore never clean, whatever the exit
/// code. (Raft corruption is a panic and cannot exit 0; it is gated the same
/// way so every detected class behaves identically.)
fn exit_is_clean(exited_ok: bool, was_corrupt: bool, was_removed_member: bool) -> bool {
    exited_ok && !was_corrupt && !was_removed_member
}

/// Publish the first stop signal the container runtime sends.
///
/// This binary is PID 1 in the etcd image (`ENTRYPOINT ["/entrypoint"]`), and
/// a container stop delivers exactly one signal, to PID 1. A PID 1 that has
/// installed no handler drops it — so until this listener existed every
/// `stop`/`restart` of an etcd member sat through the whole grace period and
/// ended in SIGKILL: no leadership hand-off, no clean raft close, a hard exit
/// on every redeploy (measured on the published image: 16s, exit 137, no
/// shutdown line in etcd's own log). The supervisor selects on the returned
/// receiver wherever it waits, forwards the stop to etcd, and exits with etcd's
/// own status.
fn spawn_stop_listener() -> watch::Receiver<Option<Signal>> {
    let (tx, rx) = watch::channel(None);
    tokio::spawn(async move {
        let (mut term, mut int, mut quit, mut hup) = match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
            signal(SignalKind::quit()),
            signal(SignalKind::hangup()),
        ) {
            (Ok(term), Ok(int), Ok(quit), Ok(hup)) => (term, int, quit, hup),
            _ => {
                error!("Failed to install stop-signal handlers; a stop will fall through to the runtime's kill");
                return;
            }
        };
        let sig = tokio::select! {
            _ = term.recv() => Signal::SIGTERM,
            _ = int.recv() => Signal::SIGINT,
            _ = quit.recv() => Signal::SIGQUIT,
            _ = hup.recv() => Signal::SIGHUP,
        };
        let _ = tx.send(Some(sig));
    });
    rx
}

/// Resolve once a stop has been requested. Never resolves when the listener
/// could not be installed, which leaves the supervisor's pre-existing
/// behaviour untouched.
async fn stop_requested(rx: &mut watch::Receiver<Option<Signal>>) -> Signal {
    loop {
        if let Some(sig) = *rx.borrow_and_update() {
            return sig;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Sleep, returning early with the signal if a stop is requested meanwhile.
async fn sleep_or_stop(
    duration: Duration,
    rx: &mut watch::Receiver<Option<Signal>>,
) -> Option<Signal> {
    tokio::select! {
        _ = sleep(duration) => None,
        sig = stop_requested(rx) => Some(sig),
    }
}

/// Exit code for a requested stop, mirroring etcd's own exit status.
///
/// etcd's interrupt handler runs its shutdown (leadership hand-off, "closed
/// etcd server") and then re-raises the very signal it received with the
/// default disposition, so a graceful stop ends as a death by the forwarded
/// signal — not as exit 0. That is the clean outcome here and is reported as
/// 0. Any other status is passed through: etcd's own code when it exited, the
/// conventional 128+N when some other signal killed it.
fn exit_code_for(code: Option<i32>, signal: Option<i32>, forwarded: i32) -> i32 {
    match (code, signal) {
        (Some(code), _) => code,
        (None, Some(signal)) if signal == forwarded => 0,
        (None, Some(signal)) => 128 + signal,
        (None, None) => 1,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_logging("etcd");

    let telemetry = Telemetry::from_env("etcd");
    let config = Config::from_env()?;

    fs::create_dir_all(&config.data_dir)
        .await
        .context("Failed to create data directory")?;

    clean_stale_data(&config, &telemetry).await?;

    let bootstrap_leader = get_bootstrap_leader(&config.initial_cluster)?;
    let is_leader = config.etcd_name == bootstrap_leader;

    info!(
        leader = %bootstrap_leader,
        node = %config.etcd_name,
        is_leader = is_leader,
        "Cluster bootstrap"
    );

    let mut stop_rx = spawn_stop_listener();

    let mut attempt = 1;
    while attempt <= config.max_retries {
        info!(attempt, max = config.max_retries, "Starting etcd");

        // Determine bootstrap parameters based on role. A stop that lands
        // while we are still negotiating (a follower can wait minutes for the
        // leader) has nothing to clean up: exit at once rather than letting the
        // runtime's grace period expire into SIGKILL.
        let bootstrap = async {
            if is_leader {
                bootstrap_as_leader(&config, &telemetry).await
            } else {
                bootstrap_as_follower(&config, &bootstrap_leader, &telemetry).await
            }
        };
        let bootstrap_result = tokio::select! {
            result = bootstrap => result,
            sig = stop_requested(&mut stop_rx) => {
                info!(signal = ?sig, "Stop requested before etcd started; exiting");
                return Ok(());
            }
        };

        let params = match bootstrap_result {
            Ok(Some(params)) => params,
            Ok(None) => {
                // Only send telemetry at milestones to reduce noise (60 retries → 4 events)
                if attempt == 1 || attempt == 10 || attempt == 30 || attempt == config.max_retries {
                    telemetry.send(TelemetryEvent::EtcdStartupFailed {
                        node: config.etcd_name.clone(),
                        attempt,
                        max_attempts: config.max_retries,
                        error: "Bootstrap params not ready".to_string(),
                    });
                }
                attempt += 1;
                if let Some(sig) = sleep_or_stop(config.retry_delay, &mut stop_rx).await {
                    info!(signal = ?sig, "Stop requested while waiting to retry; exiting");
                    return Ok(());
                }
                continue;
            }
            Err(e) => {
                // Always send telemetry for actual errors (not just "not ready")
                telemetry.send(TelemetryEvent::EtcdStartupFailed {
                    node: config.etcd_name.clone(),
                    attempt,
                    max_attempts: config.max_retries,
                    error: e.to_string(),
                });
                attempt += 1;
                if let Some(sig) = sleep_or_stop(config.retry_delay, &mut stop_rx).await {
                    info!(signal = ?sig, "Stop requested while waiting to retry; exiting");
                    return Ok(());
                }
                continue;
            }
        };

        let mut child = start_etcd(&params.initial_cluster, &params.initial_cluster_state).await?;
        info!(pid = ?child.id(), "etcd started");

        // Tee etcd's stderr to our own stderr (so container logs are unchanged)
        // while watching for the unrecoverable raft-log corruption panic. The
        // reader must keep draining or etcd would block on a full stderr pipe.
        let corruption_detected = Arc::new(AtomicBool::new(false));
        let removed_member_detected = Arc::new(AtomicBool::new(false));
        let stderr_reader = child.stderr.take().map(|stderr| {
            let corrupt_flag = Arc::clone(&corruption_detected);
            let removed_flag = Arc::clone(&removed_member_detected);
            tokio::spawn(async move {
                let mut stderr_out = tokio::io::stderr();
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if is_raft_corruption_line(&line) {
                        corrupt_flag.store(true, Ordering::Relaxed);
                    }
                    if is_removed_member_line(&line) {
                        removed_flag.store(true, Ordering::Relaxed);
                    }
                    // Tee to our stderr so etcd's logs still reach the container.
                    let _ = stderr_out.write_all(format!("{}\n", line).as_bytes()).await;
                }
            })
        });

        // Spawn monitoring task
        // Note: monitor_and_mark_bootstrap handles its own errors by calling exit(1)
        // directly, which ensures we crash and recover on fatal errors (promotion
        // exhaustion, health check errors). On restart, clean_stale_data() will
        // clear incomplete bootstrap data.
        let monitor_config = Config::from_env()?;
        let monitor_telemetry = telemetry.clone();
        let joined_as_learner = params.joined_as_learner;
        let monitor_handle = tokio::spawn(async move {
            monitor_and_mark_bootstrap(&monitor_config, joined_as_learner, monitor_telemetry).await
        });

        let defrag_config = Config::from_env()?;
        let defrag_telemetry = telemetry.clone();
        let defrag_handle =
            tokio::spawn(async move { defrag_loop(defrag_config, defrag_telemetry).await });

        // Long-lived watchdog over the LOCAL etcd endpoint: crashes the container if
        // this node's etcd stops serving while still running, so a wedged "zombie"
        // member (deployment SUCCESS, process not answering) becomes a CRASHED deploy
        // the platform restarts instead of a silent reduction in fault tolerance.
        let watchdog_config = Config::from_env()?;
        let watchdog_telemetry = telemetry.clone();
        let watchdog_handle = tokio::spawn(async move {
            local_liveness_watchdog(watchdog_config, watchdog_telemetry).await
        });

        let mut stop_signal = None;
        let status = tokio::select! {
            status = child.wait() => status?,
            sig = stop_requested(&mut stop_rx) => {
                // etcd handles SIGTERM itself: it hands leadership off when it
                // holds it, closes raft and its listeners, and exits 0. Every
                // stop signal is forwarded as SIGTERM — SIGQUIT would make the
                // Go runtime dump goroutines and die non-zero instead.
                info!(signal = ?sig, pid = ?child.id(), "Stop requested; forwarding SIGTERM to etcd for a graceful shutdown");
                stop_signal = Some(sig);
                // The liveness watchdog crashes the container when the local
                // endpoint stops answering — exactly what a graceful stop looks
                // like from outside. Cancel the side tasks before etcd goes
                // down so none of them turns a requested stop into an exit(1).
                monitor_handle.abort();
                defrag_handle.abort();
                watchdog_handle.abort();
                if let Some(pid) = child.id() {
                    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
                }
                child.wait().await?
            }
        };
        monitor_handle.abort();
        defrag_handle.abort();
        watchdog_handle.abort();
        // Drain the rest of etcd's stderr (the reader ends at pipe EOF on exit) so
        // the corruption flag reflects the whole run before we read it.
        if let Some(reader) = stderr_reader {
            let _ = reader.await;
        }
        let was_corrupt = corruption_detected.load(Ordering::Relaxed);
        let was_removed_member = removed_member_detected.load(Ordering::Relaxed);

        if let Some(sig) = stop_signal {
            // A requested stop ends supervision with etcd's own status — never
            // a retry, whatever the run's log said before the signal arrived.
            let code = exit_code_for(status.code(), status.signal(), Signal::SIGTERM as i32);
            info!(signal = ?sig, exit_code = code, "etcd stopped on request");
            std::process::exit(code);
        }

        if exit_is_clean(status.success(), was_corrupt, was_removed_member) {
            info!("etcd exited cleanly");
            return Ok(());
        }

        let exit_code = status.code();
        info!(exit_code = ?exit_code, "etcd exited");
        if status.success() {
            warn!("etcd exited 0 after an unrecoverable-data-dir refusal - staying up so recovery can run");
        }

        // Handle incomplete bootstrap
        let marker_exists = bootstrap_marker_present(&config.bootstrap_marker());
        let has_data = has_local_data(&config.data_dir).await?;
        if !marker_exists && has_data {
            info!("Bootstrap incomplete - cleaning data");
            match clear_directory(Path::new(&config.data_dir)).await {
                Ok(()) => {
                    telemetry.send(TelemetryEvent::EtcdDataCleared {
                        node: config.etcd_name.clone(),
                        reason: "incomplete bootstrap after etcd exit".to_string(),
                    });
                }
                Err(e) => {
                    telemetry.send(TelemetryEvent::ComponentError {
                        component: "etcd".to_string(),
                        error: e.to_string(),
                        context: "clearing data after incomplete bootstrap".to_string(),
                    });
                }
            }
        } else if marker_exists && (was_corrupt || was_removed_member) {
            // A fully-bootstrapped member whose data dir is unrecoverable — raft
            // WAL corrupt, or the member identity in the dir was removed from the
            // cluster — fails identically on every start, so retrying the same
            // data dir loops until max_retries → CRASHED.
            // etcd is built for this: a follower can be wiped and re-cloned as long
            // as the rest of the cluster has quorum. Only act when a healthy peer
            // cluster exists (never wipe the last copy of the data), then drop the
            // marker + wipe — the next iteration takes the existing volume-loss
            // recovery path (remove stale member → re-add as learner → re-sync).
            let reason = if was_corrupt {
                "raft log corrupted/truncated; quorum intact on peers"
            } else {
                "member removed from cluster; local data dir orphaned; quorum intact on peers"
            };
            match check_existing_cluster(&config.initial_cluster, &config.etcd_name).await {
                Ok(Some(_)) => {
                    warn!(reason, "etcd data dir unrecoverable with a healthy peer cluster - wiping data dir to re-clone as a fresh member");
                    // Clear data FIRST; remove marker only on success. If clear fails,
                    // marker + data are preserved and we retry detection next cycle. If
                    // clear succeeds but remove fails, the stale marker is left with an
                    // empty data dir — handle that on the next startup.
                    match clear_directory(Path::new(&config.data_dir)).await {
                        Ok(()) => {
                            let _ = fs::remove_file(&config.bootstrap_marker()).await;
                            telemetry.send(TelemetryEvent::EtcdDataDirWiped {
                                node: config.etcd_name.clone(),
                                reason: reason.to_string(),
                            });
                        }
                        Err(e) => {
                            telemetry.send(TelemetryEvent::ComponentError {
                                component: "etcd".to_string(),
                                error: e.to_string(),
                                context: "wiping unrecoverable etcd data dir".to_string(),
                            });
                        }
                    }
                }
                Ok(None) => {
                    // No healthy peer to re-clone from — wiping could destroy the
                    // only surviving copy. Preserve and keep retrying; this is a
                    // manual-intervention situation (the wider cluster is down too).
                    warn!(reason, "etcd data dir unrecoverable but NO healthy peer cluster - preserving data (manual intervention needed)");
                }
                Err(e) => {
                    warn!(error = %e, reason, "etcd data dir unrecoverable: failed to probe peers - preserving data this cycle");
                }
            }
        } else if marker_exists {
            info!("Bootstrap complete - preserving data");
        }

        attempt += 1;
        if attempt <= config.max_retries {
            info!(delay = ?config.retry_delay, "Retrying");
            if let Some(sig) = sleep_or_stop(config.retry_delay, &mut stop_rx).await {
                info!(signal = ?sig, "Stop requested while waiting to retry; exiting");
                return Ok(());
            }
        }
    }

    error!(attempts = config.max_retries, "Failed to start etcd");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::{exit_code_for, exit_is_clean, is_raft_corruption_line, is_removed_member_line};

    #[test]
    fn clean_exit_without_flags_ends_supervision() {
        assert!(exit_is_clean(true, false, false));
    }

    #[test]
    fn requested_stop_exit_code_mirrors_etcd() {
        const SIGTERM: i32 = 15;
        // etcd's graceful stop ends by re-raising the forwarded SIGTERM after
        // "closed etcd server" — the clean outcome, reported as 0.
        assert_eq!(exit_code_for(None, Some(SIGTERM), SIGTERM), 0);
        // An explicit exit code is passed through unchanged.
        assert_eq!(exit_code_for(Some(0), None, SIGTERM), 0);
        assert_eq!(exit_code_for(Some(2), None, SIGTERM), 2);
        // Killed by some OTHER signal: conventional 128+N (SIGKILL = 9 -> 137).
        assert_eq!(exit_code_for(None, Some(9), SIGTERM), 137);
        // Neither known: never report success we cannot vouch for.
        assert_eq!(exit_code_for(None, None, SIGTERM), 1);
    }

    #[test]
    fn removed_member_refusal_is_never_a_clean_exit() {
        // etcd v3.6 exits 0 on the graceful removed-member stop whenever
        // etcdmain's <-stopped branch wins the shutdown race (observed on
        // onyx-staging/etcd-2 alongside exit-1 runs of the same refusal).
        // Supervision must not end on it, or the node dies as a SUCCESS
        // zombie with the wipe recovery unreachable.
        assert!(!exit_is_clean(true, false, true));
        assert!(!exit_is_clean(false, false, true));
    }

    #[test]
    fn corrupt_run_is_never_a_clean_exit() {
        assert!(!exit_is_clean(true, true, false));
        assert!(!exit_is_clean(false, true, false));
    }

    #[test]
    fn nonzero_exit_is_not_clean() {
        assert!(!exit_is_clean(false, false, false));
    }

    #[test]
    fn matches_removed_member_refusals() {
        // The two shapes etcd prints when the local dir carries a removed
        // member's identity (observed onyx-staging/etcd-2, 2026-08-26).
        assert!(is_removed_member_line(
            "data-dir used by this member must be removed"
        ));
        assert!(is_removed_member_line(
            "the member has been permanently removed from the cluster"
        ));
    }

    #[test]
    fn removed_member_matcher_ignores_membership_churn_lines() {
        assert!(!is_removed_member_line("ignore already removed member"));
        assert!(!is_removed_member_line(
            "skipped attributes update of removed member"
        ));
        assert!(!is_removed_member_line(
            "removing member 83132ee335828269 from cluster"
        ));
    }

    #[test]
    fn matches_tocommit_out_of_range_panic() {
        assert!(is_raft_corruption_line(
            "panic: tocommit(7026531) is out of range [lastIndex(7026530)]. Was the raft log corrupted, truncated, or lost?"
        ));
    }

    #[test]
    fn matches_corrupted_log_phrase_alone() {
        assert!(is_raft_corruption_line(
            "Was the raft log corrupted, truncated, or lost?"
        ));
        // Without the interrogative prefix the substring does NOT match — guards
        // against a hypothetical diagnostic like "checking if raft log corrupted...".
        assert!(!is_raft_corruption_line(
            "checking if raft log corrupted, truncated, or lost"
        ));
    }

    #[test]
    fn ignores_unrelated_lines() {
        assert!(!is_raft_corruption_line(
            "etcd 19ef95a6820cd674 became follower at term 15"
        ));
        assert!(!is_raft_corruption_line("slow fdatasync took 2.9s"));
        // "tocommit" without the range phrase is a normal commit-advance log line.
        assert!(!is_raft_corruption_line(
            "raft.node: tocommit advanced to 12345"
        ));
    }
}
