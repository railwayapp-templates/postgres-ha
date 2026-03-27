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
use tokio::fs;
use tokio::time::sleep;
use tracing::{error, info};

use bootstrap::{
    bootstrap_as_follower, bootstrap_as_leader, clean_stale_data, defrag_loop,
    monitor_and_mark_bootstrap,
};
use cluster::{clear_directory, has_local_data, start_etcd};
use config::{get_bootstrap_leader, Config};

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
        let defrag_handle = tokio::spawn(async move {
            defrag_loop(defrag_config, defrag_telemetry).await
        });

        let status = child.wait().await?;
        monitor_handle.abort();
        defrag_handle.abort();

        if status.success() {
            info!("etcd exited cleanly");
            return Ok(());
        }

        let exit_code = status.code();
        info!(exit_code = ?exit_code, "etcd exited");

        // Handle incomplete bootstrap
        let marker_exists = Path::new(&config.bootstrap_marker()).exists();
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
