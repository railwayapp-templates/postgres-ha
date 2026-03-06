//! Patroni process monitoring
//!
//! Handles the monitoring loop, signal handling, health check management,
//! and timeline divergence auto-recovery.

use super::{check_health, is_start_failed, safe_reinitialize, Config};
use common::{Telemetry, TelemetryEvent};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use std::time::Duration;
use tokio::process::Child;
use tokio::signal::unix::{signal, SignalKind};
use tokio::time::sleep;
use tracing::{error, info, warn};

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
    //
    // Timeline divergence detection:
    // If health check fails and node is in "start failed" state for 3 consecutive
    // checks (15 seconds), we attempt safe reinitialize to recover from timeline
    // divergence caused by multiple failovers.
    info!(
        grace_period = config.startup_grace_period,
        max_timeout = config.max_startup_timeout,
        "Waiting for Patroni to initialize"
    );

    let mut startup_elapsed = 0u64;
    let mut start_failed_count = 0u32;
    const START_FAILED_THRESHOLD: u32 = 3; // 15 seconds of "start failed"

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
                startup_elapsed += 5;
                if check_health(config.health_check_timeout).await {
                    info!(elapsed = startup_elapsed, "Patroni healthy, starting monitoring");
                    start_failed_count = 0;
                    break;
                }

                // Check for "start failed" state (timeline divergence indicator)
                // Only check after grace period to give Patroni time to start
                if startup_elapsed >= config.startup_grace_period {
                    if is_start_failed(&config.name, config.health_check_timeout).await {
                        start_failed_count += 1;
                        warn!(
                            count = start_failed_count,
                            threshold = START_FAILED_THRESHOLD,
                            "Node in 'start failed' state - possible timeline divergence"
                        );

                        // After threshold consecutive failures, attempt safe reinitialize
                        if start_failed_count >= START_FAILED_THRESHOLD {
                            info!("Threshold reached, attempting safe reinitialize");

                            match safe_reinitialize(&config.name, config.health_check_timeout).await {
                                Ok((true, reason, local_tl, leader_tl)) => {
                                    info!(
                                        reason = %reason,
                                        local_timeline = ?local_tl,
                                        leader_timeline = ?leader_tl,
                                        "Reinitialize triggered successfully"
                                    );
                                    telemetry.send(TelemetryEvent::TimelineDivergenceRecovery {
                                        node: config.name.clone(),
                                        action: "reinitialize".to_string(),
                                        local_timeline: local_tl.unwrap_or(0),
                                        leader_timeline: leader_tl,
                                        reason,
                                    });
                                    // Reset counters and give time for reinit to complete
                                    start_failed_count = 0;
                                    startup_elapsed = 0; // Reset startup timer
                                }
                                Ok((false, reason, local_tl, leader_tl)) => {
                                    warn!(
                                        reason = %reason,
                                        local_timeline = ?local_tl,
                                        leader_timeline = ?leader_tl,
                                        "Reinitialize blocked by safety check"
                                    );
                                    telemetry.send(TelemetryEvent::TimelineDivergenceRecovery {
                                        node: config.name.clone(),
                                        action: "blocked".to_string(),
                                        local_timeline: local_tl.unwrap_or(0),
                                        leader_timeline: leader_tl,
                                        reason,
                                    });
                                    // Don't exit immediately - keep trying in case
                                    // the situation changes (e.g., leader comes up)
                                    start_failed_count = 0;
                                }
                                Err(e) => {
                                    error!(error = %e, "Failed to trigger reinitialize");
                                    telemetry.send(TelemetryEvent::TimelineDivergenceRecovery {
                                        node: config.name.clone(),
                                        action: "error".to_string(),
                                        local_timeline: 0,
                                        leader_timeline: None,
                                        reason: e,
                                    });
                                    start_failed_count = 0;
                                }
                            }
                        }
                    } else {
                        // Not in "start failed" state, reset counter
                        start_failed_count = 0;
                    }
                }

                // Check if we've exceeded max startup timeout
                if startup_elapsed >= config.max_startup_timeout {
                    error!(
                        elapsed = startup_elapsed,
                        max = config.max_startup_timeout,
                        "Patroni failed to become healthy within timeout - exiting for recovery"
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

                // Log progress after grace period
                if startup_elapsed >= config.startup_grace_period && startup_elapsed % 30 == 0 {
                    warn!(
                        elapsed = startup_elapsed,
                        max = config.max_startup_timeout,
                        "Still waiting for Patroni to become healthy"
                    );
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
