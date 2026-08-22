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
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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

    let mut attempt = 1;
    while attempt <= config.max_retries {
        info!(attempt, max = config.max_retries, "Starting etcd");

        // Determine bootstrap parameters based on role
        let bootstrap_result = if is_leader {
            bootstrap_as_leader(&config, &telemetry).await
        } else {
            bootstrap_as_follower(&config, &bootstrap_leader, &telemetry).await
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
                sleep(config.retry_delay).await;
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
                sleep(config.retry_delay).await;
                continue;
            }
        };

        let mut child = start_etcd(&params.initial_cluster, &params.initial_cluster_state).await?;
        info!(pid = ?child.id(), "etcd started");

        // Tee etcd's stderr to our own stderr (so container logs are unchanged)
        // while watching for the unrecoverable raft-log corruption panic. The
        // reader must keep draining or etcd would block on a full stderr pipe.
        let corruption_detected = Arc::new(AtomicBool::new(false));
        let stderr_reader = child.stderr.take().map(|stderr| {
            let flag = Arc::clone(&corruption_detected);
            tokio::spawn(async move {
                let mut stderr_out = tokio::io::stderr();
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if is_raft_corruption_line(&line) {
                        flag.store(true, Ordering::Relaxed);
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

        let status = child.wait().await?;
        monitor_handle.abort();
        defrag_handle.abort();
        watchdog_handle.abort();
        // Drain the rest of etcd's stderr (the reader ends at pipe EOF on exit) so
        // the corruption flag reflects the whole run before we read it.
        if let Some(reader) = stderr_reader {
            let _ = reader.await;
        }
        let was_corrupt = corruption_detected.load(Ordering::Relaxed);

        if status.success() {
            info!("etcd exited cleanly");
            return Ok(());
        }

        let exit_code = status.code();
        info!(exit_code = ?exit_code, "etcd exited");

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
        } else if marker_exists && was_corrupt {
            // A fully-bootstrapped member whose raft WAL is corrupt panics on every
            // start, so retrying the same data dir loops until max_retries → CRASHED.
            // etcd is built for this: a follower can be wiped and re-cloned as long
            // as the rest of the cluster has quorum. Only act when a healthy peer
            // cluster exists (never wipe the last copy of the data), then drop the
            // marker + wipe — the next iteration takes the existing volume-loss
            // recovery path (remove stale member → re-add as learner → re-sync).
            match check_existing_cluster(&config.initial_cluster, &config.etcd_name).await {
                Ok(Some(_)) => {
                    warn!("etcd raft log corruption with a healthy peer cluster - wiping data dir to re-clone as a fresh member");
                    // Clear data FIRST; remove marker only on success. If clear fails,
                    // marker + data are preserved and we retry detection next cycle. If
                    // clear succeeds but remove fails, the stale marker is left with an
                    // empty data dir — handle that on the next startup.
                    match clear_directory(Path::new(&config.data_dir)).await {
                        Ok(()) => {
                            let _ = fs::remove_file(&config.bootstrap_marker()).await;
                            telemetry.send(TelemetryEvent::EtcdDataDirWiped {
                                node: config.etcd_name.clone(),
                                reason: "raft log corrupted/truncated; quorum intact on peers"
                                    .to_string(),
                            });
                        }
                        Err(e) => {
                            telemetry.send(TelemetryEvent::ComponentError {
                                component: "etcd".to_string(),
                                error: e.to_string(),
                                context: "wiping corrupt etcd data dir".to_string(),
                            });
                        }
                    }
                }
                Ok(None) => {
                    // No healthy peer to re-clone from — wiping could destroy the
                    // only surviving copy. Preserve and keep retrying; this is a
                    // manual-intervention situation (the wider cluster is down too).
                    warn!("etcd raft log corruption but NO healthy peer cluster - preserving data (manual intervention needed)");
                }
                Err(e) => {
                    warn!(error = %e, "etcd raft log corruption: failed to probe peers - preserving data this cycle");
                }
            }
        } else if marker_exists {
            info!("Bootstrap complete - preserving data");
        }

        attempt += 1;
        if attempt <= config.max_retries {
            info!(delay = ?config.retry_delay, "Retrying");
            sleep(config.retry_delay).await;
        }
    }

    error!(attempts = config.max_retries, "Failed to start etcd");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::is_raft_corruption_line;

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
