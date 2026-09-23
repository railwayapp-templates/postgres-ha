//! The `sli haproxy` line: this replica's own report of the customer path.
//!
//! Every 10 seconds the replica opens a Postgres handshake on its own port
//! 5432 (see `probe.rs`) and logs one line with the result and the backends it
//! routes to. After a failure it probes back to back, one attempt a second,
//! until a handshake succeeds, and logs the time that took as `rto_ms`: the
//! measured recovery of the path. While failing it still logs one line per
//! 10-second slot, so every slot of the minute carries this replica's verdict.
//!
//! The control plane reads these lines from ClickHouse (a per-service,
//! per-minute view): a slot is available when some replica of the service
//! logged ok in it, down when every line in it failed, unknown when there was
//! no line. The line keeps the `sli haproxy primary_up=` prefix the engine
//! already matches as a heartbeat.
//!
//! Probes run on 10-second wall-clock boundaries plus a per-replica offset in
//! [0, 4 s), so a probe and its line (at most 5 s later) land in the same slot,
//! and the fleet's lines spread over the slot instead of arriving at once.

use crate::probe::{self, FailReason, Outcome, ProbeResult};
use crate::signals;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::info;

/// One probe per this period while the path is healthy.
pub const PROBE_PERIOD: Duration = Duration::from_secs(10);
/// Gap between attempt starts while the path is failing.
pub const RETRY_GAP: Duration = Duration::from_secs(1);
/// A handshake with no answer in this long is a failure.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Offsets stay below PROBE_PERIOD - PROBE_TIMEOUT so a probe's line lands in
/// the slot the probe started in.
const MAX_OFFSET_MS: u64 = 4_000;
const WAIT_SLICE: Duration = Duration::from_millis(200);

/// What the stats page last said about the backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backends {
    pub primary_up: bool,
    pub replicas_up: usize,
    pub replicas_total: usize,
}

pub type SharedBackends = Arc<Mutex<Option<Backends>>>;

/// One `sli haproxy` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub backends: Backends,
    pub result: ProbeResult,
    /// Set on the first ok after a failure: first failed attempt's start to
    /// this success.
    pub rto: Option<Duration>,
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "sli haproxy primary_up={} replicas_up={} replicas_total={}",
            u8::from(self.backends.primary_up),
            self.backends.replicas_up,
            self.backends.replicas_total
        )?;
        match &self.result.outcome {
            Outcome::Ok => write!(f, " probe=ok")?,
            Outcome::Fail(reason) => {
                write!(f, " probe=fail reason={}", reason.token())?;
                if let FailReason::Error { sqlstate } = reason {
                    write!(f, " sqlstate={sqlstate}")?;
                }
            }
        }
        write!(f, " latency_ms={}", self.result.latency.as_millis())?;
        if let Some(rto) = self.rto {
            write!(f, " rto_ms={}", rto.as_millis())?;
        }
        Ok(())
    }
}

/// The 10-second slot a wall-clock instant falls in.
pub fn slot_of(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / PROBE_PERIOD.as_secs()
}

/// The next scheduled probe strictly after `now`: a slot boundary plus `offset`.
pub fn next_probe_at(now: SystemTime, offset: Duration) -> SystemTime {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let period = PROBE_PERIOD.as_millis() as u64;
    let now_ms = since.as_millis() as u64;
    let offset_ms = offset.as_millis() as u64 % period;
    let mut at = (now_ms / period) * period + offset_ms;
    if at <= now_ms {
        at += period;
    }
    UNIX_EPOCH + Duration::from_millis(at)
}

/// This replica's offset inside the slot, stable for the process.
pub fn replica_offset(identity: &str) -> Duration {
    let mut h = DefaultHasher::new();
    identity.hash(&mut h);
    Duration::from_millis(h.finish() % MAX_OFFSET_MS)
}

/// What the loop does after a probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Healthy: wait for the next scheduled slot.
    Scheduled,
    /// Failing: the next attempt starts RETRY_GAP after this one started.
    BackToBack,
}

/// Decides which probe results become lines.
#[derive(Debug, Default)]
pub struct Prober {
    failing_since: Option<Instant>,
    last_logged_slot: Option<u64>,
}

impl Prober {
    #[cfg(test)]
    pub fn is_failing(&self) -> bool {
        self.failing_since.is_some()
    }

