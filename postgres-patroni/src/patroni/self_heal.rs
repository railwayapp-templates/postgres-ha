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
//! Three complementary, non-log-scraping signals:
//!
//! 1. **Crash-loop / divergence (this module's watcher)** — poll Patroni REST
//!    `/patroni` and watch `postmaster_start_time`. Each Postgres restart
//!    advances that timestamp; a stable postgres holds it constant. Three or
//!    more distinct values inside `recent_window_secs` is a crash loop. A
//!    replica Patroni still considers healthy but frozen behind the leader's
//!    timeline past a dwell is the silent-divergence variant.
//!
//! 1b. **Same-timeline WAL-replay stall (this module's watcher)** — the
//!    silent-divergence dwell window above is shared with a replica that
//!    never diverges onto a stale timeline at all: it stays on the SAME
//!    timeline as the leader, Patroni keeps logging "no action ... following
//!    a leader", yet the WAL cursor (`received_location`/`replayed_location`)
//!    never moves because Postgres is stuck replaying a corrupt or
//!    incomplete local WAL segment (`record with incorrect prev-link`,
//!    `invalid resource manager ID`, repeating `waiting for WAL to become
//!    available at <same LSN>` every few seconds, forever). No postmaster
//!    restart happens (so the crash-loop signal never fires) and the
//!    timeline never falls behind (so `timeline_divergence_present`'s
//!    `min_gap` guard never fires either) — this replica just never counted
//!    as unhealthy in the first place. Detected the same way as the
//!    cross-timeline case (frozen WAL cursor across the dwell window), gated
//!    on a longer dwell (`wal_stall_dwell_secs`), plus one extra requirement
//!    the cross-timeline case doesn't need: the leader's own WAL position
//!    (also polled every iteration, via a request to the leader's own
//!    `/patroni`) must have *advanced* at some point during the window. A
//!    frozen cursor on a matching timeline has one legitimate, non-broken
//!    cause a cross-timeline gap doesn't — a genuinely idle primary — and an
//!    idle primary's own WAL position is frozen too, so it can never satisfy
//!    this.
//!    Only a primary that is demonstrably still writing while the replica
//!    demonstrably isn't replaying counts as a stall.
//!
//! 2. **WAL-too-old probe ([`confirm_wal_unrecoverable`])** — for the
//!    manifestation where the replica never becomes healthy at all (so the
//!    startup-health gate in `monitoring.rs` SIGKILLs the container every
//!    `max_startup_timeout`, resetting the crash-loop counter before it trips),
//!    we don't infer from a timer or a stalled cursor. We read two segment
//!    numbers: the successor of the newest segment in the replica's own
//!    `pg_wal` — an UPPER BOUND on what it must resume streaming from (read
//!    offline, works while Postgres is down) — and a `pg_ls_waldir()` query on
//!    the leader gives the oldest segment it still retains. If the former
//!    predates the latter the replica provably cannot stream-catch-up; see the
//!    detection section comment for the full argument. On clusters without a
//!    WAL archive that verdict is final (push-only `archive_command`, no
//!    `restore_command`) and the startup gate wipes on it directly. On
//!    archiving clusters standbys self-serve missed segments through
//!    `restore_command`, so the gate additionally requires the zero-progress
//!    stall to outlive an archive-stall dwell before wiping — see
//!    `wal_reinit_confirmed` in `monitoring.rs`.
//!
//! ## Action
//! `POST /reinitialize {"force": true}` on the local Patroni REST API.
//! Patroni wipes pgdata and re-creates the replica via its configured
//! `create_replica_methods` — on archiving clusters a parallel `pgbackrest`
//! restore from the S3 archive with `pg_basebackup` off the leader as the
//! fallback; plain `pg_basebackup` elsewhere. Bypasses pg_rewind entirely —
//! the right move when rewind isn't the problem but its aftermath is.
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
use crate::major_upgrade;
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
/// Cadence for re-emitting the upgrade-standdown event while the marker
/// persists. Long enough not to spam a legitimate upgrade window, short
/// enough that a stale marker (self-heal silently disabled) keeps waving.
const STANDDOWN_REEMIT_SECS: i64 = 6 * 3600;
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
// timeline as the leader; being behind at all is abnormal. Originally set to
// 2 ("missed ≥2 promotions") as an extra false-positive guard layered on top
// of `divergence_dwell_secs`, on the theory that a single-timeline gap might
// just be a borderline in-flight switch — but the dwell (five continuous
// minutes of Patroni still reporting the replica healthy) already rules that
// out on its own: a real in-flight switch resolves in seconds, not minutes.
// Confirmed live: a replica stuck exactly 1 timeline behind (the leader
// rotated WAL past the segment it needed mid-switch) sat unrecovered for
// days, because gap=1 never met the old floor of 2 — and it was equally
// invisible to the same-timeline stall path (`stalled_same_timeline_replay`),
// which requires the timelines to match exactly. Gap=1 was double-uncovered.
// Lowered to the floor of 1 to close that band for good: gap=0 → the
// same-timeline stall path, gap>=1 → this path. This is a false-positive
// guard, not a correctness one — raise it to be stricter, never below 1.
//
// The "dwell already rules out an in-flight switch" argument above only holds
// because `accrue_divergence_window` resets the window the instant the
// LEADER's own timeline changes (`DivergenceWindow::leader_tl_at_open`), not
// just when the local node's timeline moves. Without that, a window matured
// during an unrelated idle period (gap 0, nothing behind at all) could
// survive a subsequent promotion untouched and satisfy `divergence_dwell_secs`
// on the very next poll — a gap that is seconds old reported as if it had
// been open for the full accrued dwell.
const DEFAULT_DIVERGENCE_MIN_GAP: i64 = 1;
// How long a replica's WAL cursor must sit continuously frozen on the SAME
// timeline as the leader, while Patroni still considers it healthy, before we
// treat it as a same-timeline replay stall and reinitialize. Longer than
// `DEFAULT_DIVERGENCE_DWELL_SECONDS` on purpose: unlike a cross-timeline gap
// (always abnormal), a frozen cursor on a matching timeline has one
// legitimate cause — a genuinely idle primary — that the cross-timeline case
// doesn't share. The real guard against that false positive is structural,
// not time-based: `stalled_same_timeline_replay` additionally requires the
// leader's own WAL position to have advanced during the window, which an
// idle primary's cursor never does. This dwell is a second, time-based layer
// on top of that — insurance against acting on a short-lived blip — so it
// firing in minutes rather than days (how long these replicas were observed
// silently stuck for in practice) is still a comfortable margin.
const DEFAULT_WAL_STALL_DWELL_SECONDS: u64 = 900;
// How long Patroni must continuously report "start failed" before we issue a
// reinitialize. A transient "start failed" poll is expected during a live
// pg_basebackup clone (PR #61 wipes a partial pgdata on start, then the new
// clone is in progress while Patroni's startup loop observes "start failed").
// Without this dwell, the watcher immediately re-wipes a node that is already
// cloning, compounding the loop: wipe → clone → "start failed" → wipe again.
// 180 s is long enough that a healthy clone (even on a large primary) would
// have advanced far enough for Patroni to report "creating replica" or better,
// while short enough to act promptly on a genuinely wedged node.
const DEFAULT_START_FAILED_DWELL_SECONDS: u64 = 180;

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
    /// Seconds Patroni must continuously report "start failed" before a
    /// reinitialize fires. Prevents re-wiping a node that is already cloning.
    pub start_failed_dwell_secs: u64,
    /// Seconds a replica's WAL cursor must sit frozen on the SAME timeline as
    /// the leader before a same-timeline replay-stall reinitialize fires. See
    /// [`DEFAULT_WAL_STALL_DWELL_SECONDS`] for why this is longer than
    /// `divergence_dwell_secs`.
    pub wal_stall_dwell_secs: u64,
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
            start_failed_dwell_secs: DEFAULT_START_FAILED_DWELL_SECONDS,
            wal_stall_dwell_secs: DEFAULT_WAL_STALL_DWELL_SECONDS,
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
    /// Seconds the node has been *continuously* observed with **nothing
    /// progressing** — frozen local timeline, frozen WAL cursor — while
    /// Patroni reports it healthy. `0` when not currently stalled. Shared by
    /// two decisions that differ only in what they require of the timeline
    /// gap at fire time: cross-timeline divergence (`stalled_timeline_divergence`,
    /// leader clearly ahead) and same-timeline replay stall
    /// (`stalled_same_timeline_replay`, leader on the identical timeline). The
    /// orchestrator resets this to `0` the instant the node catches up, leaves
    /// a healthy state, *or makes any forward progress* (timeline switch or
    /// WAL cursor advance), so only a true stall ever accrues.
    pub diverged_for_secs: u64,
    /// Whether the leader's own WAL position has advanced at any point during
    /// the current `diverged_for_secs` window. Only meaningful to (and only
    /// consulted by) `stalled_same_timeline_replay`: it is the proof that a
    /// frozen same-timeline replica cursor reflects a genuine replay stall
    /// rather than a primary that has simply gone idle — an idle primary's
    /// own WAL position freezes right along with a healthy replica's, so it
    /// can never make this `true`. `false` whenever the leader's progress is
    /// unmeasurable (mirrors `DivergenceObs::progress`'s "cannot prove a
    /// stall" stance for the local cursor).
    pub leader_advanced_during_stall: bool,
    /// Seconds Patroni has continuously reported this node's state as
    /// "start failed". Resets to `0` the instant the state changes. The
    /// dwell gate prevents a single transient "start failed" observation
    /// during an active pg_basebackup clone from triggering an immediate
    /// re-wipe that would abort the in-progress clone.
    pub start_failed_for_secs: u64,
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
    WalReplayStalled,
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

    let patroni_says_failed = s.patroni_state == "start failed"
        && s.start_failed_for_secs >= s.thresholds.start_failed_dwell_secs;
    let is_crash_loop = s.pg_starts_in_window >= s.thresholds.crash_loop_threshold;
    let stalled_divergence = stalled_timeline_divergence(s);
    let stalled_replay = stalled_same_timeline_replay(s);

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
    } else if let Some((local, leader)) = stalled_divergence {
        (
            ReinitTrigger::TimelineDivergence,
            format!(
                "timeline_diverged:tl{local}_behind_leader_tl{leader}_for_{}s",
                s.diverged_for_secs
            ),
        )
    } else if let Some(tl) = stalled_replay {
        (
            ReinitTrigger::WalReplayStalled,
            format!(
                "wal_replay_stalled:tl{tl}_frozen_for_{}s",
                s.diverged_for_secs
            ),
        )
    } else {
        return SelfHealAction::NoOp;
    };

    SelfHealAction::Reinitialize {
        reason,
        attempt: s.action_attempts_in_window + 1,
        trigger,
    }
}

