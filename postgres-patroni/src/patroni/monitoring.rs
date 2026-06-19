//! Patroni process monitoring
//!
//! Handles the monitoring loop, signal handling, and health check management.

use super::{check_health, Config};
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
    // exit while volume usage is NOT growing: any growth is a clone/replay in
    // flight (data files being laid down, or post-clone WAL streaming into
    // pg_wal + spool) and resets the clock, while a genuinely stalled startup
    // (no progress for max_startup_timeout) still exits for recovery.
    let mut startup_elapsed = 0u64;
    let mut last_volume_used = volume_used_bytes(&config.data_dir);
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
                let progressing = used > last_volume_used;
                last_volume_used = used;

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
                                "Startup making progress (volume usage growing); resetting startup timeout"
                            );
                        }
                        startup_elapsed = 0;
                    }
                    StartupTick::Stalled => {
                        error!(
                            elapsed_without_progress = startup_elapsed,
                            max = config.max_startup_timeout,
                            "Patroni not healthy and no clone/startup progress (volume usage flat) within timeout - exiting for recovery"
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
/// Used as the startup "is the clone making progress" signal: a clone in flight
/// grows the volume — pg_basebackup writing data files, and (post-clone) the WAL
/// receiver streaming segments into pg_wal + replay churning the spool. Keying
/// on whole-volume usage rather than a recursive pgdata size means a large
/// replica still reads as "progressing" while it catches up after the base
/// backup, not just while files are first laid down — and it costs one syscall
/// instead of stat'ing every relation file every 5s. Best-effort: a missing
/// path or `statvfs` error returns 0 (the next successful read then registers
/// as growth, i.e. progress).
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