    /// Feed one result. `started` is when the attempt began, `now` the wall
    /// clock at its end (the line's timestamp).
    pub fn observe(
        &mut self,
        backends: Backends,
        result: ProbeResult,
        started: Instant,
        now: SystemTime,
    ) -> (Option<Line>, Next) {
        let slot = slot_of(now);
        let (line, next) = match (result.is_ok(), self.failing_since) {
            (true, None) => (
                Some(Line {
                    backends,
                    result,
                    rto: None,
                }),
                Next::Scheduled,
            ),
            (true, Some(since)) => {
                self.failing_since = None;
                let rto = (started + result.latency).saturating_duration_since(since);
                (
                    Some(Line {
                        backends,
                        result,
                        rto: Some(rto),
                    }),
                    Next::Scheduled,
                )
            }
            (false, None) => {
                self.failing_since = Some(started);
                (
                    Some(Line {
                        backends,
                        result,
                        rto: None,
                    }),
                    Next::BackToBack,
                )
            }
            (false, Some(_)) => {
                let line = (self.last_logged_slot != Some(slot)).then_some(Line {
                    backends,
                    result,
                    rto: None,
                });
                (line, Next::BackToBack)
            }
        };
        if line.is_some() {
            self.last_logged_slot = Some(slot);
        }
        (line, next)
    }
}

/// Sleep until `until`, in short slices so a requested stop ends the wait.
/// Returns false when a stop was requested.
fn wait_until(until: SystemTime) -> bool {
    loop {
        if signals::stop_requested() {
            return false;
        }
        let left = match until.duration_since(SystemTime::now()) {
            Ok(d) if !d.is_zero() => d,
            _ => return true,
        };
        thread::sleep(left.min(WAIT_SLICE));
    }
}

