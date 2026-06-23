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
//! Two complementary, non-log-scraping signals:
//!
//! 1. **Crash-loop / divergence (this module's watcher)** — poll Patroni REST
//!    `/patroni` and watch `postmaster_start_time`. Each Postgres restart
//!    advances that timestamp; a stable postgres holds it constant. Three or
//!    more distinct values inside `recent_window_secs` is a crash loop. A
//!    replica Patroni still considers healthy but frozen behind the leader's
//!    timeline past a dwell is the silent-divergence variant.
//!
//! 2. **WAL-too-old probe ([`confirm_wal_unrecoverable`])** — for the
//!    manifestation where the replica never becomes healthy at all (so the
//!    startup-health gate in `monitoring.rs` SIGKILLs the container every
//!    `max_startup_timeout`, resetting the crash-loop counter before it trips),
//!    we don't infer from a timer or a stalled cursor. We read two segment
//!    numbers: `pg_controldata` gives the checkpoint REDO segment — a lower
//!    bound on what the replica must stream from (read offline, works while
//!    Postgres is down) — and a `pg_ls_waldir()` query on the leader gives the
//!    oldest segment it still retains. If the former predates the latter the
//!    replica is too far behind to stream-catch-up (standbys here have no
//!    archive fallback: push-only `archive_command`, no `restore_command`).
//!    Because REDO is a lower bound this never misses a real case (no wedged
//!    replica left to restart-loop), at the cost of a possible bounded over-fire;
//!    see the detection section comment for the full argument. The startup gate
//!    calls this before wiping.
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

use super::Config;
use anyhow::Result;
use common::{Telemetry, TelemetryEvent};
use serde::Deserialize;
use std::collections::VecDeque;
use std::env;
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
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
// How long a replica's timeline must sit continuously below the leader's,
// while Patroni still considers it healthy, before we treat it as a silent
// stall and reinitialize. Long enough that a normal post-failover replica —
// which fast-forwards onto the new timeline within seconds — never accrues it.
const DEFAULT_DIVERGENCE_DWELL_SECONDS: u64 = 300;
// Minimum number of timelines the leader must be ahead before a divergence
// reinit is even considered. A healthy streaming replica is on the *same*
// timeline as the leader; being behind at all is abnormal, but we require a
// clear margin (missed ≥2 promotions) so a single borderline/in-flight switch
// can never trip a destructive wipe. This is a false-positive guard, not a
// correctness one — raise it to be stricter, never below 1.
const DEFAULT_DIVERGENCE_MIN_GAP: i64 = 2;

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
    pub divergence_dwell_secs: u64,
    pub divergence_min_gap: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            crash_loop_threshold: DEFAULT_CRASH_LOOP_THRESHOLD,
            recent_window_secs: DEFAULT_RECENT_WINDOW_SECONDS,
            action_backoff_secs: DEFAULT_ACTION_BACKOFF_SECONDS,
            max_attempts_per_hour: DEFAULT_MAX_ATTEMPTS_PER_HOUR,
            divergence_dwell_secs: DEFAULT_DIVERGENCE_DWELL_SECONDS,
            divergence_min_gap: DEFAULT_DIVERGENCE_MIN_GAP,
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
    /// This node's Patroni timeline (from `/patroni`). `None` if unknown.
    pub local_timeline: Option<i64>,
    /// The current leader's timeline (from `/cluster`). `None` when the leader
    /// is unknown or unreachable.
    pub leader_timeline: Option<i64>,
    /// Seconds the node has been *continuously* observed behind the leader with
    /// **nothing progressing** — same timeline, same WAL cursor — while Patroni
    /// reports it healthy. `0` when not currently diverged. The orchestrator
    /// resets this to `0` the instant the node catches up, leaves a healthy
    /// state, *or makes any forward progress* (timeline switch or WAL cursor
    /// advance), so only a true stall ever accrues.
    pub diverged_for_secs: u64,
    pub last_action_at: Option<i64>,
    pub action_attempts_in_window: u32,
    pub recovery_seen_after_action: bool,
    pub thresholds: Thresholds,
}

