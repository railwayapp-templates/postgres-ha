//! The `sli etcd` line: this member's report of the cluster's redundancy.
//!
//! Every 20 seconds the member asks its own `/health` and every configured
//! member's, and logs one line:
//!
//! `sli etcd healthy=1 members=3 members_healthy=3 quorum=1`
//!
//! `healthy` is this member serving; `members` the members configured in
//! ETCD_INITIAL_CLUSTER; `quorum` whether the healthy ones are a majority.
//! While any member is unhealthy it checks every second (logging on every
//! change and every 20 seconds, with `seconds_degraded`), and the first line
//! after recovery carries `recovery_ms`, the length of the degraded episode.
//!
//! etcd loss is a redundancy loss, not an availability one: Patroni runs with
//! `failsafe_mode`, so a primary keeps serving without the DCS; what is lost is
//! the ability to fail over. The uptime SLI reads these lines for its
//! redundancy verdict (ETCD_MEMBER_DOWN, ETCD_QUORUM_RISK).

use crate::config::{parse_initial_cluster, peer_to_client_url, Config};
use common::etcd_http_health;
use std::fmt;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{info, warn};

/// A line at least this often.
pub const LOG_PERIOD: Duration = Duration::from_secs(20);
/// Check cadence while every member is healthy.
pub const HEALTHY_CHECK: Duration = LOG_PERIOD;
/// Check cadence while any member is not.
pub const DEGRADED_CHECK: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Health {
    pub local_healthy: bool,
    pub members: usize,
    pub members_healthy: usize,
}

impl Health {
    pub fn quorum(&self) -> bool {
        self.members > 0 && self.members_healthy * 2 > self.members
    }

    pub fn degraded(&self) -> bool {
        !self.local_healthy || self.members_healthy < self.members
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub health: Health,
    pub seconds_degraded: u64,
    pub recovery: Option<Duration>,
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "sli etcd healthy={} members={} members_healthy={} quorum={}",
            u8::from(self.health.local_healthy),
            self.health.members,
            self.health.members_healthy,
            u8::from(self.health.quorum())
        )?;
        if self.health.degraded() {
            write!(f, " seconds_degraded={}", self.seconds_degraded)?;
        }
        if let Some(r) = self.recovery {
            write!(f, " recovery_ms={}", r.as_millis())?;
        }
        Ok(())
    }
}

/// Decides which checks become lines.
#[derive(Debug, Default)]
pub struct Reporter {
    last: Option<(Health, Instant)>,
    degraded_since: Option<Instant>,
}

impl Reporter {
    /// Feed one check made at `now`; returns the line to log, if any, and how
    /// long to wait before the next check.
    pub fn observe(&mut self, health: Health, now: Instant) -> (Option<Line>, Duration) {
        let mut recovery = None;
        match (health.degraded(), self.degraded_since) {
            (true, None) => self.degraded_since = Some(now),
            (false, Some(since)) => {
                recovery = Some(now.saturating_duration_since(since));
                self.degraded_since = None;
            }
            _ => {}
        }
        let due = match self.last {
            None => true,
            Some((prev, at)) => prev != health || now.saturating_duration_since(at) >= LOG_PERIOD,
        };
        let line = due.then(|| Line {
            health,
            seconds_degraded: self
                .degraded_since
                .map(|s| now.saturating_duration_since(s).as_secs())
                .unwrap_or(0),
            recovery,
        });
        if line.is_some() {
            self.last = Some((health, now));
        }
        let wait = if health.degraded() {
            DEGRADED_CHECK
        } else {
            HEALTHY_CHECK
        };
        (line, wait)
    }
}

/// The client endpoints of every configured member.
pub fn member_endpoints(initial_cluster: &str) -> Vec<String> {
    let mut v: Vec<String> = parse_initial_cluster(initial_cluster)
        .map(|m| m.values().map(|peer| peer_to_client_url(peer)).collect())
        .unwrap_or_default();
    v.sort();
    v
}

async fn check(local: &str, members: &[String]) -> Health {
    let local_healthy = etcd_http_health(local).await.unwrap_or(false);
    let answers = join_all(
        members
            .iter()
            .cloned()
            .map(|m| async move { etcd_http_health(&m).await.unwrap_or(false) }),
    )
    .await;
    Health {
        local_healthy,
        members: members.len(),
        members_healthy: answers.into_iter().filter(|ok| *ok).count(),
    }
}