/// Run the probe loop on its own thread for the life of the process. Probes
/// start once the stats page has been read once (the line carries the
/// backends) and stop when a stop has been relayed to haproxy: a replica
/// shutting down is not the path failing.
pub fn spawn(addr: SocketAddr, user: String, identity: String, backends: SharedBackends) {
    let offset = replica_offset(&identity);
    info!(
        %addr,
        offset_ms = offset.as_millis() as u64,
        "sli probe: handshake every {}s, no login",
        PROBE_PERIOD.as_secs()
    );
    thread::spawn(move || {
        let mut prober = Prober::default();
        let mut next_at = next_probe_at(SystemTime::now(), offset);
        loop {
            if !wait_until(next_at) {
                return;
            }
            let Some(current) = *backends.lock().unwrap_or_else(|p| p.into_inner()) else {
                next_at = next_probe_at(SystemTime::now(), offset);
                continue;
            };
            let started = Instant::now();
            let started_wall = SystemTime::now();
            let result = probe::handshake(addr, &user, PROBE_TIMEOUT);
            if signals::stop_requested() {
                return;
            }
            let (line, next) = prober.observe(current, result, started, SystemTime::now());
            if let Some(line) = line {
                info!("{line}");
            }
            next_at = match next {
                Next::Scheduled => next_probe_at(SystemTime::now(), offset),
                Next::BackToBack => started_wall + RETRY_GAP,
            };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: Backends = Backends {
        primary_up: true,
        replicas_up: 2,
        replicas_total: 2,
    };

    fn ok(ms: u64) -> ProbeResult {
        ProbeResult {
            outcome: Outcome::Ok,
            latency: Duration::from_millis(ms),
        }
    }

    fn fail(reason: FailReason, ms: u64) -> ProbeResult {
        ProbeResult {
            outcome: Outcome::Fail(reason),
            latency: Duration::from_millis(ms),
        }
    }

    fn wall(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn ok_line_keeps_the_heartbeat_prefix() {
        let line = Line {
            backends: B,
            result: ok(3),
            rto: None,
        };
        assert_eq!(
            line.to_string(),
            "sli haproxy primary_up=1 replicas_up=2 replicas_total=2 probe=ok latency_ms=3"
        );
    }

    #[test]
    fn error_line_carries_the_sqlstate() {
        let line = Line {
            backends: Backends {
                primary_up: true,
                replicas_up: 1,
                replicas_total: 2,
            },
            result: fail(
                FailReason::Error {
                    sqlstate: "53300".into(),
                },
                12,
            ),
            rto: None,
        };
        assert_eq!(
            line.to_string(),
            "sli haproxy primary_up=1 replicas_up=1 replicas_total=2 probe=fail reason=error sqlstate=53300 latency_ms=12"
        );
    }

    #[test]
    fn timeout_and_closed_lines_have_no_sqlstate() {
        for (reason, token) in [
            (FailReason::Timeout, "timeout"),
            (FailReason::Closed, "closed"),
            (FailReason::Connect, "connect"),
            (FailReason::Protocol, "protocol"),
        ] {
            let s = Line {
                backends: B,
                result: fail(reason, 5000),
                rto: None,
            }
            .to_string();
            assert!(
                s.contains(&format!(" probe=fail reason={token} latency_ms=5000")),
                "{s}"
            );
            assert!(!s.contains("sqlstate"), "{s}");
        }
    }

    #[test]
    fn healthy_probes_each_log_and_wait_for_the_next_slot() {
        let mut p = Prober::default();
        let t0 = Instant::now();
        for i in 0..3u64 {
            let (line, next) = p.observe(B, ok(2), t0, wall(1_000 + i * 10));
            assert!(line.is_some());
            assert_eq!(next, Next::Scheduled);
        }
    }

    #[test]
    fn a_failure_logs_at_once_then_retries_back_to_back_one_line_per_slot() {
        let mut p = Prober::default();
        let t0 = Instant::now();
        // First failure at wall 1000 (slot 100): logged, switch to back-to-back.
        let (line, next) = p.observe(B, fail(FailReason::Closed, 1), t0, wall(1_000));
        assert!(line.is_some());
        assert_eq!(next, Next::BackToBack);
        assert!(p.is_failing());
        // More failures inside slot 100: not logged.
        for s in 1..10u64 {
            let (line, next) = p.observe(B, fail(FailReason::Closed, 1), t0, wall(1_000 + s));
            assert!(line.is_none(), "second {s}");
            assert_eq!(next, Next::BackToBack);
        }
        // First failure in slot 101: logged.
        let (line, _) = p.observe(B, fail(FailReason::Timeout, 5_000), t0, wall(1_010));
        assert!(line.is_some());
        let (line, _) = p.observe(B, fail(FailReason::Timeout, 5_000), t0, wall(1_015));
        assert!(line.is_none());
    }

    #[test]
    fn recovery_logs_the_rto_from_the_first_failed_attempt() {
        let mut p = Prober::default();
        let first = Instant::now();
        p.observe(B, fail(FailReason::Closed, 4), first, wall(2_000));
        p.observe(
            B,
            fail(FailReason::Closed, 4),
            first + Duration::from_secs(1),
            wall(2_001),
        );
        let (line, next) = p.observe(B, ok(6), first + Duration::from_secs(12), wall(2_012));
        let line = line.expect("recovery is always logged");
        assert_eq!(next, Next::Scheduled);
        assert_eq!(line.rto, Some(Duration::from_millis(12_006)));
        assert!(line
            .to_string()
            .ends_with(" probe=ok latency_ms=6 rto_ms=12006"));
        assert!(!p.is_failing());
        // The next healthy probe carries no rto.
        let (line, _) = p.observe(B, ok(2), first + Duration::from_secs(20), wall(2_020));
        assert_eq!(line.unwrap().rto, None);
    }

    #[test]
    fn recovery_in_the_same_slot_as_a_logged_failure_is_still_logged() {
        let mut p = Prober::default();
        let t0 = Instant::now();
        p.observe(B, fail(FailReason::Closed, 1), t0, wall(3_000));
        let (line, _) = p.observe(B, ok(1), t0 + Duration::from_secs(1), wall(3_001));
        assert!(line.is_some());
    }

    #[test]
    fn schedule_is_the_next_boundary_plus_offset() {
        let off = Duration::from_millis(2_500);
        // 1000.0 s → 1002.5 s
        assert_eq!(
            next_probe_at(wall(1_000), off),
            UNIX_EPOCH + Duration::from_millis(1_002_500)
        );
        // 1002.5 s exactly → strictly after → 1012.5 s
        assert_eq!(
            next_probe_at(UNIX_EPOCH + Duration::from_millis(1_002_500), off),
            UNIX_EPOCH + Duration::from_millis(1_012_500)
        );
        // 1007 s → 1012.5 s
        assert_eq!(
            next_probe_at(wall(1_007), off),
            UNIX_EPOCH + Duration::from_millis(1_012_500)
        );
    }

    #[test]
    fn a_probe_and_its_line_share_a_slot() {
        // The worst case: the largest offset plus a full timeout stays inside
        // the slot the probe started in.
        for identity in ["a", "b", "haproxy-7f9c", "replica-2", ""] {
            let off = replica_offset(identity);
            assert!(off.as_millis() < MAX_OFFSET_MS as u128);
            let at = next_probe_at(wall(5_000), off);
            assert_eq!(slot_of(at), slot_of(at + PROBE_TIMEOUT));
        }
    }

    #[test]
    fn offset_is_stable_for_an_identity() {
        assert_eq!(replica_offset("x"), replica_offset("x"));
    }
}
