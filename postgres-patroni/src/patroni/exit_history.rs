//! Persistent wrapper-exit history: crash-loop backoff + exit diagnostics.
//!
//! Every recovery exit this wrapper takes (`patroni` died, startup stalled,
//! health checks exhausted) restarts the container — and when the underlying
//! fault is instant (a patroni that exits within a second of starting), the
//! container loop turns into a ~1.5s hammer: observed 2026-08-24, a
//! standalone node restarted patroni every ~1.5s for 17+ hours, emitting
//! 34,821 PROCESS_DIED telemetry events and burning a CPU doing nothing.
//! Nothing bounded the loop because the wrapper's memory dies with the
//! container.
//!
//! This module gives exits a memory that survives restarts — a small JSON
//! file on the VOLUME root (never inside pgdata: wipes and re-clones must not
//! erase it) — and uses it for two things:
//!
//!  1. **Backoff**: when the recent history shows a rapid crash loop
//!     (>= [`RAPID_LOOP_THRESHOLD`] exits within [`RAPID_WINDOW_SECS`]), the
//!     exit is delayed by an escalating sleep, turning a ~1.5s hammer into a
//!     minutes-paced retry. The fault stays visible (every exit still logs
//!     and sends telemetry) — it just stops being a stampede.
//!  2. **Diagnostics**: every recovery exit logs one structured line with the
//!     consecutive-exit count and a summary of pgdata's state, so a single
//!     log line answers "how long has this been looping and what does the
//!     data dir look like" without ssh.
//!
//! Best-effort by design: an unreadable/unwritable history file must never
//! block an exit that is already the failure path.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{error, warn};

/// Exits inside this window count toward the rapid-loop verdict.
const RAPID_WINDOW_SECS: u64 = 15 * 60;
/// This many exits inside [`RAPID_WINDOW_SECS`] is a rapid loop.
const RAPID_LOOP_THRESHOLD: usize = 5;
/// Backoff per extra rapid exit past the threshold, and its ceiling. At the
/// ceiling the loop paces at ~1 exit per 10 minutes instead of ~40/minute.
const BACKOFF_STEP_SECS: u64 = 60;
const BACKOFF_MAX_SECS: u64 = 600;
/// History retention: entries older than this are pruned, and the file is
/// capped so it can never grow unbounded.
const HISTORY_RETENTION_SECS: u64 = 24 * 60 * 60;
const HISTORY_MAX_ENTRIES: usize = 50;

const HISTORY_FILE: &str = ".wrapper-exit-history.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
struct ExitRecord {
    at_epoch_secs: u64,
    kind: String,
}

fn history_path(volume_root: &str) -> std::path::PathBuf {
    Path::new(volume_root).join(HISTORY_FILE)
}

fn load(volume_root: &str) -> Vec<ExitRecord> {
    let raw = match fs::read_to_string(history_path(volume_root)) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save(volume_root: &str, records: &[ExitRecord]) {
    match serde_json::to_string(records) {
        Ok(json) => {
            if let Err(e) = fs::write(history_path(volume_root), json) {
                warn!(error = %e, "could not persist wrapper exit history (backoff still applies this boot)");
            }
        }
        Err(e) => warn!(error = %e, "could not serialize wrapper exit history"),
    }
}

/// Prune to the retention window and entry cap, newest kept.
fn prune(mut records: Vec<ExitRecord>, now_epoch_secs: u64) -> Vec<ExitRecord> {
    records.retain(|r| now_epoch_secs.saturating_sub(r.at_epoch_secs) <= HISTORY_RETENTION_SECS);
    if records.len() > HISTORY_MAX_ENTRIES {
        let excess = records.len() - HISTORY_MAX_ENTRIES;
        records.drain(0..excess);
    }
    records
}

/// How many recorded exits (including the one being recorded) fall inside the
/// rapid window.
fn rapid_count(records: &[ExitRecord], now_epoch_secs: u64) -> usize {
    records
        .iter()
        .filter(|r| now_epoch_secs.saturating_sub(r.at_epoch_secs) <= RAPID_WINDOW_SECS)
        .count()
}

/// Escalating backoff for a rapid loop; zero below the threshold.
fn backoff_secs(rapid: usize) -> u64 {
    if rapid < RAPID_LOOP_THRESHOLD {
        return 0;
    }
    (BACKOFF_STEP_SECS * (rapid - RAPID_LOOP_THRESHOLD + 1) as u64).min(BACKOFF_MAX_SECS)
}

/// One-line summary of pgdata's state for the exit diagnostics: enough to
/// distinguish "never bootstrapped" from "torn clone" from "real database"
/// without a shell on the box.
pub fn pgdata_state_summary(data_dir: &str) -> String {
    let dir = Path::new(data_dir);
    if !dir.exists() {
        return "missing".to_string();
    }
    let entries = match fs::read_dir(dir) {
        Ok(it) => it.count(),
        Err(e) => return format!("unreadable ({e})"),
    };
    if entries == 0 {
        return "empty".to_string();
    }
    let has_pg_control = dir.join("global/pg_control").exists();
    if !has_pg_control {
        return format!("control-less ({entries} entries, no global/pg_control)");
    }
    let version = fs::read_to_string(dir.join("PG_VERSION"))
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| "?".to_string());
    format!("initialized (PG_VERSION {version}, {entries} entries)")
}