/// Minimal join_all over tokio tasks (the crate does not depend on `futures`).
async fn join_all<F>(futs: impl Iterator<Item = F>) -> Vec<bool>
where
    F: std::future::Future<Output = bool> + Send + 'static,
{
    let handles: Vec<_> = futs.map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.unwrap_or(false));
    }
    out
}

/// Run for the life of the etcd child; the caller aborts it on stop.
pub async fn sli_loop(config: Config, local: &'static str) {
    let members = member_endpoints(&config.initial_cluster);
    if members.is_empty() {
        warn!("sli etcd: no usable ETCD_INITIAL_CLUSTER entry, not reporting");
        return;
    }
    let mut reporter = Reporter::default();
    loop {
        let health = check(local, &members).await;
        let (line, wait) = reporter.observe(health, Instant::now());
        if let Some(line) = line {
            info!("{line}");
        }
        sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: Health = Health {
        local_healthy: true,
        members: 3,
        members_healthy: 3,
    };
    const ONE_DOWN: Health = Health {
        local_healthy: true,
        members: 3,
        members_healthy: 2,
    };
    const NO_QUORUM: Health = Health {
        local_healthy: false,
        members: 3,
        members_healthy: 1,
    };

    #[test]
    fn full_line() {
        let line = Line {
            health: FULL,
            seconds_degraded: 0,
            recovery: None,
        };
        assert_eq!(
            line.to_string(),
            "sli etcd healthy=1 members=3 members_healthy=3 quorum=1"
        );
    }

    #[test]
    fn quorum_is_a_strict_majority() {
        assert!(ONE_DOWN.quorum());
        assert!(!NO_QUORUM.quorum());
        let two_of_four = Health {
            local_healthy: true,
            members: 4,
            members_healthy: 2,
        };
        assert!(!two_of_four.quorum());
        let none = Health {
            local_healthy: false,
            members: 0,
            members_healthy: 0,
        };
        assert!(!none.quorum());
    }

    #[test]
    fn healthy_logs_every_period_and_checks_at_that_cadence() {
        let mut r = Reporter::default();
        let t0 = Instant::now();
        let (line, wait) = r.observe(FULL, t0);
        assert!(line.is_some());
        assert_eq!(wait, HEALTHY_CHECK);
        let (line, _) = r.observe(FULL, t0 + Duration::from_secs(5));
        assert!(line.is_none());
        let (line, _) = r.observe(FULL, t0 + LOG_PERIOD);
        assert!(line.is_some());
    }

    #[test]
    fn degraded_checks_every_second_and_logs_on_change_and_period() {
        let mut r = Reporter::default();
        let t0 = Instant::now();
        r.observe(FULL, t0);
        let (line, wait) = r.observe(ONE_DOWN, t0 + Duration::from_secs(1));
        assert_eq!(wait, DEGRADED_CHECK);
        let line = line.expect("a change is logged");
        assert_eq!(
            line.to_string(),
            "sli etcd healthy=1 members=3 members_healthy=2 quorum=1 seconds_degraded=0"
        );
        // Same state a second later: no line.
        let (line, _) = r.observe(ONE_DOWN, t0 + Duration::from_secs(2));
        assert!(line.is_none());
        // Worse: logged at once.
        let (line, _) = r.observe(NO_QUORUM, t0 + Duration::from_secs(3));
        assert!(line
            .unwrap()
            .to_string()
            .contains("quorum=0 seconds_degraded=2"));
        // Period elapses in the same state: logged.
        let (line, _) = r.observe(NO_QUORUM, t0 + Duration::from_secs(23));
        assert!(line.unwrap().to_string().ends_with("seconds_degraded=22"));
    }

    #[test]
    fn recovery_is_logged_with_the_episode_length() {
        let mut r = Reporter::default();
        let t0 = Instant::now();
        r.observe(ONE_DOWN, t0);
        let (line, wait) = r.observe(FULL, t0 + Duration::from_millis(7_500));
        assert_eq!(wait, HEALTHY_CHECK);
        assert_eq!(
            line.unwrap().to_string(),
            "sli etcd healthy=1 members=3 members_healthy=3 quorum=1 recovery_ms=7500"
        );
    }

    #[test]
    fn endpoints_come_from_the_initial_cluster() {
        let e = member_endpoints(
            "etcd-2=http://etcd-2.railway.internal:2380,etcd-1=http://etcd-1.railway.internal:2380,etcd-3=http://:2380",
        );
        assert_eq!(
            e,
            vec![
                "http://etcd-1.railway.internal:2379".to_string(),
                "http://etcd-2.railway.internal:2379".to_string(),
            ]
        );
    }
}