/// Which signal drove a reinit. Lets the orchestrator treat the
/// single-signal, destructive `TimelineDivergence` case more cautiously (an
/// independent re-read before acting) than the multi-poll-evidenced crash-loop
/// and start-failed cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReinitTrigger {
    StartFailed,
    CrashLoop,
    TimelineDivergence,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelfHealAction {
    NoOp,
    Wait,
    Reinitialize {
        reason: String,
        attempt: u32,
        trigger: ReinitTrigger,
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
    let stalled_divergence = stalled_timeline_divergence(s);

    if patroni_says_failed || is_crash_loop || stalled_divergence.is_some() {
        let (trigger, reason) = if patroni_says_failed {
            (
                ReinitTrigger::StartFailed,
                "patroni_start_failed".to_string(),
            )
        } else if is_crash_loop {
            (
                ReinitTrigger::CrashLoop,
                format!(
                    "postgres_crash_loop:{}_restarts_in_{}s",
                    s.pg_starts_in_window, s.thresholds.recent_window_secs
                ),
            )
        } else {
            // Only reachable when stalled_divergence is Some.
            let (local, leader) = stalled_divergence.expect("checked is_some above");
            (
                ReinitTrigger::TimelineDivergence,
                format!(
                    "timeline_diverged:tl{local}_behind_leader_tl{leader}_for_{}s",
                    s.diverged_for_secs
                ),
            )
        };
        return SelfHealAction::Reinitialize {
            reason,
            attempt: s.action_attempts_in_window + 1,
            trigger,
        };
    }

    SelfHealAction::NoOp
}

/// Pure predicate for "this looks like a stuck-behind replica right now",
/// independent of how long it has been so. Shared by the decision function and
/// the pre-action re-read so both judge divergence by exactly the same rules.
///
/// All four false-positive guards live here:
/// 1. We are a `Replica` — never a leader (destructive) and never `Unknown`
///    (a joining/uninitialized node whose timeline isn't meaningful yet).
/// 2. Patroni reports us healthy (`running`/`streaming`) — never mid-clone
///    (`creating replica`/`starting`), where a lower timeline is expected.
/// 3. Both timelines are known.
/// 4. The leader is clearly ahead — at least `min_gap` timelines — not just
///    one borderline/in-flight switch.
fn timeline_divergence_present(
    role: Role,
    patroni_state: &str,
    local_timeline: Option<i64>,
    leader_timeline: Option<i64>,
    min_gap: i64,
) -> Option<(i64, i64)> {
    if !matches!(role, Role::Replica) {
        return None;
    }
    if patroni_state != "running" && patroni_state != "streaming" {
        return None;
    }
    let local = local_timeline?;
    let leader = leader_timeline?;
    if leader.saturating_sub(local) < min_gap {
        return None;
    }
    Some((local, leader))
}

/// The silent-stall case: a replica Patroni still considers healthy
/// (`running`/`streaming`, postmaster stable so no crash-loop signal) whose
/// timeline has sat clearly below the leader's past the dwell. Patroni's own
/// `remove_data_directory_on_diverged_timelines` only re-evaluates on a
/// (re)start, so a node that came up "following leader" on a stale timeline and
/// never restarts is invisible to it — and to the crash-loop detector. A full
/// reinit (pg_basebackup from the leader) is the only fix.
///
/// Returns `(local_timeline, leader_timeline)` when it applies, else `None`.
/// The dwell itself is accrued by the orchestrator (`diverged_for_secs`); the
/// structural guards live in [`timeline_divergence_present`]. Pure and
/// unit-testable.
fn stalled_timeline_divergence(s: &SelfHealInputs) -> Option<(i64, i64)> {
    if s.diverged_for_secs < s.thresholds.divergence_dwell_secs {
        return None;
    }
    timeline_divergence_present(
        s.role,
        &s.patroni_state,
        s.local_timeline,
        s.leader_timeline,
        s.thresholds.divergence_min_gap,
    )
}

/// One poll's progress fingerprint for an actively-diverged replica: the local
/// timeline and the replication cursor (highest of received/replayed WAL
/// position). Together these are *every* axis on which a healthy replica can
/// make forward progress — a timeline switch as it crosses a switchpoint, or a
/// climbing WAL position as it streams/replays. A wedged replica advances
/// neither.
#[derive(Debug, Clone, Copy)]
struct DivergenceObs {
    local_tl: i64,
    /// Highest of `received_location`/`replayed_location` from `/patroni`'s
    /// `xlog`. `None` when Patroni reported no cursor — treated as "cannot
    /// prove a stall", which restarts the window rather than risk wiping a node
    /// that might be progressing unseen.
    progress: Option<i64>,
}

/// An in-progress timeline-divergence dwell window: the epoch it opened and the
/// progress fingerprint it opened on. The dwell matures only while *both* axes
/// stay frozen, so it measures an explicit stall — never a node catching up.
#[derive(Debug, Clone, Copy)]
struct DivergenceWindow {
    since: i64,
    local_tl: i64,
    progress: Option<i64>,
}

/// Advance the divergence dwell window given this poll's observation, so the
/// dwell accrues ONLY on an explicit, fully-stalled divergence — we want to be
/// certain nothing has progressed since the window opened before we wipe.
///
/// - Not diverged this poll → clear the window (caught up, mid-clone, leader
///   unknown, role unclear, or gap closed).
/// - Diverged, and *both* the timeline and the WAL cursor have stayed frozen
///   since the window opened → keep accruing. This is the only path that lets
///   the dwell mature into a reinit.
/// - Diverged but the timeline advanced, the cursor advanced, or the cursor is
///   unmeasurable on either side → (re)start the clock at this observation. The
///   node may be replaying across the gap (catching up), so it must not inherit
///   prior dwell.
fn accrue_divergence_window(
    current: Option<DivergenceObs>,
    window: Option<DivergenceWindow>,
    now: i64,
) -> Option<DivergenceWindow> {
    let Some(obs) = current else {
        return None;
    };
    if let Some(w) = window {
        let timeline_frozen = obs.local_tl <= w.local_tl;
        let progress_frozen = match (w.progress, obs.progress) {
            (Some(opened_at), Some(cur)) => cur <= opened_at,
            // Unknown cursor on either side → cannot prove a stall.
            _ => false,
        };
        if timeline_frozen && progress_frozen {
            return Some(w);
        }
    }
    Some(DivergenceWindow {
        since: now,
        local_tl: obs.local_tl,
        progress: obs.progress,
    })
}

// ====================================================================
// Shared helpers for the startup-gate self-heal path (monitoring.rs)
// ====================================================================
//
// This watcher catches the manifestation where Patroni considers the node
// healthy: Postgres flapping underneath it (postmaster_start_time advances
// ≥3x in a minute) or a replica silently stuck behind the leader's timeline.
// It cannot catch the *other* manifestation — a replica that never becomes
// healthy at all, so the patroni-runner startup-health gate (monitoring.rs)
// SIGKILLs the whole container every `max_startup_timeout`. That kill resets
// this in-process watcher's state on every restart (the postmaster only
// starts once per ~5-minute boot, never ≥3x/60s) and the node never reaches
// the `running`/`streaming` state the divergence path requires. The canonical
// case is WAL-too-old: the leader rotated WAL past the replica's restart LSN,
// its on-disk pgdata is valid (same timeline, intact pg_control) so neither
// Patroni's built-in recovery nor a plain restart ever re-clones it.
//
// The startup gate reaches into these helpers to issue the same forced
// reinitialize, and shares the per-hour attempt accounting below so the two
// self-heal paths can't jointly exceed the reinitialize budget.

/// True when the operator kill switch `SELF_HEAL_DISABLED=1` is set. Honored by
/// both the watcher (which never spawns) and the startup-gate reinit path (which
/// falls back to the recovery exit), so disabling self-heal stops *every*
/// destructive reinit — not just the watcher's.
pub fn disabled() -> bool {
    env::var("SELF_HEAL_DISABLED").ok().as_deref() == Some("1")
}

/// Build the same short-timeout HTTP client the watcher uses. Best-effort.
pub fn http_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()
}

/// This node's Patroni role, or None if the local REST API didn't answer.
pub async fn local_role(client: &reqwest::Client) -> Option<Role> {
    let local = fetch_local_patroni(client).await.ok()?;
    Some(parse_role(local.role.as_deref()))
}

/// True if the cluster leader is reachable — a reinitialize clones from it, so
/// acting without one would wipe pgdata with no source to refill it.
pub async fn is_leader_reachable(client: &reqwest::Client, timeout_secs: u64) -> bool {
    probe_leader(client, timeout_secs).await.reachable
}