/// The structural guards shared by both the cross-timeline-divergence and
/// same-timeline-replay-stall cases: is this node even eligible to have its
/// WAL progress judged frozen right now, independent of how long it has been
/// so or how big (if any) the timeline gap is? Shared by the decision
/// function and the pre-action re-reads so all three judge eligibility by
/// exactly the same rules.
///
/// 1. We are a `Replica` — never a leader (destructive) and never `Unknown`
///    (a joining/uninitialized node whose timeline isn't meaningful yet).
/// 2. Patroni reports us healthy (`running`/`streaming`) — never mid-clone
///    (`creating replica`/`starting`), where a lower timeline is expected.
/// 3. Both timelines are known.
///
/// Returns `(local_timeline, leader_timeline)` when eligible, else `None`.
fn replay_tracking_eligible(
    role: Role,
    patroni_state: &str,
    local_timeline: Option<i64>,
    leader_timeline: Option<i64>,
) -> Option<(i64, i64)> {
    if !matches!(role, Role::Replica) {
        return None;
    }
    if patroni_state != "running" && patroni_state != "streaming" {
        return None;
    }
    let local = local_timeline?;
    let leader = leader_timeline?;
    Some((local, leader))
}

/// Pure predicate for "this looks like a stuck-behind replica right now",
/// independent of how long it has been so. Shared by the decision function and
/// the pre-action re-read so both judge divergence by exactly the same rules.
///
/// Layers the `min_gap` requirement — leader clearly ahead, at least `min_gap`
/// timelines, not just one borderline/in-flight switch — on top of
/// [`replay_tracking_eligible`]'s three structural guards.
fn timeline_divergence_present(
    role: Role,
    patroni_state: &str,
    local_timeline: Option<i64>,
    leader_timeline: Option<i64>,
    min_gap: i64,
) -> Option<(i64, i64)> {
    let (local, leader) =
        replay_tracking_eligible(role, patroni_state, local_timeline, leader_timeline)?;
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

/// The same-timeline sibling of [`stalled_timeline_divergence`]: a replica on
/// the *identical* timeline as the leader whose WAL cursor has sat frozen past
/// `wal_stall_dwell_secs` while Patroni still reports it healthy. Unlike a
/// cross-timeline gap (always abnormal — a healthy replica never falls
/// behind on timeline), a frozen cursor on a matching timeline has one
/// legitimate cause: a genuinely idle primary. The real guard against that
/// false positive is `leader_advanced_during_stall`, not the dwell — Postgres
/// skips scheduled checkpoints entirely when idle, so `wal_stall_dwell_secs`
/// doesn't bound the false-positive window on its own (see
/// [`DEFAULT_WAL_STALL_DWELL_SECONDS`]). The canonical broken case: Postgres
/// stuck replaying a corrupt/incomplete local WAL segment, logging
/// `record with incorrect prev-link` / `invalid resource manager ID` and
/// `waiting for WAL to become available at <the same LSN>` every few seconds
/// forever, while Patroni's `/patroni` keeps reporting `running`/`streaming`
/// because the postmaster itself never crashes or restarts.
///
/// Returns `local_timeline` when it applies, else `None`. Mutually exclusive
/// with `stalled_timeline_divergence`: fires only when the timelines are
/// exactly equal. With `divergence_min_gap` at its floor of 1, every nonzero
/// gap is covered by the cross-timeline path instead, so no band is left
/// uncovered between the two.
///
/// Requires `leader_advanced_during_stall` in addition to the dwell: an idle
/// primary freezes its own WAL position exactly like a healthy replica
/// freezes its cursor, so the dwell alone can't tell the two apart. Only a
/// leader that is demonstrably still advancing while the replica isn't
/// counts as a real stall.
fn stalled_same_timeline_replay(s: &SelfHealInputs) -> Option<i64> {
    if s.diverged_for_secs < s.thresholds.wal_stall_dwell_secs {
        return None;
    }
    if !s.leader_advanced_during_stall {
        return None;
    }
    let (local, leader) = replay_tracking_eligible(
        s.role,
        &s.patroni_state,
        s.local_timeline,
        s.leader_timeline,
    )?;
    if local != leader {
        return None;
    }
    Some(local)
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
    /// The leader's own WAL position this poll (from `/cluster`), independent
    /// of the local node's progress. Used only to prove the same-timeline
    /// replay-stall case isn't actually an idle primary — see
    /// `DivergenceWindow::leader_advanced`. `None` when unmeasurable.
    leader_progress: Option<i64>,
    /// The leader's own timeline this poll (from `/cluster`). Distinct from
    /// `local_tl`'s freeze check: a promotion bumps the *leader's* timeline
    /// while the replica's `local_tl` hasn't moved yet, so watching `local_tl`
    /// alone can't detect it. See `DivergenceWindow::leader_tl_at_open`.
    leader_tl: i64,
}

/// An in-progress timeline-divergence dwell window: the epoch it opened and the
/// progress fingerprint it opened on. The dwell matures only while *both* axes
/// stay frozen, so it measures an explicit stall — never a node catching up.
#[derive(Debug, Clone, Copy)]
struct DivergenceWindow {
    since: i64,
    local_tl: i64,
    progress: Option<i64>,
    /// The leader's WAL position when this window opened. Compared against
    /// each subsequent poll's `leader_progress` to compute `leader_advanced`.
    leader_progress_at_open: Option<i64>,
    /// Sticky: once the leader's WAL position is observed strictly past
    /// `leader_progress_at_open` at any point while this window has stayed
    /// open, this latches `true` for the rest of the window's life — proof
    /// the primary was writing while the local node stayed frozen. Never
    /// un-latches within a window; only resetting the window (a fresh
    /// baseline) can clear it.
    leader_advanced: bool,
    /// The leader's own timeline when this window opened. A promotion (the
    /// leader's timeline advancing) invalidates whatever dwell was accrued
    /// against the OLD leader — that time measured how long the replica sat
    /// frozen relative to a leader that, from this poll on, no longer exists.
    /// Without this, a window that matured during an idle period (frozen
    /// local timeline + cursor, gap 0) would survive a subsequent promotion
    /// untouched, since neither `local_tl` nor `progress` need to change for
    /// the replica to suddenly be "behind" a brand-new leader — and the stale
    /// dwell could then satisfy `divergence_dwell_secs` on the very next poll,
    /// firing `TimelineDivergence` on a replica that has been behind the real
    /// (new) leader for seconds, not minutes.
    leader_tl_at_open: i64,
}

/// Advance the divergence dwell window given this poll's observation, so the
/// dwell accrues ONLY on an explicit, fully-stalled divergence — we want to be
/// certain nothing has progressed since the window opened before we wipe.
///
/// - Not diverged this poll → clear the window (caught up, mid-clone, leader
///   unknown, role unclear, or gap closed).
/// - Diverged, and the timeline, the WAL cursor, AND the leader's own timeline
///   have all stayed frozen since the window opened → keep accruing, latching
///   `leader_advanced` if the leader's own WAL position has moved past its
///   value when the window opened. This is the only path that lets the dwell
///   mature into a reinit.
/// - Diverged but the local timeline advanced, the cursor advanced, the cursor
///   is unmeasurable on either side, or the LEADER's timeline advanced (a
///   promotion) → (re)start the clock at this observation, with a fresh
///   `leader_advanced` baseline. The node may be replaying across the gap
///   (catching up), so it must not inherit prior dwell — and a promotion means
///   whatever dwell was accrued measured time against a leader that no longer
///   holds that role, so it must not be inherited either (see
///   `DivergenceWindow::leader_tl_at_open`).
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
        // Strict equality, not `<=`: a leader timeline can only legitimately
        // advance (a promotion), never regress, so any observed difference —
        // in either direction — means the baseline no longer describes the
        // current leader relationship and the window must not treat it as
        // frozen. `<=` would have silently treated a stale/regressed DCS read
        // as still-frozen instead of resetting on it.
        let leader_tl_frozen = obs.leader_tl == w.leader_tl_at_open;
        if timeline_frozen && progress_frozen && leader_tl_frozen {
            // Late-bind the leader-progress baseline: if the leader was
            // unmeasurable on the poll this window opened, the first later poll
            // that CAN measure it becomes the baseline. A cleanly-frozen stall
            // never resets the window (measurable, frozen cursor every poll),
            // so this accrual branch is the only place the baseline is ever
            // read after open — leaving it `None` here would permanently blind
            // the same-timeline stall check for the whole episode. Adopting the
            // first measurable value costs at most one extra poll before an
            // advance can latch and never resets the accrued dwell.
            let leader_progress_at_open = w.leader_progress_at_open.or(obs.leader_progress);
            let leader_advanced = w.leader_advanced
                || matches!(
                    (leader_progress_at_open, obs.leader_progress),
                    (Some(opened_at), Some(cur)) if cur > opened_at
                );
            return Some(DivergenceWindow {
                leader_progress_at_open,
                leader_advanced,
                ..w
            });
        }
    }
    Some(DivergenceWindow {
        since: now,
        local_tl: obs.local_tl,
        progress: obs.progress,
        leader_progress_at_open: obs.leader_progress,
        leader_advanced: false,
        leader_tl_at_open: obs.leader_tl,
    })
}

