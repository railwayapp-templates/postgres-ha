//! Replica self-heal supervisor for stuck-postgres states that Patroni
//! does not recover from on its own.
//!
//! ## What Patroni already handles (we do NOT duplicate)
//! - **Timeline divergence**: `remove_data_directory_on_diverged_timelines: true`
//!   in `yaml.rs` makes Patroni wipe pgdata and re-clone when a replica's
//!   timeline is less than the leader's.
//! - **pg_rewind failure**: `remove_data_directory_on_rewind_failure: true`
//!   handles the case where pg_rewind itself errors out.
//! - **Slot-pinned streaming**: `use_slots: true` keeps the leader from
//!   rotating WAL while a connected replica is behind.
//!
//! ## The gap this module fills
//! When a former leader is demoted (e.g., after a DCS outage), Patroni
//! runs `pg_rewind` to align it with the new leader's history. If
//! `pg_rewind` **succeeds** but the new leader has since rotated WAL past
//! the rewind point, the demoted node enters a crash loop:
//!
//! ```text
//! LOG:   started streaming WAL from primary at <LSN> on timeline <N>
//! FATAL: could not receive data from WAL stream:
//!        ERROR: requested WAL segment <X> has already been removed
//! ```
//!
//! Patroni's view stays "running, secondary, following leader" — none of
//! the three built-in recovery paths above fire, and the node sits in a
//! restart loop forever. The kl7q incident on Brava staging
//! (2026-05-12 → 2026-05-13) is the canonical instance: postgres-1 was
//! the leader, lost DCS, was demoted, pg_rewind succeeded, then
//! WAL-too-old on every streaming attempt for 13 days.
//!
//! ## Detection
//! We don't scrape postgres logs (Patroni-managed Postgres writes to
//! stderr, not to a file we can tail). Instead we poll Patroni REST
//! `/patroni` and watch `postmaster_start_time`. Each Postgres restart
//! advances that timestamp; a stable postgres holds it constant. Three
//! or more distinct values inside `recent_window_secs` is a crash loop.
//!
//! ## Action
//! `POST /reinitialize {"force": true}` on the local Patroni REST API.
//! Patroni wipes pgdata and runs a full pg_basebackup from the leader.
//! Bypasses pg_rewind entirely — the right move when rewind isn't the
//! problem but its aftermath is.
//!
//! ## Safety
//! Enforced by `decide_self_heal` and unit-tested:
//! 1. Never act on a `Leader` — wiping a primary is destructive.
//! 2. Never act unless the leader is reachable — guarantees a clone source.
//! 3. Respect `action_backoff_secs` between attempts on the same node.
//! 4. Cap at `max_attempts_per_hour`; beyond that emit `SelfHealGaveUp`
//!    and stop. Something deeper is wrong and humans should look.
//!
//! ## State persistence
//! `<volume_root>/.self_heal_state` (key=value lines, not in pgdata so it
//! survives the reinit wipe). Carries `last_action_at` and a rolling
//! action timestamp history so backoff and the per-hour cap work across
//! container restarts.
//!
//! ## Supervisor
//! Same shape as `backup_watcher::spawn`: outer respawn loop wraps the
//! main loop in `tokio::task::spawn` so a panic surfaces as
//! `JoinError::is_panic()` instead of aborting the host. 5s respawn
//! delay.

use anyhow::Result;
use common::{Telemetry, TelemetryEvent};
use serde::Deserialize;
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use tracing::{info, warn};

const STATE_FILENAME: &str = ".self_heal_state";
const PATRONI_PATRONI_URL: &str = "http://localhost:8008/patroni";
const PATRONI_CLUSTER_URL: &str = "http://localhost:8008/cluster";
const PATRONI_REINIT_URL: &str = "http://localhost:8008/reinitialize";