/// POST `/reinitialize {"force": true}` to the local Patroni REST API.
pub async fn force_reinitialize(client: &reqwest::Client) -> Result<()> {
    issue_reinitialize(client).await
}

/// Per-hour reinitialize cap, env-overridable, shared by both self-heal paths.
pub fn reinit_attempt_cap() -> u32 {
    env_u32(
        "SELF_HEAL_MAX_ATTEMPTS_PER_HOUR",
        DEFAULT_MAX_ATTEMPTS_PER_HOUR,
    )
}

/// Reinitialize attempts recorded in the trailing hour. Persistent (the state
/// file lives at the volume root, not in pgdata) so the count survives the
/// container restarts the startup gate triggers between attempts.
pub fn recent_reinit_attempts() -> u32 {
    recent_action_count(&state_file_path(), now_epoch())
}

/// Record a reinitialize attempt against the shared per-hour budget. Mirrors
/// the watcher: persist before issuing the call so a wedged Patroni REST can't
/// drive unbounded re-issues.
pub fn record_reinit_attempt() {
    let now = now_epoch();
    let path = state_file_path();
    let _ = write_state_field(&path, "last_action_at", &now.to_string());
    append_attempt(&path, now);
}

/// Pure decision for the startup-gate path: reinitialize a stalled node only
/// when it is NOT a leader (wiping a primary is catastrophic), the leader is
/// reachable (a clone source exists), and we are under the per-hour cap. Kept
/// pure and zero-I/O so it is unit-testable.
pub fn should_reinit_stalled(
    role: Role,
    leader_reachable: bool,
    recent_attempts: u32,
    cap: u32,
) -> bool {
    !matches!(role, Role::Leader) && leader_reachable && recent_attempts < cap
}

/// Path to the persistent self-heal state file. At the volume root (NOT pgdata)
/// so it survives the reinitialize wipe — the same path the watcher derives
/// from the volume root it is spawned with, so both paths share one budget.
fn state_file_path() -> String {
    format!("{}/{}", crate::volume_root(), STATE_FILENAME)
}

// ====================================================================
// WAL-too-old detection (sufficient-condition probe, no false positives)
// ====================================================================
//
// Rather than infer WAL-too-old from a stalled log cursor or a quiet timer, we
// read WAL segment numbers directly and compare them. Standbys here have no
// archive fallback (archive_command is push-only; there is no restore_command),
// so a replica is unrecoverable-by-streaming exactly when the segment it must
// resume streaming from is older than everything the leader still retains.
//
// A reinitialize WIPES pgdata and forces a full re-clone, so the verdict MUST be
// a SUFFICIENT condition: fire only when the replica is *provably* unable to
// stream-catch-up. We never read the exact resume point offline, but we can
// bound it from ABOVE and compare that upper bound against a hard fact about the
// leader:
//
//   1. An UPPER BOUND on the segment the replica will request from the leader.
//      On restart the replica replays its local WAL and then streams from the
//      end of it, so the segment it needs is at most one past the newest segment
//      already in its own pg_wal:  resume_point <= newest_local + 1. We read the
//      newest local segment by listing pg_wal *offline* (works while Postgres is
//      down/crash-looping — exactly the WAL-too-old state) and take its
//      successor. Preallocated/recycled future segment files only inflate
//      `newest_local`, which makes the bound LARGER and the verdict STRICTER —
//      never wrong, only more conservative.
//   2. The oldest segment the leader still physically retains — one
//      `pg_ls_waldir()` query against the leader's Postgres.
//
// Verdict: reinit only when `successor(newest_local) < leader_oldest`. Because
// `resume_point <= successor(newest_local)`, this is a SUFFICIENT condition:
//
//   * No false positives. The trigger guarantees `resume_point < leader_oldest`,
//     i.e. the segment the replica must stream from is genuinely gone from the
//     leader. We never wipe a replica a plain restart could have recovered — the
//     property that matters for a destructive op.
//   * Possible false negatives. We may MISS a genuine WAL-too-old in the narrow
//     boundary band where the true resume point is just below the leader's floor
//     but `newest_local + 1 >= leader_oldest`. Such a node keeps restart-looping
//     exactly as it does today — no worse than the status quo, and backboard's
//     monitor / a human still handle it. We deliberately accept that over the
//     alternative (a destructive re-clone of a replica that was actually fine).
//
// Comparisons use `(logid, segid)` — the byte-position half of the WAL filename,
// ignoring the 8-hex timeline prefix. That ordering matches WAL byte-position
// ordering for any fixed segment size, so the comparison is timeline-independent:
// a pg_rewind'd replica (which ends up on the leader's timeline but needs WAL
// from before its slot floor) shares segment numbering with the leader even when
// the prefixes differ. The successor step is the one place we need the WAL
// segment size; we read it from the same control file (`Bytes per WAL segment`),
// and both nodes share it (initdb-fixed, inherited by the clone).

/// The byte-position key `(logid, segid)` of a 24-hex WAL filename, ignoring
/// the timeline prefix (first 8 hex). Ordering this tuple matches WAL
/// byte-position ordering for any fixed segment size, so two keys are
/// comparable across timelines without knowing the segment size. `None` when
/// `name` is not a 24-hex-digit WAL segment (e.g. `.history`, `.partial`,
/// `archive_status`).
fn wal_segment_key(name: &str) -> Option<(u64, u64)> {
    let name = name.trim();
    if name.len() != 24 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let logid = u64::from_str_radix(&name[8..16], 16).ok()?;
    let segid = u64::from_str_radix(&name[16..24], 16).ok()?;
    Some((logid, segid))
}

/// Number of WAL segments per logid for a given segment size — the value the low
/// 8 hex of a WAL filename wraps at (`0x1_0000_0000 / wal_segment_size`, e.g. 256
/// for the default 16 MiB). `None` for a zero/garbage size.
fn segments_per_logid(wal_segment_size: u64) -> Option<u64> {
    (wal_segment_size != 0).then(|| 0x1_0000_0000u64 / wal_segment_size)
}

/// The next WAL segment key after `key` in byte-position order, carrying into the
/// logid when `segid` reaches `segs_per_logid`.
fn wal_segment_successor((logid, segid): (u64, u64), segs_per_logid: u64) -> (u64, u64) {
    if segid + 1 >= segs_per_logid {
        (logid + 1, 0)
    } else {
        (logid, segid + 1)
    }
}