/// Record a recovery exit and, when the history shows a rapid crash loop,
/// sleep an escalating backoff BEFORE returning — the caller exits right
/// after. Returns the backoff applied (for tests/logging).
///
/// `rest_ever_answered`: whether Patroni's local REST answered at least once
/// this container lifetime (None when unknown/not applicable) — the
/// discriminator between "patroni wedged pre-REST" (the pre-REST-wedge class, where
/// only a reinit/wipe or a human helps) and "patroni was up and degraded".
pub async fn record_recovery_exit(
    volume_root: &str,
    data_dir: &str,
    kind: &str,
    rest_ever_answered: Option<bool>,
) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut records = prune(load(volume_root), now);
    records.push(ExitRecord {
        at_epoch_secs: now,
        kind: kind.to_string(),
    });
    let rapid = rapid_count(&records, now);
    let total_24h = records.len();
    save(volume_root, &records);

    let backoff = backoff_secs(rapid);
    error!(
        exit_kind = kind,
        exits_last_15m = rapid,
        exits_last_24h = total_24h,
        backoff_secs = backoff,
        pgdata = %pgdata_state_summary(data_dir),
        rest_ever_answered = ?rest_ever_answered,
        "wrapper recovery exit — persistent history and pgdata state attached"
    );
    if backoff > 0 {
        warn!(
            backoff_secs = backoff,
            "rapid crash loop detected — delaying the exit so the container loop paces in minutes, not seconds"
        );
        tokio::time::sleep(Duration::from_secs(backoff)).await;
    }
    backoff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(at: u64) -> ExitRecord {
        ExitRecord {
            at_epoch_secs: at,
            kind: "test".to_string(),
        }
    }

    #[test]
    fn backoff_is_zero_below_threshold() {
        assert_eq!(backoff_secs(0), 0);
        assert_eq!(backoff_secs(4), 0);
    }

    #[test]
    fn backoff_escalates_then_caps() {
        assert_eq!(backoff_secs(5), 60);
        assert_eq!(backoff_secs(6), 120);
        assert_eq!(backoff_secs(14), 600);
        assert_eq!(backoff_secs(50), 600);
    }

    #[test]
    fn rapid_count_ignores_old_entries() {
        let now = 100_000;
        let records = vec![
            rec(now - RAPID_WINDOW_SECS - 1), // outside the window
            rec(now - 10),
            rec(now - 5),
            rec(now),
        ];
        assert_eq!(rapid_count(&records, now), 3);
    }

    #[test]
    fn prune_drops_stale_and_caps_length() {
        let now = 1_000_000;
        let mut records: Vec<ExitRecord> = (0..(HISTORY_MAX_ENTRIES + 10))
            .map(|i| rec(now - i as u64))
            .collect();
        records.push(rec(now - HISTORY_RETENTION_SECS - 1)); // stale
        let pruned = prune(records, now);
        assert!(pruned.len() <= HISTORY_MAX_ENTRIES);
        assert!(pruned
            .iter()
            .all(|r| now - r.at_epoch_secs <= HISTORY_RETENTION_SECS));
    }

    #[test]
    fn a_slow_restart_loop_never_backs_off() {
        // The 300s startup-timeout loop paces at ~3 exits/15min — below the
        // rapid threshold on purpose: it is already bounded, and delaying it
        // would only slow a recovery that might succeed.
        let now = 100_000;
        let records: Vec<ExitRecord> = (0..3).map(|i| rec(now - i * 300)).collect();
        assert_eq!(backoff_secs(rapid_count(&records, now)), 0);
    }
}
