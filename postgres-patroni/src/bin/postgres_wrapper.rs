//! Wrapper script for Patroni-enabled PostgreSQL startup
//!
//! Validates volume mounts, checks PGDATA configuration, generates SSL certificates
//! if missing or expired, handles permission setup, and decides between Patroni HA
//! mode or standalone PostgreSQL mode based on the PATRONI_ENABLED flag.

use anyhow::{anyhow, Context, Result};
use common::{init_logging, ConfigExt, RailwayEnv, Telemetry, TelemetryEvent};
use postgres_patroni::{
    cert_expires_within, ensure_pg_stat_statements, is_patroni_enabled, is_valid_x509v3_cert,
    pgdata, ssl_dir, sudo_command, EXPECTED_VOLUME_MOUNT_PATH,
};
use std::env;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::time::timeout;
use tracing::{error, info, warn};

const INIT_SSL_SCRIPT: &str = "/docker-entrypoint-initdb.d/init-ssl.sh";

async fn run_init_ssl() -> Result<()> {
    let status = tokio::process::Command::new("bash")
        .arg(INIT_SSL_SCRIPT)
        .status()
        .await
        .context("Failed to run init-ssl script")?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("init-ssl script failed"))
    }
}

async fn check_and_generate_ssl(telemetry: &Telemetry) -> Result<()> {
    let ssl_dir = ssl_dir();
    let server_crt = format!("{}/server.crt", ssl_dir);

    if !Path::new(&server_crt).exists() {
        info!("SSL certificates missing, generating...");
        telemetry.send(TelemetryEvent::SslRenewed {
            node: String::env_or("PATRONI_NAME", "unknown"),
            reason: "missing".to_string(),
        });
        run_init_ssl().await?;
        return Ok(());
    }

    let is_valid = match is_valid_x509v3_cert(&server_crt) {
        Ok(valid) => valid,
        Err(e) => {
            warn!(error = %e, path = %server_crt, "Failed to validate certificate, will regenerate");
            false
        }
    };

    if !is_valid {
        info!("Invalid x509v3 certificate, regenerating...");
        telemetry.send(TelemetryEvent::SslRenewed {
            node: String::env_or("PATRONI_NAME", "unknown"),
            reason: "invalid".to_string(),
        });
        run_init_ssl().await?;
        return Ok(());
    }

    let expires_soon = match cert_expires_within(&server_crt, 2592000) {
        Ok(expires) => expires,
        Err(e) => {
            warn!(error = %e, path = %server_crt, "Failed to check certificate expiry, will regenerate");
            true
        }
    };

    if expires_soon {
        info!("Certificate expiring soon, regenerating...");
        telemetry.send(TelemetryEvent::SslRenewed {
            node: String::env_or("PATRONI_NAME", "unknown"),
            reason: "expiring".to_string(),
        });
        run_init_ssl().await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let _guard = init_logging("postgres-wrapper");

    let telemetry = Telemetry::from_env("postgres-ha");
    let pgdata = pgdata();
    let data_dir = EXPECTED_VOLUME_MOUNT_PATH;

    // Check if the Railway volume is mounted correctly
    if RailwayEnv::is_railway() {
        let volume_mount_path = RailwayEnv::volume_mount_path().unwrap_or_default();

        if volume_mount_path != EXPECTED_VOLUME_MOUNT_PATH {
            error!(
                expected = EXPECTED_VOLUME_MOUNT_PATH,
                got = %volume_mount_path,
                "Volume mount path mismatch"
            );
            telemetry.send(TelemetryEvent::ComponentError {
                component: "postgres-wrapper".to_string(),
                error: format!(
                    "Volume mounted to {} instead of {}",
                    volume_mount_path, EXPECTED_VOLUME_MOUNT_PATH
                ),
                context: "startup".to_string(),
            });
            std::process::exit(1);
        }
    }

    if !pgdata.starts_with(EXPECTED_VOLUME_MOUNT_PATH) {
        error!(
            expected = EXPECTED_VOLUME_MOUNT_PATH,
            pgdata = %pgdata,
            "PGDATA not in expected volume"
        );
        std::process::exit(1);
    }

    let postgres_conf_file = format!("{}/postgresql.conf", pgdata);

    if is_patroni_enabled() {
        info!("=== Patroni mode enabled ===");

        telemetry.send(TelemetryEvent::ComponentStarted {
            component: "postgres-wrapper".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        });

        if !Path::new(data_dir).exists() {
            info!("Creating data directory...");
            sudo_command(&["mkdir", "-p", data_dir]).await?;
        }

        info!("Setting data directory ownership...");
        let chown_result = timeout(
            Duration::from_secs(120),
            sudo_command(&["chown", "-R", "postgres:postgres", data_dir]),
        )
        .await;

        match chown_result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!(error = %e, "Failed to set ownership");
                std::process::exit(1);
            }
            Err(_) => {
                error!("chown timed out after 120s");
                std::process::exit(1);
            }
        }

        sudo_command(&["chmod", "700", data_dir]).await?;

        // Check for required passwords on fresh installs
        let pg_version_file = format!("{}/PG_VERSION", data_dir);
        if !Path::new(&pg_version_file).exists() {
            if env::var("POSTGRES_PASSWORD").is_err() || env::var("POSTGRES_PASSWORD")?.is_empty() {
                error!("POSTGRES_PASSWORD required for new database");
                std::process::exit(1);
            }
            if env::var("PATRONI_REPLICATION_PASSWORD").is_err()
                || env::var("PATRONI_REPLICATION_PASSWORD")?.is_empty()
            {
                error!("PATRONI_REPLICATION_PASSWORD required for HA mode");
                std::process::exit(1);
            }
        }

        check_and_generate_ssl(&telemetry).await?;

        info!("Starting Patroni runner...");
        let err = Command::new("gosu")
            .args(["postgres", "/usr/local/bin/patroni-runner"])
            .exec();

        Err(anyhow!("Failed to exec patroni-runner: {}", err))
    } else {
        let ssl_dir = ssl_dir();
        let server_crt = format!("{}/server.crt", ssl_dir);

        if Path::new(&server_crt).exists() {
            match is_valid_x509v3_cert(&server_crt) {
                Ok(false) => {
                    info!("Invalid certificate, regenerating...");
                    run_init_ssl().await?;
                }
                Err(e) => {
                    warn!(error = %e, path = %server_crt, "Failed to validate certificate, regenerating...");
                    run_init_ssl().await?;
                }
                Ok(true) => {}
            }
        }

        if Path::new(&server_crt).exists() {
            match cert_expires_within(&server_crt, 2592000) {
                Ok(true) => {
                    info!("Certificate expiring, regenerating...");
                    run_init_ssl().await?;
                }
                Err(e) => {
                    warn!(error = %e, path = %server_crt, "Failed to check certificate expiry, regenerating...");
                    run_init_ssl().await?;
                }
                Ok(false) => {}
            }
        }

        if Path::new(&postgres_conf_file).exists() && !Path::new(&server_crt).exists() {
            info!("Database missing certificate, generating...");
            run_init_ssl().await?;
        }

        // Ensure pg_stat_statements is configured for existing databases
        if let Err(e) = ensure_pg_stat_statements(&pgdata) {
            warn!(error = %e, "Failed to configure pg_stat_statements");
        }

        // If this was a replica (standby), promote it to primary for standalone mode.
        // This handles the case where a user downgrades from HA to standalone -
        // the standby.signal file tells PostgreSQL to start as a replica, but
        // without Patroni there's no primary to replicate from.
        let standby_signal = format!("{}/standby.signal", pgdata);
        if Path::new(&standby_signal).exists() {
            info!("Removing standby.signal to promote replica to standalone primary...");
            if let Err(e) = std::fs::remove_file(&standby_signal) {
                error!(error = %e, "Failed to remove standby.signal");
                std::process::exit(1);
            }
        }

        env::remove_var("PGHOST");
        env::remove_var("PGPORT");

        let args: Vec<String> = env::args().skip(1).collect();
        let log_to_stdout = bool::env_parse("LOG_TO_STDOUT", false);

        info!("Starting standalone PostgreSQL...");

        let mut cmd = Command::new("/usr/local/bin/docker-entrypoint.sh");
        cmd.args(&args);

        if log_to_stdout {
            cmd.stderr(Stdio::inherit());
        }

        let err = cmd.exec();
        Err(anyhow!("Failed to exec docker-entrypoint.sh: {}", err))
    }
}