/// Parse "Bytes per WAL segment" from `pg_controldata` text — needed to take the
/// successor of a segment key. `None` if absent or unparseable.
fn parse_controldata_wal_segsize(output: &str) -> Option<u64> {
    output
        .lines()
        .find_map(|l| l.split_once("Bytes per WAL segment:").map(|(_, v)| v))
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Newest WAL segment key present in this node's `pg_wal`, read offline (works
/// while Postgres is down/crash-looping). `None` when the directory is unreadable
/// or holds no segment file. Recycled/preallocated future segments may inflate
/// this — that is safe: it only makes the resume upper bound larger and the
/// verdict more conservative (see the section comment).
fn local_newest_wal_segment(data_dir: &str) -> Option<(u64, u64)> {
    fs::read_dir(format!("{data_dir}/pg_wal"))
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| wal_segment_key(e.file_name().to_str()?))
        .max()
}

/// An UPPER BOUND on the segment the replica will request from the leader on its
/// next start: `successor(newest local segment)`. Reads `pg_wal` offline for the
/// newest segment and `pg_controldata` for the WAL segment size needed to take
/// the successor. `None` on any failure — callers treat that as "cannot prove
/// unrecoverable" and never wipe on it.
async fn local_resume_upper_bound(data_dir: &str) -> Option<(u64, u64)> {
    let newest = local_newest_wal_segment(data_dir)?;
    let read = Command::new("pg_controldata")
        .arg(data_dir)
        // Stable, locale-independent field labels.
        .env("LC_ALL", "C")
        .output();
    // Reads a local file and should return instantly; bound it anyway so a wedged
    // pg_controldata can't stall the monitoring loop.
    let out = match tokio::time::timeout(Duration::from_secs(5), read).await {
        Ok(Ok(out)) if out.status.success() => out,
        _ => return None,
    };
    let segsize = parse_controldata_wal_segsize(&String::from_utf8_lossy(&out.stdout))?;
    let per_logid = segments_per_logid(segsize)?;
    Some(wal_segment_successor(newest, per_logid))
}

/// Postgres `(host, port)` of the current leader, from `/cluster`. `None` when
/// the query fails, there is no leader, or the leader advertises no connect host.
/// Each `None` path logs: the host/port branch in particular is the one external
/// assumption this whole mechanism rests on (that Patroni's `/cluster` member
/// objects carry `host`/`port`). If that ever stops holding the WAL probe silently
/// can't fire, so we surface it loudly rather than degrade to an invisible no-op.
async fn leader_pg_endpoint(client: &reqwest::Client) -> Option<(String, i64)> {
    let cluster: ClusterResponse = match client.get(PATRONI_CLUSTER_URL).send().await {
        Ok(resp) => match resp.json().await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "self-heal: /cluster JSON parse failed — cannot locate leader for WAL probe");
                return None;
            }
        },
        Err(e) => {
            warn!(error = %e, "self-heal: /cluster request failed — cannot locate leader for WAL probe");
            return None;
        }
    };
    let Some(leader) = cluster.members.iter().find(|m| m.role == "leader") else {
        warn!("self-heal: no leader member in /cluster — cannot run WAL-availability probe");
        return None;
    };
    let Some(host) = leader.host.clone() else {
        warn!(
            leader = %leader.name,
            "self-heal: leader member in /cluster advertises no host — WAL-availability probe disabled, the startup-gate re-clone cannot fire (check Patroni /cluster member schema)"
        );
        return None;
    };
    Some((host, leader.port.unwrap_or(5432)))
}

