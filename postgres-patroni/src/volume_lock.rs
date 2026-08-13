//! Volume-lifetime runtime lock: at most one node container touches this
//! volume at a time.
//!
//! A redeploy (or an instance restart during host maintenance) can leave the
//! old and new containers briefly overlapping on the shared volume. Postgres
//! has its own postmaster.pid interlock, but overlap defeats it from both
//! sides: the boot-time stale-pid handling cannot see a postmaster in another
//! container's PID namespace, and an old postmaster's late graceful shutdown
//! unlinks the pid file the NEW postmaster just wrote — which the new one
//! treats as fatal on its once-per-minute recheck. Patroni restarts a
//! postgres that dies under it, so the wedge self-heals here, but every round
//! of it is avoidable downtime and a corruption window (two postmasters, one
//! PGDATA, however briefly).
//!
//! The runner therefore holds an exclusive `flock` on a file at the volume
//! root for the whole life of this process: a booting container waits for the
//! previous holder to exit before anything touches the data directory, and
//! fail-stops loudly on timeout so the restart policy retries the boot. The
//! kernel releases a `flock` when the last open description on the file is
//! closed, however the holder ended (graceful stop, SIGKILL, OOM).
//!
//! The lock file name is deliberately the SAME one the standalone
//! postgres-ssl image uses, so a standalone↔HA conversion whose containers
//! overlap on the volume serializes across the two images too.

use anyhow::{bail, Result};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};
use tracing::{info, warn};

const RUNTIME_LOCK_FILE: &str = ".railway-postgres-runtime.lock";
const DEFAULT_WAIT_SECS: u64 = 300;

/// Acquire the exclusive volume runtime lock, waiting up to
/// `RUNTIME_LOCK_WAIT_SECONDS` (default 300) for a previous holder.
///
/// Returns the lock to be bound for the caller's lifetime (mirrors
/// `major_upgrade::take_volume_upgrade_lock`'s shape). `Ok(None)` means the
/// guard degraded fail-open (unopenable lock file, non-EWOULDBLOCK flock
/// error, or the legacy PGDATA-at-the-volume-root first-init layout where a
/// lock file would make docker-entrypoint skip initdb). A timeout waiting on
/// a live holder fails CLOSED: two nodes on one PGDATA is the outcome that
/// must not happen.
pub fn acquire_volume_runtime_lock(
    volume_root: &str,
    pgdata: &str,
) -> Result<Option<Flock<File>>> {
    let pgdata_is_volume_root =
        pgdata.trim_end_matches('/') == volume_root.trim_end_matches('/');
    if pgdata_is_volume_root {
        let pg_version = format!("{}/PG_VERSION", pgdata.trim_end_matches('/'));
        if !Path::new(&pg_version).exists() {
            info!(
                pgdata = %pgdata,
                "PGDATA is the volume root and uninitialized — skipping the runtime lock \
                 so the lock file can't make docker-entrypoint skip initdb"
            );
            return Ok(None);
        }
    }

    let wait_secs = std::env::var("RUNTIME_LOCK_WAIT_SECONDS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_WAIT_SECS);
    acquire_with_wait(volume_root, wait_secs)
}

fn acquire_with_wait(volume_root: &str, wait_secs: u64) -> Result<Option<Flock<File>>> {
    let path = Path::new(volume_root).join(RUNTIME_LOCK_FILE);
    let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(err) => {
            warn!(
                path = %path.display(),
                error = %err,
                "could not open the runtime lock file; continuing without the volume lock"
            );
            return Ok(None);
        }
    };

    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    let mut waited = false;
    loop {
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => {
                if waited {
                    info!("previous container released the volume; continuing boot");
                }
                return Ok(Some(lock));
            }
            Err((returned, Errno::EWOULDBLOCK)) => {
                if !waited {
                    warn!(
                        wait_secs,
                        "another container still holds this volume (overlapping deploy); \
                         waiting for it to shut down"
                    );
                    waited = true;
                }
                if Instant::now() >= deadline {
                    bail!(
                        "previous container did not release the volume within {wait_secs}s; \
                         refusing to start against a volume another node may still be using"
                    );
                }
                file = returned;
                std::thread::sleep(Duration::from_secs(1));
            }
            Err((_returned, errno)) => {
                warn!(
                    path = %path.display(),
                    error = %errno,
                    "could not take the runtime lock; continuing without it"
                );
                return Ok(None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::{Flock, FlockArg};
    use std::fs::OpenOptions;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pg-rt-lock-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn acquires_when_uncontended_and_blocks_reacquisition() {
        let dir = tmp_dir("basic");
        let dir_str = dir.to_str().unwrap();

        let held = acquire_with_wait(dir_str, 1)
            .expect("uncontended acquire should succeed")
            .expect("should hold the lock");

        let probe = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(RUNTIME_LOCK_FILE))
            .unwrap();
        assert!(
            Flock::lock(probe, FlockArg::LockExclusiveNonblock).is_err(),
            "lock should still be held"
        );
        drop(held);
    }

    #[test]
    fn times_out_when_another_holder_keeps_the_lock() {
        let dir = tmp_dir("timeout");
        let dir_str = dir.to_str().unwrap();

        let holder = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(RUNTIME_LOCK_FILE))
            .unwrap();
        let held = Flock::lock(holder, FlockArg::LockExclusiveNonblock)
            .expect("holder should acquire first");

        let start = Instant::now();
        assert!(
            acquire_with_wait(dir_str, 2).is_err(),
            "contended acquire should time out"
        );
        assert!(start.elapsed() >= Duration::from_secs(2));

        drop(held);
        acquire_with_wait(dir_str, 1)
            .expect("acquire after release should succeed")
            .expect("should hold the lock");
    }

    #[test]
    fn legacy_volume_root_first_init_skips_the_lock() {
        let dir = tmp_dir("legacy");
        let dir_str = dir.to_str().unwrap();
        // PGDATA == volume root with no PG_VERSION: first init of the legacy
        // layout — the guard must abstain entirely (no lock file created).
        let result = acquire_volume_runtime_lock(dir_str, dir_str).unwrap();
        assert!(result.is_none());
        assert!(!dir.join(RUNTIME_LOCK_FILE).exists());
    }

    #[test]
    fn missing_directory_fails_open() {
        let dir = std::env::temp_dir().join(format!("pg-rt-lock-missing-{}", std::process::id()));
        let pgdata = dir.join("pgdata");
        let result =
            acquire_volume_runtime_lock(dir.to_str().unwrap(), pgdata.to_str().unwrap())
                .expect("unopenable lock file must fail open, not fail the boot");
        assert!(result.is_none());
    }
}
