//! Wrapper script for Patroni-enabled PostgreSQL startup
//!
//! Validates volume mounts, checks PGDATA configuration, generates SSL certificates
//! if missing or expired, handles permission setup, and decides between Patroni HA
//! mode or standalone PostgreSQL mode based on the PATRONI_ENABLED flag.

use anyhow::{anyhow, Context, Result};
use common::{init_logging, ConfigExt, RailwayEnv, Telemetry, TelemetryEvent};
use postgres_patroni::{
    cert_expires_within, ensure_pg_stat_statements, is_patroni_enabled, is_valid_x509v3_cert,
    major_upgrade, pgdata, ssl_dir, sudo_command, volume_root, EXPECTED_VOLUME_MOUNT_PATH,
};
use nix::sys::signal::{SigHandler, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::Pid;
use std::env;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
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

    // Shared flock on the SAME lock file upgrade-job.sh takes exclusively for
    // its own run (see major_upgrade::take_volume_upgrade_lock). The fd — like
    // every fd Rust's std opens — is O_CLOEXEC, so this lock does NOT survive
    // an exec; what it buys each mode differs:
    //
    // - Patroni mode execs into patroni-runner below, releasing this lock at
    //   the handoff. That is fine BY DESIGN: the runner re-takes it first
    //   thing in async_main and holds it for the container's lifetime, and a
    //   job that wins the race in the gap makes the runner refuse boot —
    //   fail-stop in the safe direction.
    // - Standalone mode has no second binary: the branch below stays resident
    //   as the lock holder for as long as Postgres runs (it spawns
    //   docker-entrypoint as a CHILD, never execs — see the comment there).
    //
    // Taken before the mode fork, like the version guard below, so a job
    // holding the volume right now refuses BOTH modes with the same message.
    let _upgrade_volume_lock =
        match major_upgrade::take_volume_upgrade_lock(&volume_root(), &pgdata) {
        Ok(lock) => lock,
        Err(reason) => {
            error!("{reason}");
            telemetry.send(TelemetryEvent::MajorUpgradeBootRefused {
                node: String::env_or("PATRONI_NAME", "unknown"),
                reason,
            });
            std::process::exit(1);
        }
    };

    // Major-upgrade boot guard, before the mode fork so BOTH modes are
    // covered. Patroni mode re-checks in patroni-runner, but the standalone
    // branch below execs straight into docker-entrypoint, which would initdb
    // an ABSENT data directory — exactly the mid-swap state an in-flight
    // marker describes — and boot the wrong major without it. Fail-stop and
    // loud; a `reseed` marker passes (boot allowed by contract; patroni-runner
    // consumes it, and standalone Postgres refuses a cross-major data dir on
    // its own without touching the files). The image's major comes from the
    // installed server tree (see major_upgrade::image_major), so a stray
    // user-set PG_MAJOR service variable can't refuse a healthy boot.
    let image_major = major_upgrade::image_major();
    if let Some(reason) =
        major_upgrade::boot_refusal_reason(&volume_root(), &pgdata, image_major.as_deref())
    {
        error!("{reason}");
        telemetry.send(TelemetryEvent::MajorUpgradeBootRefused {
            node: String::env_or("PATRONI_NAME", "unknown"),
            reason,
        });
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

        // Re-apply the ssl settings when the certificate exists but
        // postgresql.conf doesn't reference it. The checks above are keyed on
        // the CERTIFICATE, which lives outside PGDATA and therefore survives
        // anything that replaces the data directory — a major upgrade
        // promotes a freshly initdb'd PGDATA, so postgresql.conf loses
        // `ssl = on` while server.crt is still right there. The result is a
        // database that silently comes back with SSL off, rejecting every
        // sslmode=require client. Keying this on the CONFIG instead self-heals
        // that and any other path that resets postgresql.conf (mirrors
        // postgres-ssl's wrapper.sh fix for the same bug).
        if Path::new(&postgres_conf_file).exists() && Path::new(&server_crt).exists() {
            let has_ssl_directive = std::fs::read_to_string(&postgres_conf_file)
                .map(|contents| postgresql_conf_has_ssl_directive(&contents))
                .unwrap_or(false);

            if !has_ssl_directive {
                info!(
                    "postgresql.conf has no ssl directive but certificates exist, re-applying..."
                );
                let ssl_conf = format!(
                    "ssl = on\nssl_cert_file = '{ssl_dir}/server.crt'\nssl_key_file = '{ssl_dir}/server.key'\nssl_ca_file = '{ssl_dir}/root.crt'\n"
                );
                let append_result = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&postgres_conf_file)
                    .and_then(|mut f| f.write_all(ssl_conf.as_bytes()));
                if let Err(e) = append_result {
                    error!(error = %e, "Failed to re-apply ssl settings to postgresql.conf");
                    std::process::exit(1);
                }
            }
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

        // Spawn docker-entrypoint as a CHILD and stay resident — never exec.
        // The volume-upgrade lock's fd is O_CLOEXEC (std opens every fd that
        // way), so an exec here would release the flock at the moment of
        // handoff and leave the entire life of standalone Postgres
        // unprotected against an upgrade job. Holding it from a process that
        // stays alive is the same design postgres-ssl's wrapper.sh documents
        // ("opened by THIS shell, which stays alive as PID 1 —
        // docker-entrypoint.sh below is a child, not an exec").
        //
        // Staying resident makes this process the container's long-lived
        // PID 1, so it also inherits every orphaned descendant; the
        // waitpid(-1) loop reaps them. Blanket reaping is safe here for the
        // same reason patroni_runner's mini-init argues: this process has
        // exactly one direct child left (every earlier subprocess was awaited
        // to completion above), so waitpid(-1) can only ever collect that
        // child or orphans. Terminal signals forward raw to the child
        // (async-signal-safe: atomic load + kill), and the child's exit
        // status is propagated as ours.
        let child = cmd
            .spawn()
            .context("Failed to spawn docker-entrypoint.sh")?;
        STANDALONE_CHILD.store(child.id() as i32, Ordering::Relaxed);
        for sig in [
            Signal::SIGTERM,
            Signal::SIGINT,
            Signal::SIGQUIT,
            Signal::SIGHUP,
        ] {
            // SAFETY: handler is async-signal-safe (atomic load + kill).
            unsafe {
                let _ = nix::sys::signal::signal(sig, SigHandler::Handler(standalone_forward));
            }
        }

        let child_pid = Pid::from_raw(child.id() as i32);
        loop {
            match waitpid(Pid::from_raw(-1), None) {
                Ok(WaitStatus::Exited(pid, code)) if pid == child_pid => {
                    std::process::exit(code)
                }
                Ok(WaitStatus::Signaled(pid, sig, _)) if pid == child_pid => {
                    std::process::exit(128 + sig as i32)
                }
                // An orphan reaped — the point of standing here.
                Ok(_) => {}
                Err(nix::errno::Errno::EINTR) => {}
                // No children at all: the real child is gone without us
                // seeing its status (should not happen; don't spin on it).
                Err(nix::errno::Errno::ECHILD) => std::process::exit(0),
                Err(e) => {
                    error!("standalone supervisor: waitpid failed: {e}; exiting");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// PID of the spawned docker-entrypoint child, for the standalone
/// supervisor's signal-forwarding handler. Written exactly once, before the
/// handlers are installed. Mirrors patroni_runner's MINI_INIT_CHILD.
static STANDALONE_CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn standalone_forward(sig: nix::libc::c_int) {
    let pid = STANDALONE_CHILD.load(Ordering::Relaxed);
    if pid > 0 {
        // Async-signal-safe: raw kill only.
        unsafe { nix::libc::kill(pid, sig) };
    }
}

/// True if `contents` has a top-level `ssl = ...` directive (any whitespace,
/// with or without the `=` spaced out). Deliberately does NOT match
/// `ssl_cert_file` / `ssl_key_file` / `ssl_ca_file` — a promoted, freshly
/// initdb'd postgresql.conf has none of the `ssl_*` lines either, but the
/// discriminating one for "did SSL survive" is the top-level toggle.
fn postgresql_conf_has_ssl_directive(contents: &str) -> bool {
    contents.lines().any(|line| {
        line.trim_start()
            .strip_prefix("ssl")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_directive_detected_when_present() {
        assert!(postgresql_conf_has_ssl_directive("ssl = on\nport = 5432"));
    }

    #[test]
    fn ssl_directive_detected_regardless_of_spacing() {
        assert!(postgresql_conf_has_ssl_directive("ssl=on"));
        assert!(postgresql_conf_has_ssl_directive("  ssl   =   on  "));
    }

    #[test]
    fn ssl_directive_absent_on_freshly_initdbd_conf() {
        assert!(!postgresql_conf_has_ssl_directive(
            "port = 5432\nmax_connections = 100\n"
        ));
    }

    #[test]
    fn ssl_cert_file_alone_does_not_count_as_the_toggle() {
        // A config could in principle carry ssl_cert_file/ssl_key_file
        // without the top-level `ssl = on` toggle actually being set —
        // that's not "SSL survived", it's SSL still off with stale paths.
        assert!(!postgresql_conf_has_ssl_directive(
            "ssl_cert_file = '/etc/ssl/server.crt'"
        ));
    }
}