/// The oldest WAL segment key the leader still physically retains, via
/// `pg_ls_waldir()`. `None` on any connection/query failure. We min over
/// `(logid, segid)` rather than trust lexical filename ordering so a stray
/// higher-timeline-prefixed segment around a switchpoint can't mask an older
/// byte position.
async fn leader_oldest_segment(host: &str, port: i64, config: &Config) -> Option<(u64, u64)> {
    let query = Command::new("psql")
        .args([
            "-U",
            &config.superuser,
            "-h",
            host,
            "-p",
            &port.to_string(),
            "-d",
            "postgres",
            "-tAXq",
            "-c",
            "SELECT name FROM pg_ls_waldir() WHERE name ~ '^[0-9A-Fa-f]{24}$'",
        ])
        .env("PGPASSWORD", &config.superuser_pass)
        .env("PGCONNECT_TIMEOUT", "5")
        .env_remove("PGHOST")
        .env_remove("PGPORT")
        .env_remove("PGDATABASE")
        .output();
    // PGCONNECT_TIMEOUT only bounds connection setup. Wrap the whole call so a
    // post-connect hang (a leader busy/in-recovery during a failover wave) can't
    // block the startup monitoring loop indefinitely — treat a timeout as "can't
    // prove unrecoverable" like any other query failure.
    let out = match tokio::time::timeout(Duration::from_secs(10), query).await {
        Ok(Ok(out)) => out,
        Ok(Err(_)) => return None,
        Err(_) => {
            warn!(host, "self-heal: leader pg_ls_waldir query timed out");
            return None;
        }
    };
    if !out.status.success() {
        warn!(
            host,
            stderr = %String::from_utf8_lossy(&out.stderr),
            "self-heal: leader pg_ls_waldir query failed"
        );
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(wal_segment_key)
        .min()
}

/// Pure verdict: is the segment the replica must resume from (its UPPER BOUND,
/// `successor(newest_local)`) older than everything the leader retains? `true`
/// only when both keys are known and `resume_upper_bound < leader_oldest` — a
/// SUFFICIENT condition for WAL-too-old (no false positives). Any unknown →
/// `false` (never wipe on a maybe).
fn streaming_unrecoverable(
    resume_upper_bound: Option<(u64, u64)>,
    leader_oldest: Option<(u64, u64)>,
) -> bool {
    matches!((resume_upper_bound, leader_oldest), (Some(r), Some(o)) if r < o)
}

/// WAL-too-old probe: `true` when the replica is PROVABLY unable to stream-catch-
/// up — an UPPER BOUND on the segment it must resume from (the successor of its
/// newest local WAL segment) is older than the oldest WAL the leader still
/// retains, and standbys here have no archive fallback. This is a SUFFICIENT
/// condition: it never wipes a replica a restart could recover (no false
/// positives), at the cost of possibly missing a genuine case in a narrow
/// boundary band (a safe false negative — the node keeps restart-looping as it
/// does today). Any uncertainty — pg_wal/control file unreadable, no leader
/// endpoint, leader query failed, or the needed segment is still present —
/// returns `false`. See the section comment for the full argument. Replaces every
/// prior heuristic (stalled cursor, stderr scraping, dwell timers, REDO lower-
/// bound) for the "replica never becomes healthy" manifestation.
pub async fn confirm_wal_unrecoverable(client: &reqwest::Client, config: &Config) -> bool {
    let resume_upper_bound = local_resume_upper_bound(&config.data_dir).await;
    if resume_upper_bound.is_none() {
        warn!(
            data_dir = %config.data_dir,
            "self-heal: could not read pg_wal or pg_controldata — WAL-too-old probe cannot fire"
        );
    }
    let Some((host, port)) = leader_pg_endpoint(client).await else {
        return false;
    };
    let leader_oldest = leader_oldest_segment(&host, port, config).await;
    let unrecoverable = streaming_unrecoverable(resume_upper_bound, leader_oldest);
    if unrecoverable {
        warn!(
            resume_upper_bound = ?resume_upper_bound,
            leader_oldest = ?leader_oldest,
            leader_host = %host,
            "self-heal: the segment this replica must resume from is older than the leader's oldest retained WAL (no archive fallback) — provably unrecoverable by streaming"
        );
    }
    unrecoverable
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
    if disabled() {
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
        divergence_dwell_secs = cfg.thresholds.divergence_dwell_secs,
        divergence_min_gap = cfg.thresholds.divergence_min_gap,
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
                divergence_dwell_secs: env_u64(
                    "SELF_HEAL_DIVERGENCE_DWELL_SECONDS",
                    DEFAULT_DIVERGENCE_DWELL_SECONDS,
                ),
                divergence_min_gap: env_i64(
                    "SELF_HEAL_DIVERGENCE_MIN_GAP",
                    DEFAULT_DIVERGENCE_MIN_GAP,
                )
                .max(1),
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

fn env_i64(k: &str, default: i64) -> i64 {
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
    // Open dwell window for a replica observed stalled behind the leader on a
    // fixed timeline while healthy; None when caught up or making progress.
    // In-memory like `starts_seen` — a container restart re-arms Patroni's own
    // startup divergence check anyway.
    let mut divergence_window: Option<DivergenceWindow> = None;

    loop {
        if let Err(e) = iteration(
            &client,
            &cfg,
            &state_path,
            &mut starts_seen,
            &mut action_pending_recovery,
            &mut gave_up_emitted,
            &mut divergence_window,
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
    divergence_window: &mut Option<DivergenceWindow>,
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

    // 3. Leader probe: reachable (clone source for a reinit) + its timeline
    //    (so we can detect a replica stuck behind it).
    let leader = probe_leader(client, cfg.leader_health_timeout_secs).await;
    let leader_reachable = leader.reachable;
    let leader_timeline = leader.timeline;
    let local_timeline = local.timeline;

    // 3b. Accrue the divergence dwell, but ONLY against an explicit stall: the
    // window matures while the local timeline sits frozen below the leader's
    // (replica, healthy, leader clearly ahead — the same predicate the decision
    // uses). Any forward progress of the local timeline restarts the clock — a
    // node replaying across the gap is catching up, not wedged — and any
    // non-diverged observation (caught up, mid-clone, leader unknown, role
    // unclear, gap too small) clears it. A normal post-failover replica
    // fast-forwards within a poll or two and so never reaches the dwell; a
    // silently stuck one, frozen on a stale timeline, does.
    let patroni_state = local.state.clone().unwrap_or_default();
    // Highest WAL position Patroni reports for this node — the axis (besides the
    // timeline) on which a catching-up replica makes visible progress.
    let local_progress = local.xlog.and_then(|x| x.highest_location());
    let current_obs = timeline_divergence_present(
        role,
        &patroni_state,
        local_timeline,
        leader_timeline,
        cfg.thresholds.divergence_min_gap,
    )
    .map(|(local_tl, _leader_tl)| DivergenceObs {
        local_tl,
        progress: local_progress,
    });
    *divergence_window = accrue_divergence_window(current_obs, *divergence_window, now);
    let diverged_for_secs = divergence_window
        .map(|w| now.saturating_sub(w.since).max(0) as u64)
        .unwrap_or(0);

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
    // Recovery detection requires (a) postgres state is healthy, (b)
    // exactly one stable postmaster_start_time observed in the window
    // (no flickers — a crash loop would have ≥ 2, an empty deque 0;
    // both reject), and (c) that observation was made at or after the
    // action timestamp. Without (c), a stale pre-action entry that
    // happens to still be inside the window would satisfy the count
    // check and fire `EmitRecovered` with a misleading
    // `recovered_in_secs` measured against `last_action_at` — most
    // visible on a watcher that restored `action_pending_recovery`
    // from disk on cold start.
    let recovery_seen_after_action = match (*action_pending_recovery, &local.state) {
        (Some(action_t), Some(s)) if s == "running" || s == "streaming" => {
            pg_starts_in_window == 1 && starts_seen.front().is_some_and(|(t, _)| *t >= action_t)
        }
        _ => false,
    };

    // 6. Build snapshot, decide, dispatch.
    let snapshot = SelfHealInputs {
        now,
        role,
        leader_reachable,
        patroni_state,
        pg_starts_in_window,
        local_timeline,
        leader_timeline,
        diverged_for_secs,
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
        SelfHealAction::Reinitialize {
            reason,
            attempt,
            trigger,
        } => {
            // Double-check destructive divergence wipes against an independent,
            // fresh read taken right now — guards against acting on a single
            // stale or flaky poll. Crash-loop and start-failed are evidenced
            // across many polls already, so they skip this. If the re-read no
            // longer agrees, the state changed under us: reset the dwell and
            // wait for it to re-accrue rather than wipe on stale data.
            if trigger == ReinitTrigger::TimelineDivergence
                && !confirm_timeline_divergence(client, cfg).await
            {
                // Skip this cycle without consuming an attempt or starting
                // backoff. We deliberately leave the divergence window alone: if
                // the node genuinely caught up or made progress, the next
                // iteration's top-level accrual resets it; if this was just a
                // flaky read, the dwell stands and we retry next poll instead of
                // paying a full re-accrual.
                info!(
                    reason = %reason,
                    "self-heal: timeline-divergence re-check did not confirm on fresh read, skipping reinit"
                );
                return Ok(());
            }

            info!(
                reason = %reason,
                attempt,
                "self-heal: triggering POST /reinitialize"
            );
            // Persist state *before* the API call so backoff/cap apply
            // even if Patroni REST is wedged — otherwise we'd hammer it
            // every poll. A mid-call crash leaves us re-counting on next
            // iteration, which is harmless because backoff covers it.
            let _ = write_state_field(state_path, "last_action_at", &now.to_string());
            append_attempt(state_path, now);
            *action_pending_recovery = Some(now);

            // Telemetry is split by outcome: `SelfHealReinitTriggered`
            // means Patroni accepted the call (reinit is in flight);
            // `SelfHealReinitRequestFailed` means we tried but couldn't
            // reach Patroni. Without the split, operators paged on
            // Triggered would hunt for a reinit in progress and find
            // none when the API call had actually failed.
            match issue_reinitialize(client).await {
                Ok(()) => {
                    info!("self-heal: reinitialize accepted");
                    telemetry.send(TelemetryEvent::SelfHealReinitTriggered {
                        node: member_name.clone(),
                        reason: reason.clone(),
                        attempt,
                    });
                }
                Err(e) => {
                    warn!(error = %e, "self-heal: reinitialize API call failed");
                    telemetry.send(TelemetryEvent::SelfHealReinitRequestFailed {
                        node: member_name.clone(),
                        reason: reason.clone(),
                        attempt,
                        error: e.to_string(),
                    });
                }
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
    timeline: Option<i64>,
    xlog: Option<Xlog>,
}

/// Replication progress block from `/patroni`. On a replica these report the
/// WAL byte position received from / replayed off the primary; both climb while
/// the node is making progress and freeze when it stalls.
#[derive(Debug, Deserialize, Clone, Copy)]
struct Xlog {
    received_location: Option<i64>,
    replayed_location: Option<i64>,
}

impl Xlog {
    /// Highest position observed across both cursors — the most generous
    /// "is it progressing" reading, so any advance on either resets the dwell.
    fn highest_location(self) -> Option<i64> {
        match (self.received_location, self.replayed_location) {
            (Some(r), Some(p)) => Some(r.max(p)),
            (Some(r), None) => Some(r),
            (None, Some(p)) => Some(p),
            (None, None) => None,
        }
    }
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
    timeline: Option<i64>,
    /// Postgres connect host/port from `/cluster`. Used to query the leader's
    /// physical WAL retention directly (the WAL-too-old probe).
    host: Option<String>,
    port: Option<i64>,
}

/// What we learn about the leader from one `/cluster` poll: whether it is
/// reachable and healthy (so a reinit has a clone source) and its current
/// timeline (so we can spot a replica stuck behind it).
#[derive(Debug, Default, Clone, Copy)]
struct LeaderProbe {
    reachable: bool,
    timeline: Option<i64>,
}

async fn probe_leader(client: &reqwest::Client, timeout_secs: u64) -> LeaderProbe {
    let cluster: ClusterResponse = match client
        .get(PATRONI_CLUSTER_URL)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(r) => match r.json().await {
            Ok(c) => c,
            Err(_) => return LeaderProbe::default(),
        },
        Err(_) => return LeaderProbe::default(),
    };
    let Some(leader) = cluster.members.iter().find(|m| m.role == "leader") else {
        return LeaderProbe::default();
    };
    // Carry the leader's timeline even before the health probe — the divergence
    // check needs it, and a leader that fails the /health probe still pins the
    // timeline a stuck replica should be measured against.
    let timeline = leader.timeline;
    if leader.state != "running" {
        return LeaderProbe {
            reachable: false,
            timeline,
        };
    }
    let Some(api_url) = leader.api_url.as_ref() else {
        return LeaderProbe {
            reachable: false,
            timeline,
        };
    };
    // Patroni's /health endpoint mirrors /leader on the leader: 200 if
    // healthy, 503 otherwise. Reuse the shared client with a per-request
    // timeout override so we keep the connection pool warm across polls.
    let url = format!("{}/health", api_url.trim_end_matches('/'));
    let reachable = matches!(
        client
            .get(&url)
            .timeout(Duration::from_secs(timeout_secs))
            .send()
            .await
            .map(|r| r.status().as_u16()),
        Ok(200)
    );
    LeaderProbe {
        reachable,
        timeline,
    }
}

/// Independent re-confirmation of a timeline divergence taken immediately
/// before the destructive reinit. Does its own fresh reads of `/patroni`
/// (local) and `/cluster` (leader) — not the snapshot the decision was made
/// from — then re-applies the exact structural guards via
/// [`timeline_divergence_present`] plus an explicit active-leader check
/// (`reachable`, i.e. leader present, `running`, and `/health` 200). Any
/// disagreement returns `false` and the caller backs off. This is the final
/// safety net against acting on a single stale or flaky poll.
async fn confirm_timeline_divergence(client: &reqwest::Client, cfg: &WatcherConfig) -> bool {
    let Ok(local) = fetch_local_patroni(client).await else {
        return false;
    };
    let leader = probe_leader(client, cfg.leader_health_timeout_secs).await;
    if !leader.reachable {
        return false;
    }
    timeline_divergence_present(
        parse_role(local.role.as_deref()),
        &local.state.unwrap_or_default(),
        local.timeline,
        leader.timeline,
        cfg.thresholds.divergence_min_gap,
    )
    .is_some()
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
            local_timeline: None,
            leader_timeline: None,
            diverged_for_secs: 0,
            last_action_at: None,
            action_attempts_in_window: 0,
            recovery_seen_after_action: false,
            thresholds: Thresholds::default(),
        }
    }

    /// A replica healthy in Patroni's eyes but stuck behind the leader's
    /// timeline past the dwell.
    fn diverged() -> SelfHealInputs {
        let mut s = base();
        s.patroni_state = "running".into();
        s.local_timeline = Some(3);
        s.leader_timeline = Some(7);
        s.diverged_for_secs = s.thresholds.divergence_dwell_secs;
        s
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
            SelfHealAction::Reinitialize {
                reason,
                attempt,
                trigger,
            } => {
                assert!(reason.starts_with("postgres_crash_loop"));
                assert_eq!(attempt, 1);
                assert_eq!(trigger, ReinitTrigger::CrashLoop);
            }
            other => panic!("expected Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn patroni_start_failed_triggers_reinit() {
        let mut s = base();
        s.patroni_state = "start failed".into();
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize {
                reason,
                attempt: 1,
                trigger,
            } => {
                assert_eq!(reason, "patroni_start_failed");
                assert_eq!(trigger, ReinitTrigger::StartFailed);
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
    fn stalled_divergence_triggers_reinit() {
        let s = diverged();
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize {
                reason,
                attempt: 1,
                trigger,
            } => {
                assert!(reason.starts_with("timeline_diverged"), "got {reason}");
                assert!(reason.contains("tl3"), "got {reason}");
                assert!(reason.contains("tl7"), "got {reason}");
                assert_eq!(trigger, ReinitTrigger::TimelineDivergence);
            }
            other => panic!("expected Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn divergence_requires_replica_role() {
        // An Unknown-role node (joining/uninitialized) whose timeline reads low
        // must never be wiped for divergence — only crash-loop evidence can act
        // on a non-replica.
        let mut s = diverged();
        s.role = Role::Unknown;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn divergence_below_min_gap_is_noop() {
        // One timeline behind is not "clearly ahead" — could be a single
        // in-flight switch. Require the configured margin.
        let mut s = diverged();
        s.local_timeline = Some(6);
        s.leader_timeline = Some(7); // gap 1 < default min_gap 2
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn divergence_at_exactly_min_gap_acts() {
        let mut s = diverged();
        s.local_timeline = Some(5);
        s.leader_timeline = Some(7); // gap 2 == default min_gap
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize {
                trigger: ReinitTrigger::TimelineDivergence,
                ..
            } => {}
            other => panic!("expected divergence Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn divergence_below_dwell_is_noop() {
        let mut s = diverged();
        s.diverged_for_secs = s.thresholds.divergence_dwell_secs - 1;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn divergence_only_acts_in_healthy_state() {
        // Mid-clone states sit on a lower timeline legitimately; never act.
        let mut s = diverged();
        s.patroni_state = "creating replica".into();
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn streaming_state_is_eligible_for_divergence() {
        let mut s = diverged();
        s.patroni_state = "streaming".into();
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize { .. } => {}
            other => panic!("expected Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn caught_up_replica_is_noop_even_past_dwell() {
        let mut s = diverged();
        s.local_timeline = Some(7);
        s.leader_timeline = Some(7);
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn unknown_timelines_never_trigger_divergence() {
        let mut s = diverged();
        s.leader_timeline = None;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn divergence_respects_leader_unreachable() {
        // No clone source → Wait, even with a long-stalled divergence.
        let mut s = diverged();
        s.leader_reachable = false;
        assert_eq!(decide_self_heal(&s), SelfHealAction::Wait);
    }

    #[test]
    fn divergence_respects_backoff_and_cap() {
        let mut s = diverged();
        s.last_action_at = Some(s.now - 30);
        s.thresholds.action_backoff_secs = 600;
        assert_eq!(decide_self_heal(&s), SelfHealAction::Wait);

        let mut s = diverged();
        s.action_attempts_in_window = 5;
        s.thresholds.max_attempts_per_hour = 5;
        match decide_self_heal(&s) {
            SelfHealAction::EmitGaveUp { .. } => {}
            other => panic!("expected EmitGaveUp, got {other:?}"),
        }
    }

    #[test]
    fn leader_never_acted_on_even_when_diverged() {
        // A leader reporting a lower timeline than some stale member view must
        // never be wiped — safety #1 wins.
        let mut s = diverged();
        s.role = Role::Leader;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    fn obs(local_tl: i64, progress: Option<i64>) -> Option<DivergenceObs> {
        Some(DivergenceObs { local_tl, progress })
    }

    #[test]
    fn dwell_accrues_only_while_fully_frozen() {
        // Window opens on the first diverged poll, then a later poll with the
        // same timeline AND the same WAL cursor must not reset the clock.
        let w0 = accrue_divergence_window(obs(3, Some(100)), None, 1_000).unwrap();
        assert_eq!(w0.since, 1_000);
        let w1 = accrue_divergence_window(obs(3, Some(100)), Some(w0), 1_060).unwrap();
        assert_eq!(w1.since, 1_000, "a fully-frozen poll must keep accruing");
    }

    #[test]
    fn replay_progress_resets_dwell_even_on_stale_timeline() {
        // The catch-up case: timeline still 3 (below the leader) but the WAL
        // cursor is climbing as the node replays toward the switchpoint. That
        // is forward progress — the clock must restart so it is never wiped.
        let w0 = accrue_divergence_window(obs(3, Some(100)), None, 1_000).unwrap();
        let w1 = accrue_divergence_window(obs(3, Some(200)), Some(w0), 1_060).unwrap();
        assert_eq!(
            w1.since, 1_060,
            "WAL progress on a stale timeline must reset"
        );
        assert_eq!(w1.progress, Some(200));
    }

    #[test]
    fn timeline_progress_resets_dwell() {
        let w0 = accrue_divergence_window(obs(3, Some(100)), None, 1_000).unwrap();
        let w1 = accrue_divergence_window(obs(4, Some(100)), Some(w0), 1_060).unwrap();
        assert_eq!(w1.since, 1_060, "crossing a switchpoint must reset");
    }

    #[test]
    fn unknown_cursor_never_accrues() {
        // Without a measurable cursor we cannot prove a stall, so the window
        // restarts every poll and the dwell never matures. Fail-safe: a node
        // whose progress we can't see is never wiped for divergence.
        let w0 = accrue_divergence_window(obs(3, None), None, 1_000).unwrap();
        let w1 = accrue_divergence_window(obs(3, None), Some(w0), 1_060).unwrap();
        assert_eq!(w1.since, 1_060, "unmeasurable cursor must not accrue");
    }

    #[test]
    fn not_diverged_clears_window() {
        let w0 = accrue_divergence_window(obs(3, Some(100)), None, 1_000).unwrap();
        assert!(accrue_divergence_window(None, Some(w0), 1_060).is_none());
    }

    #[test]
    fn should_reinit_stalled_guards() {
        let cap = 5;
        // Replica, leader reachable, under cap → act.
        assert!(should_reinit_stalled(Role::Replica, true, 0, cap));
        // Unknown role (joining/uninitialized but genuinely stalled with a
        // reachable leader) → act: a fresh clone is the right recovery.
        assert!(should_reinit_stalled(Role::Unknown, true, 4, cap));
        // Never wipe a leader.
        assert!(!should_reinit_stalled(Role::Leader, true, 0, cap));
        // No clone source.
        assert!(!should_reinit_stalled(Role::Replica, false, 0, cap));
        // Cap reached → leave for manual intervention.
        assert!(!should_reinit_stalled(Role::Replica, true, cap, cap));
        assert!(!should_reinit_stalled(Role::Replica, true, cap + 1, cap));
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

    // ----------------------------------------------------------------
    // WAL-too-old detection (sufficient-condition, no false positives)
    // ----------------------------------------------------------------

    #[test]
    fn wal_segment_key_parses_byte_position_ignoring_timeline() {
        // TLI=1, logid=0, segid=3.
        assert_eq!(wal_segment_key("000000010000000000000003"), Some((0, 3)));
        // Same byte position on a different timeline → same key.
        assert_eq!(wal_segment_key("000000090000000000000003"), Some((0, 3)));
        // logid=2, segid=0xB1.
        assert_eq!(wal_segment_key("0000000100000002000000B1"), Some((2, 0xB1)));
        // Trims surrounding whitespace (psql -tAXq can leave it).
        assert_eq!(
            wal_segment_key("  00000001000000000000000A  "),
            Some((0, 0xA))
        );
    }

    #[test]
    fn wal_segment_key_rejects_non_segments() {
        // .history, .partial, archive_status dir, short/garbage names.
        assert_eq!(wal_segment_key("00000002.history"), None);
        assert_eq!(wal_segment_key("000000010000000000000003.partial"), None);
        assert_eq!(wal_segment_key("archive_status"), None);
        assert_eq!(wal_segment_key(""), None);
        assert_eq!(wal_segment_key("not-hex-not-hex-not-hex0"), None);
    }

    #[test]
    fn wal_segment_key_ordering_matches_byte_position() {
        // Crossing the logid boundary: segid 0xFF on logid 0 precedes
        // segid 0 on logid 1, exactly as byte positions do.
        let a = wal_segment_key("0000000100000000000000FF").unwrap();
        let b = wal_segment_key("000000010000000100000000").unwrap();
        assert!(a < b);
    }

    #[test]
    fn parse_controldata_extracts_wal_segsize() {
        let sample = "\
pg_control version number:            1300
Latest checkpoint location:           0/4000060
Latest checkpoint's REDO location:    0/4000028
Latest checkpoint's REDO WAL file:    000000010000000000000004
Latest checkpoint's TimeLineID:       1
Minimum recovery ending location:     0/0
Bytes per WAL segment:                16777216
";
        assert_eq!(parse_controldata_wal_segsize(sample), Some(16777216));
    }

    #[test]
    fn parse_controldata_missing_segsize_is_none() {
        let sample = "Latest checkpoint location:           0/4000060\n";
        assert_eq!(parse_controldata_wal_segsize(sample), None);
    }

    #[test]
    fn segments_per_logid_for_known_sizes() {
        // Default 16 MiB → 256 segments per logid (the 0xFF wrap point).
        assert_eq!(segments_per_logid(16 * 1024 * 1024), Some(256));
        // 1 MiB → 4096; a non-default size still maps cleanly.
        assert_eq!(segments_per_logid(1024 * 1024), Some(4096));
        // Garbage size → None (caller treats as "cannot prove unrecoverable").
        assert_eq!(segments_per_logid(0), None);
    }

    #[test]
    fn wal_segment_successor_increments_and_carries() {
        // Plain increment within a logid.
        assert_eq!(wal_segment_successor((0, 3), 256), (0, 4));
        // Carry at the segs-per-logid boundary (0xFF + 1 → next logid, seg 0).
        assert_eq!(wal_segment_successor((0, 0xFF), 256), (1, 0));
        // Non-default segment size carries at its own boundary, not 0xFF.
        assert_eq!(wal_segment_successor((5, 4095), 4096), (6, 0));
        assert_eq!(wal_segment_successor((5, 4094), 4096), (5, 4095));
    }

    #[test]
    fn streaming_unrecoverable_only_when_resume_predates_leader_oldest() {
        // Resume upper bound older than the leader's oldest → provably gone.
        assert!(streaming_unrecoverable(Some((0, 3)), Some((0, 5))));
        // Upper bound == leader's oldest → the leader still has it → recoverable.
        assert!(!streaming_unrecoverable(Some((0, 5)), Some((0, 5))));
        // Upper bound newer than the oldest → present.
        assert!(!streaming_unrecoverable(Some((0, 9)), Some((0, 5))));
        // Crosses logid boundary correctly.
        assert!(streaming_unrecoverable(Some((0, 0xFF)), Some((1, 0))));
        // Any unknown → never (never wipe on a maybe).
        assert!(!streaming_unrecoverable(None, Some((0, 5))));
        assert!(!streaming_unrecoverable(Some((0, 3)), None));
        assert!(!streaming_unrecoverable(None, None));
    }

    #[test]
    fn local_newest_wal_segment_picks_max_ignoring_non_segments() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("pg_wal");
        fs::create_dir(&wal).unwrap();
        // Two real segments, plus noise that must be ignored.
        for name in [
            "000000010000000000000003",
            "000000010000000000000007", // newest by byte position
            "00000002.history",
            "000000010000000000000007.partial",
        ] {
            fs::write(wal.join(name), b"").unwrap();
        }
        fs::create_dir(wal.join("archive_status")).unwrap();
        let data_dir = dir.path().to_str().unwrap();
        assert_eq!(local_newest_wal_segment(data_dir), Some((0, 7)));
    }

    #[test]
    fn local_newest_wal_segment_empty_or_missing_is_none() {
        // Missing pg_wal entirely.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(local_newest_wal_segment(dir.path().to_str().unwrap()), None);
        // Present but holds no segment file.
        let wal = dir.path().join("pg_wal");
        fs::create_dir(&wal).unwrap();
        fs::write(wal.join("00000002.history"), b"").unwrap();
        assert_eq!(local_newest_wal_segment(dir.path().to_str().unwrap()), None);
    }
}