const DEFAULT_POLL_SECONDS: u64 = 10;
const DEFAULT_CRASH_LOOP_THRESHOLD: u32 = 3;
const DEFAULT_RECENT_WINDOW_SECONDS: u64 = 60;
const DEFAULT_ACTION_BACKOFF_SECONDS: u64 = 600;
const DEFAULT_MAX_ATTEMPTS_PER_HOUR: u32 = 5;
const DEFAULT_LEADER_HEALTH_TIMEOUT_SECONDS: u64 = 3;

// ====================================================================
// Public types
// ====================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Leader,
    Replica,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Thresholds {
    pub crash_loop_threshold: u32,
    pub recent_window_secs: u64,
    pub action_backoff_secs: u64,
    pub max_attempts_per_hour: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            crash_loop_threshold: DEFAULT_CRASH_LOOP_THRESHOLD,
            recent_window_secs: DEFAULT_RECENT_WINDOW_SECONDS,
            action_backoff_secs: DEFAULT_ACTION_BACKOFF_SECONDS,
            max_attempts_per_hour: DEFAULT_MAX_ATTEMPTS_PER_HOUR,
        }
    }
}

/// Plain-data snapshot built by the orchestrator per iteration. No I/O
/// happens inside `decide_self_heal`.
#[derive(Debug, Clone)]
pub struct SelfHealInputs {
    pub now: i64,
    pub role: Role,
    pub leader_reachable: bool,
    pub patroni_state: String,
    pub pg_starts_in_window: u32,
    pub last_action_at: Option<i64>,
    pub action_attempts_in_window: u32,
    pub recovery_seen_after_action: bool,
    pub thresholds: Thresholds,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelfHealAction {
    NoOp,
    Wait,
    Reinitialize {
        reason: String,
        attempt: u32,
    },
    EmitRecovered {
        recovered_in_secs: u64,
        attempts: u32,
    },
    EmitGaveUp {
        last_reason: String,
        attempts: u32,
    },
}

// ====================================================================
// Pure decision function
// ====================================================================

/// Pure (zero-I/O) decision function. All inputs come from the
/// `SelfHealInputs` snapshot; the orchestrator dispatches on the
/// returned action.
pub fn decide_self_heal(s: &SelfHealInputs) -> SelfHealAction {
    // Safety #1: never wipe a leader.
    if matches!(s.role, Role::Leader) {
        return SelfHealAction::NoOp;
    }

    // Recovery transition: previously stuck, now healthy.
    if s.recovery_seen_after_action {
        let recovered_in_secs = s
            .last_action_at
            .map(|t| (s.now.saturating_sub(t)).max(0) as u64)
            .unwrap_or(0);
        return SelfHealAction::EmitRecovered {
            recovered_in_secs,
            attempts: s.action_attempts_in_window,
        };
    }

    // Escalation cap: stop bashing it; emit a giveup once.
    if s.action_attempts_in_window >= s.thresholds.max_attempts_per_hour {
        return SelfHealAction::EmitGaveUp {
            last_reason: "max_attempts_per_hour".into(),
            attempts: s.action_attempts_in_window,
        };
    }

    // Backoff: respect minimum interval between actions.
    if let Some(t) = s.last_action_at {
        let elapsed = s.now.saturating_sub(t).max(0) as u64;
        if elapsed < s.thresholds.action_backoff_secs {
            return SelfHealAction::Wait;
        }
    }

    // Safety #2: must have a leader to clone from.
    if !s.leader_reachable {
        return SelfHealAction::Wait;
    }

    let patroni_says_failed = s.patroni_state == "start failed";
    let is_crash_loop = s.pg_starts_in_window >= s.thresholds.crash_loop_threshold;

    if patroni_says_failed || is_crash_loop {
        let reason = if patroni_says_failed {
            "patroni_start_failed".to_string()
        } else {
            format!(
                "postgres_crash_loop:{}_restarts_in_{}s",
                s.pg_starts_in_window, s.thresholds.recent_window_secs
            )
        };
        return SelfHealAction::Reinitialize {
            reason,
            attempt: s.action_attempts_in_window + 1,
        };
    }

    SelfHealAction::NoOp
}

// ====================================================================
// Supervisor (spawn + respawn loop)
// ====================================================================

/// Spawn the self-heal watcher as a long-running background task.
/// Honors `SELF_HEAL_DISABLED=1` as an operator kill switch.
///
/// Same shape as [`backup_watcher::spawn`]: an outer respawn loop wraps
/// the main loop in `tokio::task::spawn` so a panic surfaces as
/// `JoinError::is_panic()` instead of aborting patroni-runner. 5s sleep
/// between respawns prevents CPU burn on rapid failures.
pub fn spawn(volume_root: String, telemetry: Telemetry) {
    if env::var("SELF_HEAL_DISABLED").ok().as_deref() == Some("1") {
        info!("self-heal: SELF_HEAL_DISABLED=1, watcher inactive");
        return;
    }

    let cfg = WatcherConfig::from_env();
    info!(
        poll_secs = cfg.poll_secs,
        crash_loop_threshold = cfg.thresholds.crash_loop_threshold,
        recent_window_secs = cfg.thresholds.recent_window_secs,
        action_backoff_secs = cfg.thresholds.action_backoff_secs,
        max_attempts_per_hour = cfg.thresholds.max_attempts_per_hour,
        volume_root = %volume_root,
        "self-heal: starting watcher"
    );

    tokio::spawn(async move {
        loop {
            let vr = volume_root.clone();
            let t = telemetry.clone();
            let c = cfg.clone();
            let h = tokio::task::spawn(async move { run(vr, t, c).await });
            match h.await {
                Ok(Ok(())) => warn!("self-heal: run loop returned cleanly — respawning in 5s"),
                Ok(Err(e)) => warn!(error = %e, "self-heal: run loop errored — respawning in 5s"),
                Err(e) if e.is_panic() => {
                    warn!(panic = ?e, "self-heal: run loop panicked — respawning in 5s")
                }
                Err(e) => warn!(error = %e, "self-heal: join error — respawning in 5s"),
            }
            sleep(Duration::from_secs(5)).await;
        }
    });
}

#[derive(Debug, Clone)]
struct WatcherConfig {
    poll_secs: u64,
    leader_health_timeout_secs: u64,
    thresholds: Thresholds,
}

impl WatcherConfig {
    fn from_env() -> Self {
        Self {
            poll_secs: env_u64("SELF_HEAL_POLL_SECONDS", DEFAULT_POLL_SECONDS),
            leader_health_timeout_secs: env_u64(
                "SELF_HEAL_LEADER_HEALTH_TIMEOUT_SECONDS",
                DEFAULT_LEADER_HEALTH_TIMEOUT_SECONDS,
            ),
            thresholds: Thresholds {
                crash_loop_threshold: env_u32(
                    "SELF_HEAL_CRASH_LOOP_THRESHOLD",
                    DEFAULT_CRASH_LOOP_THRESHOLD,
                ),
                recent_window_secs: env_u64(
                    "SELF_HEAL_RECENT_WINDOW_SECONDS",
                    DEFAULT_RECENT_WINDOW_SECONDS,
                ),
                action_backoff_secs: env_u64(
                    "SELF_HEAL_ACTION_BACKOFF_SECONDS",
                    DEFAULT_ACTION_BACKOFF_SECONDS,
                ),
                max_attempts_per_hour: env_u32(
                    "SELF_HEAL_MAX_ATTEMPTS_PER_HOUR",
                    DEFAULT_MAX_ATTEMPTS_PER_HOUR,
                ),
            },
        }
    }
}

fn env_u64(k: &str, default: u64) -> u64 {
    env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_u32(k: &str, default: u32) -> u32 {
    env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

// ====================================================================
// Run loop (orchestrator)
// ====================================================================

async fn run(volume_root: String, telemetry: Telemetry, cfg: WatcherConfig) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;

    let state_path = format!("{volume_root}/{STATE_FILENAME}");
    // Sliding window of recent postmaster_start_time observations.
    let mut starts_seen: VecDeque<(i64, String)> = VecDeque::new();
    // "We acted recently and now postgres is stable" → emit Recovered once.
    // Rebuilt from disk on startup so a container restart between reinit
    // and stabilization still emits SelfHealRecovered when postgres comes
    // back healthy.
    let mut action_pending_recovery: Option<i64> =
        read_state_field(&state_path, "last_action_at").and_then(|s| s.parse::<i64>().ok());
    // Dedupe SelfHealGaveUp: the decide function returns EmitGaveUp on
    // every iteration while the cap is tripped (up to ~1h until the
    // oldest attempt ages out). Emit telemetry once per breach.
    let mut gave_up_emitted = false;

    loop {
        if let Err(e) = iteration(
            &client,
            &cfg,
            &state_path,
            &mut starts_seen,
            &mut action_pending_recovery,
            &mut gave_up_emitted,
            &telemetry,
        )
        .await
        {
            warn!(error = %e, "self-heal: iteration errored (continuing)");
        }
        sleep(Duration::from_secs(cfg.poll_secs)).await;
    }
}

async fn iteration(
    client: &reqwest::Client,
    cfg: &WatcherConfig,
    state_path: &str,
    starts_seen: &mut VecDeque<(i64, String)>,
    action_pending_recovery: &mut Option<i64>,
    gave_up_emitted: &mut bool,
    telemetry: &Telemetry,
) -> Result<()> {
    let now = now_epoch();

    // 1. Local Patroni status: role, state, postmaster_start_time.
    let local = match fetch_local_patroni(client).await {
        Ok(p) => p,
        Err(e) => {
            // Patroni REST not yet up (startup) or wedged. Skip silently.
            warn!(error = %e, "self-heal: /patroni unreachable");
            return Ok(());
        }
    };

    let role = parse_role(local.role.as_deref());
    if matches!(role, Role::Leader) {
        // Leaders are never acted on. Skip the rest of the work.
        return Ok(());
    }

    // 2. Track postmaster_start_time changes to detect crash loops.
    if let Some(ts) = local.postmaster_start_time.as_ref() {
        // Push if newest entry differs from this observation.
        let push = match starts_seen.back() {
            Some((_, last)) => last != ts,
            None => true,
        };
        if push {
            starts_seen.push_back((now, ts.clone()));
        }
    }
    // Trim entries older than the window.
    while let Some((t, _)) = starts_seen.front() {
        if (now.saturating_sub(*t) as u64) > cfg.thresholds.recent_window_secs {
            starts_seen.pop_front();
        } else {
            break;
        }
    }
    let pg_starts_in_window = starts_seen.len() as u32;

    // 3. Is the leader reachable (so a reinit has a clone source)?
    let leader_reachable = check_leader_reachable(client, cfg.leader_health_timeout_secs).await;

    // 4. Persistent counters.
    let last_action_at =
        read_state_field(state_path, "last_action_at").and_then(|s| s.parse::<i64>().ok());
    let action_attempts_in_window = recent_action_count(state_path, now);
    // Clear the dedupe latch once we're back under the cap so a fresh
    // breach in a future window emits again.
    if action_attempts_in_window < cfg.thresholds.max_attempts_per_hour {
        *gave_up_emitted = false;
    }

    // 5. Recovery detection: we acted, postgres has been stable since.
    let recovery_seen_after_action = match (*action_pending_recovery, &local.state) {
        (Some(_), Some(s)) if s == "running" || s == "streaming" => {
            // Stable: at least one full window has passed where
            // postmaster_start_time stayed constant.
            pg_starts_in_window <= 1
        }
        _ => false,
    };

    // 6. Build snapshot, decide, dispatch.
    let snapshot = SelfHealInputs {
        now,
        role,
        leader_reachable,
        patroni_state: local.state.clone().unwrap_or_default(),
        pg_starts_in_window,
        last_action_at,
        action_attempts_in_window,
        recovery_seen_after_action,
        thresholds: cfg.thresholds.clone(),
    };
    let action = decide_self_heal(&snapshot);

    let member_name = env::var("PATRONI_NAME").unwrap_or_else(|_| "unknown".to_string());

    match action {
        SelfHealAction::NoOp => {}
        SelfHealAction::Wait => {}
        SelfHealAction::Reinitialize { reason, attempt } => {
            info!(
                reason = %reason,
                attempt,
                "self-heal: triggering POST /reinitialize"
            );
            // Record telemetry *before* the API call so we capture the
            // intent even if the call itself errors out.
            telemetry.send(TelemetryEvent::SelfHealReinitTriggered {
                node: member_name.clone(),
                reason: reason.clone(),
                attempt,
            });
            // Persist before the API call too — see backup_watcher comment
            // on state-before-action: a mid-call crash leaves us
            // re-counting on next iteration, which is harmless because
            // the backoff window covers it.
            let _ = write_state_field(state_path, "last_action_at", &now.to_string());
            append_attempt(state_path, now);
            *action_pending_recovery = Some(now);

            match issue_reinitialize(client).await {
                Ok(()) => info!("self-heal: reinitialize accepted"),
                Err(e) => warn!(error = %e, "self-heal: reinitialize API call failed"),
            }
        }
        SelfHealAction::EmitRecovered {
            recovered_in_secs,
            attempts,
        } => {
            info!(recovered_in_secs, attempts, "self-heal: replica recovered");
            telemetry.send(TelemetryEvent::SelfHealRecovered {
                node: member_name.clone(),
                recovered_in_secs,
                attempts,
            });
            // Clear the pending-recovery flag and the action history.
            // Field name is "attempt" (singular) — that's the prefix
            // append_attempt writes and recent_action_count reads.
            *action_pending_recovery = None;
            let _ = clear_state_field(state_path, "last_action_at");
            let _ = clear_state_field(state_path, "attempt");
        }
        SelfHealAction::EmitGaveUp {
            last_reason,
            attempts,
        } => {
            // Dedupe: decide_self_heal returns EmitGaveUp every iteration
            // while the cap is tripped. Emit telemetry once per breach;
            // the latch clears when action_attempts_in_window drops below
            // the cap again.
            if !*gave_up_emitted {
                warn!(attempts, last_reason = %last_reason, "self-heal: giving up");
                telemetry.send(TelemetryEvent::SelfHealGaveUp {
                    node: member_name.clone(),
                    attempts,
                    last_reason,
                });
                *gave_up_emitted = true;
            }
            // Don't clear attempts — operator action required to reset.
        }
    }

    Ok(())
}

// ====================================================================
// Patroni REST helpers
// ====================================================================

#[derive(Debug, Deserialize)]
struct PatroniLocal {
    state: Option<String>,
    role: Option<String>,
    postmaster_start_time: Option<String>,
}

async fn fetch_local_patroni(client: &reqwest::Client) -> Result<PatroniLocal> {
    let resp = client.get(PATRONI_PATRONI_URL).send().await?;
    // Patroni returns 200 for leaders, 503 for non-leaders, but the JSON
    // body is identical in shape and we want the body either way.
    let body = resp.text().await?;
    let parsed: PatroniLocal = serde_json::from_str(&body)?;
    Ok(parsed)
}

#[derive(Debug, Deserialize)]
struct ClusterResponse {
    members: Vec<ClusterMember>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ClusterMember {
    name: String,
    role: String,
    state: String,
    api_url: Option<String>,
}

async fn check_leader_reachable(client: &reqwest::Client, timeout_secs: u64) -> bool {
    let cluster: ClusterResponse = match client
        .get(PATRONI_CLUSTER_URL)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(r) => match r.json().await {
            Ok(c) => c,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    let leader = cluster.members.iter().find(|m| m.role == "leader");
    let Some(leader) = leader else {
        return false;
    };
    if leader.state != "running" {
        return false;
    }
    let Some(api_url) = leader.api_url.as_ref() else {
        return false;
    };
    // Patroni's /health endpoint mirrors /leader on the leader: 200 if
    // healthy, 503 otherwise. Reuse the shared client with a per-request
    // timeout override so we keep the connection pool warm across polls.
    let url = format!("{}/health", api_url.trim_end_matches('/'));
    matches!(
        client
            .get(&url)
            .timeout(Duration::from_secs(timeout_secs))
            .send()
            .await
            .map(|r| r.status().as_u16()),
        Ok(200)
    )
}

async fn issue_reinitialize(client: &reqwest::Client) -> Result<()> {
    let body = serde_json::json!({ "force": true });
    let resp = client.post(PATRONI_REINIT_URL).json(&body).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("POST /reinitialize returned {}: {}", status, body);
    }
    Ok(())
}

fn parse_role(s: Option<&str>) -> Role {
    match s {
        Some("master") | Some("primary") | Some("leader") => Role::Leader,
        // standby_leader is the head of a cascading-replication chain — still
        // a replica from the primary's POV, safe to reinit just like any
        // other replica. sync_standby is treated the same way: from
        // Patroni's lifecycle perspective, it's a replica we can re-clone.
        Some("replica") | Some("standby") | Some("sync_standby") | Some("standby_leader") => {
            Role::Replica
        }
        // "uninitialized" (node still joining) and anything else map to
        // Unknown. Unknown alone won't trigger reinit — only Unknown
        // combined with a crash-loop signal will, which is the right
        // behavior for a fresh bootstrap that's genuinely failing.
        _ => Role::Unknown,
    }
}

// ====================================================================
// State-file helpers (same key=value shape as backup_watcher)
// ====================================================================

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn read_state_field(state_path: &str, field: &str) -> Option<String> {
    let content = fs::read_to_string(state_path).ok()?;
    let prefix = format!("{field}=");
    content
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .next_back()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_state_field(state_path: &str, field: &str, value: &str) -> Result<()> {
    let prefix = format!("{field}=");
    let existing = fs::read_to_string(state_path).unwrap_or_default();
    let mut new_lines: Vec<String> = existing
        .lines()
        .filter(|line| !line.starts_with(&prefix))
        .map(|s| s.to_string())
        .collect();
    new_lines.push(format!("{field}={value}"));
    let mut out = new_lines.join("\n");
    out.push('\n');
    fs::write(state_path, out)?;
    Ok(())
}

fn clear_state_field(state_path: &str, field: &str) -> Result<()> {
    let prefix = format!("{field}=");
    let Ok(existing) = fs::read_to_string(state_path) else {
        return Ok(());
    };
    let new_lines: Vec<String> = existing
        .lines()
        .filter(|line| !line.starts_with(&prefix))
        .map(|s| s.to_string())
        .collect();
    let mut out = new_lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    fs::write(state_path, out)?;
    Ok(())
}

/// Append a `attempt=<epoch>` line. The set is purely additive; pruning
/// happens at read-time via the rolling-hour window.
fn append_attempt(state_path: &str, now: i64) {
    let existing = fs::read_to_string(state_path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(|s| s.to_string()).collect();
    lines.push(format!("attempt={now}"));
    let mut out = lines.join("\n");
    out.push('\n');
    let _ = fs::write(state_path, out);
}

fn recent_action_count(state_path: &str, now: i64) -> u32 {
    let Ok(content) = fs::read_to_string(state_path) else {
        return 0;
    };
    let one_hour_ago = now - 3600;
    content
        .lines()
        .filter_map(|line| line.strip_prefix("attempt="))
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .filter(|t| *t >= one_hour_ago)
        .count() as u32
}

// ====================================================================
// Tests
// ====================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SelfHealInputs {
        SelfHealInputs {
            now: 10_000,
            role: Role::Replica,
            leader_reachable: true,
            patroni_state: "running".into(),
            pg_starts_in_window: 0,
            last_action_at: None,
            action_attempts_in_window: 0,
            recovery_seen_after_action: false,
            thresholds: Thresholds::default(),
        }
    }

    #[test]
    fn leader_never_acted_on_even_with_crash_loop() {
        let mut s = base();
        s.role = Role::Leader;
        s.pg_starts_in_window = 99;
        s.patroni_state = "start failed".into();
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn leader_with_pending_recovery_returns_noop_not_recovered() {
        // Safety #1 has priority over the recovery emission so the
        // Recovered event is only emitted for replicas.
        let mut s = base();
        s.role = Role::Leader;
        s.recovery_seen_after_action = true;
        s.last_action_at = Some(5_000);
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn quiet_replica_is_noop() {
        let s = base();
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn crash_loop_triggers_reinit() {
        let mut s = base();
        s.pg_starts_in_window = 3;
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize { reason, attempt } => {
                assert!(reason.starts_with("postgres_crash_loop"));
                assert_eq!(attempt, 1);
            }
            other => panic!("expected Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn patroni_start_failed_triggers_reinit() {
        let mut s = base();
        s.patroni_state = "start failed".into();
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize { reason, attempt: 1 } => {
                assert_eq!(reason, "patroni_start_failed");
            }
            other => panic!("expected Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn no_action_when_leader_unreachable() {
        let mut s = base();
        s.pg_starts_in_window = 10;
        s.leader_reachable = false;
        assert_eq!(decide_self_heal(&s), SelfHealAction::Wait);
    }

    #[test]
    fn backoff_blocks_action_within_window() {
        let mut s = base();
        s.pg_starts_in_window = 10;
        s.last_action_at = Some(s.now - 30);
        s.thresholds.action_backoff_secs = 600;
        assert_eq!(decide_self_heal(&s), SelfHealAction::Wait);
    }

    #[test]
    fn backoff_clears_after_window() {
        let mut s = base();
        s.pg_starts_in_window = 10;
        s.last_action_at = Some(s.now - 700);
        s.thresholds.action_backoff_secs = 600;
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize { .. } => {}
            other => panic!("expected Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn attempt_cap_emits_giveup() {
        let mut s = base();
        s.pg_starts_in_window = 10;
        s.action_attempts_in_window = 5;
        s.thresholds.max_attempts_per_hour = 5;
        match decide_self_heal(&s) {
            SelfHealAction::EmitGaveUp { attempts: 5, .. } => {}
            other => panic!("expected EmitGaveUp, got {other:?}"),
        }
    }

    #[test]
    fn attempts_below_cap_can_still_act() {
        let mut s = base();
        s.pg_starts_in_window = 10;
        s.action_attempts_in_window = 4;
        s.thresholds.max_attempts_per_hour = 5;
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize { attempt: 5, .. } => {}
            other => panic!("expected Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn recovery_emits_recovered_with_elapsed_secs() {
        let mut s = base();
        s.recovery_seen_after_action = true;
        s.last_action_at = Some(s.now - 120);
        s.action_attempts_in_window = 2;
        match decide_self_heal(&s) {
            SelfHealAction::EmitRecovered {
                recovered_in_secs: 120,
                attempts: 2,
            } => {}
            other => panic!("expected EmitRecovered, got {other:?}"),
        }
    }

    #[test]
    fn unknown_role_is_treated_as_non_leader_no_action() {
        // Unknown shouldn't be confused with Leader (would be unsafe to
        // skip safety check), but also shouldn't trigger reinit on its
        // own. With no crash loop, this is a NoOp.
        let mut s = base();
        s.role = Role::Unknown;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn unknown_role_with_crash_loop_still_acts() {
        // If we can detect a crash loop, we should reinit even when
        // Patroni REST didn't yield a clean role field.
        let mut s = base();
        s.role = Role::Unknown;
        s.pg_starts_in_window = 5;
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize { .. } => {}
            other => panic!("expected Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn crash_loop_threshold_is_respected() {
        let mut s = base();
        s.thresholds.crash_loop_threshold = 5;
        s.pg_starts_in_window = 4;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
        s.pg_starts_in_window = 5;
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize { .. } => {}
            other => panic!("expected Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn parse_role_accepts_all_known_variants() {
        assert_eq!(parse_role(Some("master")), Role::Leader);
        assert_eq!(parse_role(Some("primary")), Role::Leader);
        assert_eq!(parse_role(Some("leader")), Role::Leader);
        assert_eq!(parse_role(Some("replica")), Role::Replica);
        assert_eq!(parse_role(Some("standby")), Role::Replica);
        assert_eq!(parse_role(Some("sync_standby")), Role::Replica);
        // Cascading-replication head: still a replica from Patroni's
        // perspective; safe to reinit.
        assert_eq!(parse_role(Some("standby_leader")), Role::Replica);
        // Joining a fresh cluster or unrecognized states fall to Unknown,
        // which alone won't trigger reinit.
        assert_eq!(parse_role(Some("uninitialized")), Role::Unknown);
        assert_eq!(parse_role(Some("")), Role::Unknown);
        assert_eq!(parse_role(None), Role::Unknown);
    }

    // ---- State file roundtrip ----

    fn tmp_path() -> String {
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("/tmp/self_heal_test_{pid}_{nanos}")
    }

    #[test]
    fn state_field_roundtrip() {
        let p = tmp_path();
        let _ = fs::remove_file(&p);
        assert_eq!(read_state_field(&p, "last_action_at"), None);
        write_state_field(&p, "last_action_at", "12345").unwrap();
        assert_eq!(read_state_field(&p, "last_action_at"), Some("12345".into()));
        write_state_field(&p, "last_action_at", "67890").unwrap();
        assert_eq!(read_state_field(&p, "last_action_at"), Some("67890".into()));
        clear_state_field(&p, "last_action_at").unwrap();
        assert_eq!(read_state_field(&p, "last_action_at"), None);
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn recent_action_count_rolls_off_old_attempts() {
        let p = tmp_path();
        let _ = fs::remove_file(&p);
        let now = now_epoch();
        append_attempt(&p, now - 7200); // 2h ago — outside window
        append_attempt(&p, now - 1800); // 30m ago — inside
        append_attempt(&p, now - 60); //    1m ago  — inside
        assert_eq!(recent_action_count(&p, now), 2);
        let _ = fs::remove_file(&p);
    }

    /// Regression for the field-name typo: recovery used to call
    /// `clear_state_field("attempts")` while writes/reads used `"attempt"`
    /// (singular). The clear matched nothing, so attempts leaked across
    /// recovery cycles and the rolling-hour cap tripped early on
    /// re-occurrences. This test pins the singular spelling.
    #[test]
    fn clearing_attempt_removes_all_attempt_lines_and_preserves_others() {
        let p = tmp_path();
        let _ = fs::remove_file(&p);
        let now = now_epoch();
        append_attempt(&p, now - 100);
        append_attempt(&p, now - 50);
        write_state_field(&p, "last_action_at", "12345").unwrap();
        assert_eq!(recent_action_count(&p, now), 2);

        clear_state_field(&p, "attempt").unwrap();

        assert_eq!(recent_action_count(&p, now), 0);
        // Unrelated fields untouched.
        assert_eq!(read_state_field(&p, "last_action_at"), Some("12345".into()));
        let _ = fs::remove_file(&p);
    }
}