/// The leader timeline to feed into this poll's dwell-window accrual: this
/// poll's own reading (`fresh`) when available, else the existing window's
/// baseline (`leader_tl_at_open`) as a stand-in. A real reading always wins
/// over the fallback, including one that differs from the baseline — that
/// case still reaches `accrue_divergence_window`, whose `leader_tl_frozen`
/// check (exact equality) resets the window on it exactly as before. This
/// only softens the case where the leader probe couldn't measure anything
/// at all this poll, so a single transient miss doesn't cost the whole dwell.
fn leader_timeline_for_accrual(
    fresh: Option<i64>,
    window: Option<&DivergenceWindow>,
) -> Option<i64> {
    fresh.or_else(|| window.map(|w| w.leader_tl_at_open))
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
// read WAL segment numbers directly and compare them: a replica is
// unrecoverable-by-STREAMING exactly when the segment it must resume streaming
// from is older than everything the leader still retains. On clusters without
// a WAL archive (no restore_command; archive_command is push-only) streaming
// is the standby's only WAL source, so that verdict is final. On archiving
// clusters yaml.rs installs restore_command on every standby, which self-serves
// missed segments from the S3 archive — Postgres retries it on its own, so a
// positive verdict there usually coincides with recovery quietly fixing
// itself (and that surfaces as replay progress, which resets the startup gate
// before it ever probes). The gate therefore wipes on the verdict directly
// only where it is final, and on archiving clusters additionally requires the
// zero-progress stall to outlive a confirmation dwell — at which point the
// archive path is provably stalled too (a WAL gap in the archive, a stale repo
// path, or an object-store outage longer than the dwell); see
// `wal_reinit_confirmed` in monitoring.rs.
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
/// retains. On clusters without a WAL archive that alone means unrecoverable
/// (streaming is the only WAL source); on archiving clusters standbys can still
/// self-serve the gap through restore_command, so callers gate the destructive
/// reinit behind an archive-stall dwell on top of this verdict
/// (`wal_reinit_confirmed` in monitoring.rs). For the streaming half this is a
/// SUFFICIENT condition: it never fires on a replica a restart could recover
/// into streaming (no false positives), at the cost of possibly missing a
/// genuine case in a narrow boundary band (a safe false negative — the node
/// keeps restart-looping as it does today). Any uncertainty — pg_wal/control
/// file unreadable, no leader endpoint, leader query failed, or the needed
/// segment is still present — returns `false`. See the section comment for the
/// full argument. Replaces every prior heuristic (stalled cursor, stderr
/// scraping, dwell timers, REDO lower-bound) for the "replica never becomes
/// healthy" manifestation.
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
            archive_fallback = config.wal_archive_bucket.is_some(),
            "self-heal: the segment this replica must resume from is older than the leader's oldest retained WAL — provably unrecoverable by streaming"
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
        start_failed_dwell_secs = cfg.thresholds.start_failed_dwell_secs,
        wal_stall_dwell_secs = cfg.thresholds.wal_stall_dwell_secs,
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
                start_failed_dwell_secs: env_u64(
                    "SELF_HEAL_START_FAILED_DWELL_SECONDS",
                    DEFAULT_START_FAILED_DWELL_SECONDS,
                ),
                wal_stall_dwell_secs: env_u64(
                    "SELF_HEAL_WAL_STALL_DWELL_SECONDS",
                    DEFAULT_WAL_STALL_DWELL_SECONDS,
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
    // Epoch when Patroni first reported "start failed" in the current
    // continuous run. Resets to None the instant the state changes.
    // In-memory: a container restart implies Patroni restarted too, so the
    // "start failed" observation is fresh and the dwell clock correctly
    // restarts from zero.
    let mut start_failed_since: Option<i64> = None;
    // Dedupe the upgrade-standdown telemetry: the marker sits on the volume
    // for the whole upgrade window, so without a latch every poll would emit.
    // Epoch of the last emit; re-emits every STANDDOWN_REEMIT_SECS while the
    // episode persists (a marker a boot failed to unlink can outlive any
    // upgrade window by months, and self-heal is silently off the whole time
    // — a single event at the start is too easy to lose). Cleared once the
    // marker is gone, so a later window emits afresh.
    let mut upgrade_standdown_last_emit: Option<i64> = None;

    loop {
        if let Err(e) = iteration(
            &volume_root,
            &client,
            &cfg,
            &state_path,
            &mut starts_seen,
            &mut action_pending_recovery,
            &mut gave_up_emitted,
            &mut divergence_window,
            &mut start_failed_since,
            &mut upgrade_standdown_last_emit,
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
    volume_root: &str,
    client: &reqwest::Client,
    cfg: &WatcherConfig,
    state_path: &str,
    starts_seen: &mut VecDeque<(i64, String)>,
    action_pending_recovery: &mut Option<i64>,
    gave_up_emitted: &mut bool,
    divergence_window: &mut Option<DivergenceWindow>,
    start_failed_since: &mut Option<i64>,
    upgrade_standdown_last_emit: &mut Option<i64>,
    telemetry: &Telemetry,
) -> Result<()> {
    let now = now_epoch();

    // A major upgrade owns this volume for its window, and this watcher is the
    // one actor here that a Patroni DCS pause does NOT stop — the control plane
    // pauses failover for the upgrade, and without this check we would keep
    // POSTing /reinitialize anyway. A replica reinitialized while the leader is
    // mid-upgrade wipes itself and then cannot clone at all: pg_basebackup
    // refuses across majors, so it ends up empty rather than rebuilt. Every
    // symptom this watcher looks for (crash loops, "start failed", a timeline
    // behind the leader) is EXPECTED while the cluster's leader is being
    // upgraded, so the whole iteration stands down. In production choreography
    // the marker that engages this is the `reseed` marker the HA workflow
    // writes onto each replica's volume before pausing failover.
    let upgrade_marker = major_upgrade::read_marker(volume_root);
    if upgrade_marker.as_ref().is_some_and(|m| !m.is_completed()) {
        // Clear the transient dwell state so none of it accrues across the
        // unobserved window: a replica that spent the whole upgrade lagging
        // must re-earn its divergence/start-failed dwell from zero once the
        // marker is gone, not inherit a matured one and get wiped instantly.
        *divergence_window = None;
        *start_failed_since = None;
        // Re-emit on a slow cadence, with the marker's phase and age: a live
        // upgrade window and a stale marker a boot failed to unlink (which
        // disables self-heal indefinitely — see the runner's
        // report_marker_removal_failure) look identical from here, and the
        // age is what lets the fleet view tell them apart.
        if upgrade_standdown_last_emit
            .is_none_or(|last| now - last >= STANDDOWN_REEMIT_SECS)
        {
            let phase = upgrade_marker
                .and_then(|m| m.phase)
                .unwrap_or_else(|| "unreadable".to_string());
            let marker_age_secs = major_upgrade::marker_age_secs(volume_root);
            info!(
                phase = %phase,
                marker_age_secs = ?marker_age_secs,
                "self-heal: major upgrade in progress on this volume — standing down"
            );
            telemetry.send(TelemetryEvent::SelfHealUpgradeStanddown {
                node: env::var("PATRONI_NAME").unwrap_or_else(|_| "unknown".to_string()),
                phase,
                marker_age_secs,
            });
            *upgrade_standdown_last_emit = Some(now);
        }
        return Ok(());
    }
    *upgrade_standdown_last_emit = None;

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
        // Leaders are never acted on. Clear any dwell window accrued from a
        // former life as a replica — otherwise a promoted node keeps carrying
        // a matured (or maturing) window that would apply to whatever replica
        // it demotes back into, measured against a leader relationship that no
        // longer holds. Only an actual demotion (which always regresses
        // `local_tl` below the new leader's) would otherwise be relied on to
        // reset it; clearing here makes that explicit instead of incidental.
        *divergence_window = None;
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

    // 3b. Accrue the WAL-progress dwell against ANY structurally-eligible
    // replica (replica role, healthy state, both timelines known) regardless
    // of whether there's currently a timeline gap — `replay_tracking_eligible`
    // deliberately omits the `min_gap` floor `timeline_divergence_present`
    // applies, so this same window also matures for a replica stuck on the
    // SAME timeline as the leader (the WAL-replay-stall case), which a
    // min_gap-gated observation would never see in the first place. Any
    // forward progress (timeline switch or WAL cursor advance) restarts the
    // clock — a node replaying across a gap is catching up, not wedged — and
    // any ineligible observation (caught up, mid-clone, leader unknown, role
    // unclear) clears it. `decide_self_heal` re-examines the timeline gap at
    // fire time to pick cross-timeline divergence vs same-timeline stall, each
    // gated on its own (different) dwell threshold.
    let patroni_state = local.state.clone().unwrap_or_default();

    // Track how long Patroni has continuously reported "start failed". Reset
    // the clock the instant the state changes — only an unbroken run accrues.
    if patroni_state == "start failed" {
        if start_failed_since.is_none() {
            *start_failed_since = Some(now);
        }
    } else {
        *start_failed_since = None;
    }
    let start_failed_for_secs = start_failed_since
        .map(|t| now.saturating_sub(t).max(0) as u64)
        .unwrap_or(0);

    // Highest WAL position Patroni reports for this node — the axis (besides the
    // timeline) on which a catching-up replica makes visible progress.
    let local_progress = local.xlog.and_then(|x| x.highest_location());
    // A single missed leader-timeline read (a transient `/cluster` blip) would
    // otherwise force `current_obs` to `None` below and clear the whole dwell
    // window — paying for one flaky poll with a full re-accrual (up to
    // `wal_stall_dwell_secs`, several minutes, before a real stall can fire
    // again). For accrual purposes only, fall back via
    // `leader_timeline_for_accrual` to the window's existing leader-timeline
    // baseline so a single miss holds the window rather than wiping it. This
    // does NOT affect the decision snapshot below —
    // `SelfHealInputs::leader_timeline` stays this poll's real (possibly
    // `None`) reading, so a reinit can only ever fire on a poll that actually
    // measured the leader.
    let leader_tl_for_accrual =
        leader_timeline_for_accrual(leader_timeline, divergence_window.as_ref());
    let current_obs =
        replay_tracking_eligible(role, &patroni_state, local_timeline, leader_tl_for_accrual).map(
            |(local_tl, leader_tl)| DivergenceObs {
                local_tl,
                progress: local_progress,
                leader_progress: leader.progress,
                leader_tl,
            },
        );
    *divergence_window = accrue_divergence_window(current_obs, *divergence_window, now);
    let diverged_for_secs = divergence_window
        .map(|w| now.saturating_sub(w.since).max(0) as u64)
        .unwrap_or(0);
    let leader_advanced_during_stall = divergence_window
        .map(|w| w.leader_advanced)
        .unwrap_or(false);
    // The dwell window's frozen cursor baseline, threaded into the pre-action
    // re-checks (`confirm_timeline_divergence`/`confirm_replay_stall`) so they
    // can catch a cursor that has quietly advanced since the last regular
    // poll — not just re-verify the structural gap/timeline shape.
    let frozen_progress = divergence_window.and_then(|w| w.progress);

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
        leader_advanced_during_stall,
        start_failed_for_secs,
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
            // Double-check destructive divergence/stall wipes against an
            // independent, fresh read taken right now — guards against acting
            // on a single stale or flaky poll. Crash-loop and start-failed are
            // evidenced across many polls already, so they skip this. If the
            // re-read no longer agrees, the state changed under us: reset the
            // dwell and wait for it to re-accrue rather than wipe on stale data.
            let recheck_failed = match trigger {
                ReinitTrigger::TimelineDivergence => {
                    !confirm_timeline_divergence(client, cfg, frozen_progress).await
                }
                ReinitTrigger::WalReplayStalled => {
                    !confirm_replay_stall(client, cfg, frozen_progress).await
                }
                ReinitTrigger::StartFailed | ReinitTrigger::CrashLoop => false,
            };
            if recheck_failed {
                // Skip this cycle without consuming an attempt or starting
                // backoff. We deliberately leave the dwell window alone: if
                // the node genuinely caught up or made progress, the next
                // iteration's top-level accrual resets it; if this was just a
                // flaky read, the dwell stands and we retry next poll instead of
                // paying a full re-accrual.
                info!(
                    reason = %reason,
                    ?trigger,
                    "self-heal: re-check did not confirm on fresh read, skipping reinit"
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
/// the node is making progress and freeze when it stalls. On a leader, Patroni
/// instead reports `location` — the primary's own current WAL write position.
#[derive(Debug, Deserialize, Clone, Copy)]
struct Xlog {
    location: Option<i64>,
    received_location: Option<i64>,
    replayed_location: Option<i64>,
}

impl Xlog {
    /// Highest position observed across all reported cursors — the most
    /// generous "is it progressing" reading, so any advance on any of them
    /// resets the dwell.
    fn highest_location(self) -> Option<i64> {
        [
            self.location,
            self.received_location,
            self.replayed_location,
        ]
        .into_iter()
        .flatten()
        .max()
    }
}

async fn fetch_patroni_status(
    client: &reqwest::Client,
    base_url: &str,
    timeout: Option<Duration>,
) -> Result<PatroniLocal> {
    let mut req = client.get(base_url);
    if let Some(timeout) = timeout {
        req = req.timeout(timeout);
    }
    let resp = req.send().await?;
    // Patroni returns 200 for leaders, 503 for non-leaders, but the JSON
    // body is identical in shape and we want the body either way.
    let body = resp.text().await?;
    let parsed: PatroniLocal = serde_json::from_str(&body)?;
    Ok(parsed)
}

async fn fetch_local_patroni(client: &reqwest::Client) -> Result<PatroniLocal> {
    fetch_patroni_status(client, PATRONI_PATRONI_URL, None).await
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
/// reachable and healthy (so a reinit has a clone source), its current
/// timeline (so we can spot a replica stuck behind it), and its own WAL
/// position (so we can tell a genuinely idle primary apart from a replica
/// that has stopped replaying — see `DivergenceObs::leader_progress`).
#[derive(Debug, Default, Clone, Copy)]
struct LeaderProbe {
    reachable: bool,
    timeline: Option<i64>,
    progress: Option<i64>,
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
    // check needs it, and a leader that fails the reachability probe still
    // pins the timeline a stuck replica should be measured against.
    let timeline = leader.timeline;
    if leader.state != "running" {
        return LeaderProbe {
            reachable: false,
            timeline,
            progress: None,
        };
    }
    let Some(api_url) = leader.api_url.as_ref() else {
        return LeaderProbe {
            reachable: false,
            timeline,
            progress: None,
        };
    };
    let api_url = api_url.trim_end_matches('/');
    // A single `/patroni` request against the leader doubles as both the
    // reachability probe and the WAL-position read, rather than a separate
    // `/health` round-trip plus a `/patroni` one every poll: `state ==
    // "running"` mirrors `/health`'s 200/healthy vs 503/unhealthy semantics
    // (Postgres up and accepting connections), and the same response body
    // already carries the WAL position we need. Reuse the shared client with
    // a per-request timeout override so we keep the connection pool warm
    // across polls. Best-effort: any failure (timeout, non-JSON body) reads
    // as unreachable with no progress, never fatal to the probe.
    let leader_status = fetch_patroni_status(
        client,
        &format!("{api_url}/patroni"),
        Some(Duration::from_secs(timeout_secs)),
    )
    .await
    .ok();
    let reachable = matches!(
        leader_status.as_ref().and_then(|p| p.state.as_deref()),
        Some("running")
    );
    let progress = leader_status
        .and_then(|p| p.xlog)
        .and_then(|x| x.highest_location());
    LeaderProbe {
        reachable,
        timeline,
        progress,
    }
}

/// `true` if a fresh local WAL cursor read shows measurable progress past
/// `frozen_progress` — i.e. the node made progress since the polled dwell
/// window's baseline was last observed. `None` on either side means
/// unmeasurable, which is never treated as "advanced" (mirrors the accrual
/// function's own "cannot prove a stall, cannot prove an advance either"
/// stance).
fn progress_advanced_past(fresh: Option<i64>, frozen_progress: Option<i64>) -> bool {
    matches!((frozen_progress, fresh), (Some(opened_at), Some(cur)) if cur > opened_at)
}

/// Independent re-confirmation of a timeline divergence taken immediately
/// before the destructive reinit. Does its own fresh reads of `/patroni`
/// (local) and `/cluster` (leader) — not the snapshot the decision was made
/// from — then re-applies the exact structural guards via
/// [`timeline_divergence_present`] plus an explicit active-leader check
/// (`reachable`, i.e. leader present, `running`, and its own `/patroni` state
/// also `running`), plus a direct check that the local WAL cursor hasn't
/// advanced past `frozen_progress` (the dwell window's frozen baseline) — the
/// multi-poll accrual proves the cursor was frozen as of the LAST regular poll, but not
/// as of this instant, and the structural gap check alone can't see cursor
/// movement on a timeline that hasn't crossed a switchpoint yet. Any
/// disagreement returns `false` and the caller backs off. This is the final
/// safety net against acting on a single stale or flaky poll.
async fn confirm_timeline_divergence(
    client: &reqwest::Client,
    cfg: &WatcherConfig,
    frozen_progress: Option<i64>,
) -> bool {
    let Ok(local) = fetch_local_patroni(client).await else {
        return false;
    };
    let leader = probe_leader(client, cfg.leader_health_timeout_secs).await;
    if !leader.reachable {
        return false;
    }
    if progress_advanced_past(
        local.xlog.and_then(|x| x.highest_location()),
        frozen_progress,
    ) {
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

// How long `confirm_replay_stall` waits between its own two cursor reads.
// Guards against confirming a stall right as an asymmetric partition heals:
// the replica's walreceiver can take a few seconds to reconnect after the
// leader becomes reachable again, during which a single fresh read still
// looks frozen even though the replica is about to resume on its own.
// Matches Postgres's own default `wal_retrieve_retry_interval` (5s) — the
// interval a genuinely healed replica needs to retry the connection.
const REPLAY_STALL_RECHECK_DELAY_SECS: u64 = 5;

/// Independent fresh-read re-check for the same-timeline replay-stall
/// trigger, mirroring [`confirm_timeline_divergence`]. Re-verifies the
/// structural conditions (replica, healthy state, leader reachable, timelines
/// known and exactly equal) on a brand-new poll, plus that the local cursor
/// hasn't advanced past `frozen_progress` since the dwell window's baseline,
/// rather than re-proving the dwell itself was frozen for its whole duration
/// or that the leader genuinely advanced during it — the multi-poll accrual
/// already established both; this just guards against acting on a single
/// stale/flaky read.
///
/// Also takes a *second* cursor read `REPLAY_STALL_RECHECK_DELAY_SECS` later
/// and aborts if it moved past the first: the structural/dwell checks above
/// only rule out a stale poll, not a replica whose walreceiver reconnects
/// between this function's own first read and the reinit that would follow
/// it — the exact shape of an asymmetric-partition heal landing mid-recheck.
async fn confirm_replay_stall(
    client: &reqwest::Client,
    cfg: &WatcherConfig,
    frozen_progress: Option<i64>,
) -> bool {
    let Ok(local) = fetch_local_patroni(client).await else {
        return false;
    };
    let leader = probe_leader(client, cfg.leader_health_timeout_secs).await;
    if !leader.reachable {
        return false;
    }
    let first_progress = local.xlog.and_then(|x| x.highest_location());
    if progress_advanced_past(first_progress, frozen_progress) {
        return false;
    }
    match replay_tracking_eligible(
        parse_role(local.role.as_deref()),
        &local.state.unwrap_or_default(),
        local.timeline,
        leader.timeline,
    ) {
        Some((l, r)) if l == r => {}
        _ => return false,
    }
    sleep(Duration::from_secs(REPLAY_STALL_RECHECK_DELAY_SECS)).await;
    let Ok(recheck_local) = fetch_local_patroni(client).await else {
        return false;
    };
    let second_progress = recheck_local.xlog.and_then(|x| x.highest_location());
    !progress_advanced_past(second_progress, first_progress)
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
            leader_advanced_during_stall: false,
            start_failed_for_secs: 0,
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
    fn patroni_start_failed_triggers_reinit_after_dwell() {
        let mut s = base();
        s.patroni_state = "start failed".into();
        s.start_failed_for_secs = s.thresholds.start_failed_dwell_secs;
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
    fn start_failed_below_dwell_is_noop() {
        // "start failed" observed but dwell not yet reached — a normal
        // pg_basebackup clone is in progress and must not be interrupted.
        let mut s = base();
        s.patroni_state = "start failed".into();
        s.start_failed_for_secs = s.thresholds.start_failed_dwell_secs - 1;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn start_failed_at_zero_dwell_acts_immediately() {
        // When the operator sets the dwell to 0, revert to the old eager behaviour.
        let mut s = base();
        s.patroni_state = "start failed".into();
        s.start_failed_for_secs = 0;
        s.thresholds.start_failed_dwell_secs = 0;
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize {
                trigger: ReinitTrigger::StartFailed,
                ..
            } => {}
            other => panic!("expected Reinitialize(StartFailed), got {other:?}"),
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
    fn divergence_at_gap_zero_is_noop_for_divergence_path() {
        // Gap 0 (timelines equal) is never "behind" — that shape belongs to
        // the same-timeline stall path (`stalled_same_timeline_replay`), not
        // this one.
        let mut s = diverged();
        s.local_timeline = Some(7);
        s.leader_timeline = Some(7); // gap 0 < default min_gap 1
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn divergence_at_new_min_gap_of_one_acts() {
        // Regression pin for the exact bug this closes: a replica stuck
        // exactly 1 timeline behind the leader (WAL segment recycled before
        // it could complete the switch) sat unrecovered for days in
        // production because the old min_gap of 2 ignored gap=1 entirely —
        // and the same-timeline stall path requires gap=0, so it was
        // double-uncovered. Confirms gap=1 now acts at the lowered floor.
        let mut s = diverged();
        s.local_timeline = Some(6);
        s.leader_timeline = Some(7); // gap 1 == new default min_gap
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize {
                trigger: ReinitTrigger::TimelineDivergence,
                ..
            } => {}
            other => panic!("expected divergence Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn divergence_above_min_gap_acts() {
        let mut s = diverged();
        s.local_timeline = Some(5);
        s.leader_timeline = Some(7); // gap 2 > default min_gap 1
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

    /// A replica healthy in Patroni's eyes, on the SAME timeline as the
    /// leader, whose WAL cursor has sat frozen past the (longer)
    /// wal-replay-stall dwell, with the leader independently proven to have
    /// kept advancing during that window — the same-timeline sibling of
    /// `diverged()`.
    fn same_timeline_stalled() -> SelfHealInputs {
        let mut s = base();
        s.patroni_state = "running".into();
        s.local_timeline = Some(4);
        s.leader_timeline = Some(4);
        s.diverged_for_secs = s.thresholds.wal_stall_dwell_secs;
        s.leader_advanced_during_stall = true;
        s
    }

    #[test]
    fn same_timeline_replay_stall_triggers_reinit() {
        let s = same_timeline_stalled();
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize {
                reason,
                attempt: 1,
                trigger,
            } => {
                assert!(reason.starts_with("wal_replay_stalled"), "got {reason}");
                assert!(reason.contains("tl4"), "got {reason}");
                assert_eq!(trigger, ReinitTrigger::WalReplayStalled);
            }
            other => panic!("expected Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn same_timeline_stall_below_dwell_is_noop() {
        let mut s = same_timeline_stalled();
        s.diverged_for_secs = s.thresholds.wal_stall_dwell_secs - 1;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn same_timeline_stall_below_divergence_dwell_is_also_noop() {
        // Regression guard for the exact bug this fixes: previously a frozen
        // same-timeline replica was invisible no matter how long it stalled,
        // because `current_obs` required a `min_gap` timeline gap that a
        // same-timeline replica never has. Below `wal_stall_dwell_secs` it
        // must still be a NoOp (the dwell hasn't matured) — this only
        // confirms it isn't ALSO short-circuited by the (shorter, unrelated)
        // divergence dwell.
        let mut s = same_timeline_stalled();
        s.diverged_for_secs = s.thresholds.divergence_dwell_secs;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn same_timeline_stall_requires_replica_role() {
        let mut s = same_timeline_stalled();
        s.role = Role::Unknown;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn same_timeline_stall_only_acts_in_healthy_state() {
        let mut s = same_timeline_stalled();
        s.patroni_state = "creating replica".into();
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn same_timeline_stall_requires_leader_reachable() {
        let mut s = same_timeline_stalled();
        s.leader_reachable = false;
        assert_eq!(decide_self_heal(&s), SelfHealAction::Wait);
    }

    #[test]
    fn same_timeline_stall_never_acts_on_leader() {
        let mut s = same_timeline_stalled();
        s.role = Role::Leader;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn idle_primary_never_triggers_wal_replay_stall() {
        // The false-positive this fix closes: a replica frozen on the same
        // timeline past the dwell, but the leader's own WAL position never
        // moved either during that window — a genuinely idle primary, not a
        // stall. Without `leader_advanced_during_stall` gating the trigger,
        // this would previously fire `WalReplayStalled` and destructively
        // reinitialize a perfectly healthy, merely-quiet replica.
        let mut s = same_timeline_stalled();
        s.leader_advanced_during_stall = false;
        assert_eq!(decide_self_heal(&s), SelfHealAction::NoOp);
    }

    #[test]
    fn actual_timeline_divergence_takes_priority_over_wal_stall_reason() {
        // A real cross-timeline divergence that has ALSO cleared the (longer)
        // wal-stall dwell must still report as TimelineDivergence, not
        // WalReplayStalled — the two are mutually exclusive by gap, but this
        // pins the tie-break explicitly in case that ever changes.
        let mut s = diverged();
        s.diverged_for_secs = s.thresholds.wal_stall_dwell_secs;
        match decide_self_heal(&s) {
            SelfHealAction::Reinitialize {
                trigger: ReinitTrigger::TimelineDivergence,
                ..
            } => {}
            other => panic!("expected TimelineDivergence Reinitialize, got {other:?}"),
        }
    }

    #[test]
    fn leader_timeline_for_accrual_falls_back_to_window_baseline_on_a_missed_read() {
        // Regression pin for the flaky-poll blind spot: a single transient
        // `/cluster` miss (fresh = None) must not itself wipe the window —
        // it should fall back to the existing baseline as a stand-in.
        let w = DivergenceWindow {
            since: 1_000,
            local_tl: 3,
            progress: Some(100),
            leader_progress_at_open: Some(500),
            leader_advanced: false,
            leader_tl_at_open: 7,
        };
        assert_eq!(leader_timeline_for_accrual(None, Some(&w)), Some(7));
        // A real reading always wins over the fallback, even one that
        // differs from the baseline — that case still reaches
        // `accrue_divergence_window`, which resets on the mismatch itself.
        assert_eq!(leader_timeline_for_accrual(Some(9), Some(&w)), Some(9));
        // No window open yet and nothing fresh → nothing to fall back to.
        assert_eq!(leader_timeline_for_accrual(None, None), None);
    }

    #[test]
    fn progress_advanced_past_requires_strict_increase_past_a_measurable_baseline() {
        // Strictly past the baseline → advanced.
        assert!(progress_advanced_past(Some(101), Some(100)));
        // Equal to the baseline → still frozen, not an advance.
        assert!(!progress_advanced_past(Some(100), Some(100)));
        // Behind the baseline (shouldn't happen for a monotonic WAL cursor,
        // but must not be misread as an advance either way).
        assert!(!progress_advanced_past(Some(99), Some(100)));
        // Either side unmeasurable → cannot prove an advance.
        assert!(!progress_advanced_past(None, Some(100)));
        assert!(!progress_advanced_past(Some(100), None));
        assert!(!progress_advanced_past(None, None));
    }

    // Sentinel leader timeline held constant across every poll in tests that
    // don't care about promotions — keeps `leader_tl_frozen` trivially true so
    // these helpers don't perturb the many pre-existing tests that only vary
    // local_tl/progress.
    const UNCHANGING_LEADER_TL: i64 = 1;

    fn obs(local_tl: i64, progress: Option<i64>) -> Option<DivergenceObs> {
        obs_with_leader(local_tl, progress, None)
    }

    fn obs_with_leader(
        local_tl: i64,
        progress: Option<i64>,
        leader_progress: Option<i64>,
    ) -> Option<DivergenceObs> {
        obs_full(local_tl, progress, UNCHANGING_LEADER_TL, leader_progress)
    }

    fn obs_full(
        local_tl: i64,
        progress: Option<i64>,
        leader_tl: i64,
        leader_progress: Option<i64>,
    ) -> Option<DivergenceObs> {
        Some(DivergenceObs {
            local_tl,
            progress,
            leader_progress,
            leader_tl,
        })
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
    fn leader_advance_latches_while_local_stays_frozen() {
        // Local timeline+cursor frozen across all three polls (a real stall),
        // but the leader's own WAL position climbs on the second poll — proof
        // the primary is alive and writing while the replica isn't replaying.
        let w0 = accrue_divergence_window(obs_with_leader(3, Some(100), Some(500)), None, 1_000)
            .unwrap();
        assert!(!w0.leader_advanced, "no leader movement observed yet");
        let w1 =
            accrue_divergence_window(obs_with_leader(3, Some(100), Some(600)), Some(w0), 1_060)
                .unwrap();
        assert!(
            w1.leader_advanced,
            "leader progress past the window's baseline must latch"
        );
        // Latches sticky even if a later poll can't measure leader progress.
        let w2 =
            accrue_divergence_window(obs_with_leader(3, Some(100), None), Some(w1), 1_120).unwrap();
        assert!(w2.leader_advanced, "leader_advanced must stay latched");
    }

    #[test]
    fn idle_leader_never_latches_leader_advanced() {
        // The false-positive case this guards against: local AND leader both
        // frozen the whole window (a genuinely idle primary, not a stall).
        let w0 = accrue_divergence_window(obs_with_leader(3, Some(100), Some(500)), None, 1_000)
            .unwrap();
        let w1 =
            accrue_divergence_window(obs_with_leader(3, Some(100), Some(500)), Some(w0), 1_060)
                .unwrap();
        assert!(
            !w1.leader_advanced,
            "an idle primary's own WAL position never advances, so this must never latch"
        );
    }

    #[test]
    fn window_reset_clears_leader_advanced_baseline() {
        // Local makes forward progress (catching up) → window resets, and the
        // new window must not inherit the old baseline's latched state.
        let w0 = accrue_divergence_window(obs_with_leader(3, Some(100), Some(500)), None, 1_000)
            .unwrap();
        let w1 =
            accrue_divergence_window(obs_with_leader(3, Some(100), Some(600)), Some(w0), 1_060)
                .unwrap();
        assert!(w1.leader_advanced);
        let w2 =
            accrue_divergence_window(obs_with_leader(3, Some(200), Some(600)), Some(w1), 1_120)
                .unwrap();
        assert_eq!(w2.since, 1_120, "local progress must reset the window");
        assert!(
            !w2.leader_advanced,
            "a fresh window must start with a fresh leader_advanced baseline"
        );
    }

    #[test]
    fn leader_progress_baseline_late_binds_when_unmeasurable_at_open() {
        // The window opens on a poll where the leader's WAL position couldn't
        // be measured (a transient blip). A cleanly-frozen stall never resets
        // the window, so if the baseline stayed `None` forever the leader
        // could never be proven to advance and the stall would go undetected
        // for the whole episode. Instead, the first later poll that CAN
        // measure the leader adopts the baseline — without resetting the dwell
        // — and a subsequent advance past it still latches.
        let w0 =
            accrue_divergence_window(obs_with_leader(3, Some(100), None), None, 1_000).unwrap();
        assert_eq!(
            w0.leader_progress_at_open, None,
            "no leader baseline is captured when the leader is unmeasurable at open"
        );
        assert!(!w0.leader_advanced);

        // Leader now measurable at 500 → adopt as the baseline. Not itself an
        // advance (this IS the baseline), and the dwell clock must not reset.
        let w1 =
            accrue_divergence_window(obs_with_leader(3, Some(100), Some(500)), Some(w0), 1_060)
                .unwrap();
        assert_eq!(
            w1.leader_progress_at_open,
            Some(500),
            "first measurable leader progress must become the baseline"
        );
        assert!(
            !w1.leader_advanced,
            "adopting the baseline is not itself an advance"
        );
        assert_eq!(
            w1.since, 1_000,
            "adopting a late baseline must NOT reset the accrued dwell"
        );

        // Leader past the adopted baseline → latch, proving the primary is
        // writing while the replica stays frozen.
        let w2 =
            accrue_divergence_window(obs_with_leader(3, Some(100), Some(600)), Some(w1), 1_120)
                .unwrap();
        assert!(
            w2.leader_advanced,
            "an advance past the adopted baseline must latch"
        );
    }

    #[test]
    fn leader_promotion_resets_window_despite_frozen_local_state() {
        // Regression pin for the idle-cluster-then-failover race: a window can
        // mature for a long time at gap 0 (replica healthy, same timeline as
        // leader, both frozen because the cluster is simply idle). If the
        // leader is then unexpectedly replaced (unclean failover) and the new
        // leader's timeline appears in `/cluster`, the replica's `local_tl`
        // and cursor haven't moved at all yet — a naive freeze check would
        // treat that as "still frozen" and let the window keep its stale
        // accrued dwell, letting `TimelineDivergence` fire on the very next
        // poll even though the replica has been behind the new leader for
        // seconds, not `divergence_dwell_secs`. The leader's own timeline must
        // independently gate the freeze.
        let w0 = accrue_divergence_window(obs_full(3, Some(100), 5, None), None, 1_000).unwrap();
        assert_eq!(w0.leader_tl_at_open, 5);
        let w1 =
            accrue_divergence_window(obs_full(3, Some(100), 5, None), Some(w0), 100_000).unwrap();
        assert_eq!(
            w1.since, 1_000,
            "same leader timeline, still frozen -> dwell keeps accruing"
        );

        // Leader promotes to timeline 6. Local timeline (3) and cursor (100)
        // are unchanged from the prior poll -- a check that only watched those
        // two axes would wrongly call this "still frozen".
        let w2 =
            accrue_divergence_window(obs_full(3, Some(100), 6, None), Some(w1), 100_010).unwrap();
        assert_eq!(
            w2.since, 100_010,
            "a leader promotion must reset the window to a fresh epoch, discarding stale dwell"
        );
        assert_eq!(w2.leader_tl_at_open, 6);
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
