//! Major-version upgrade guards.
//!
//! An in-place major upgrade is driven from outside this image: the control
//! plane stops the member, runs a one-shot job image that carries both majors'
//! binaries against this same volume, and repins the service image. The job
//! writes `.railway-major-upgrade.json` at the VOLUME ROOT (not in PGDATA,
//! which pg_upgrade replaces wholesale) and that marker is the commit point:
//!
//!   absent               nothing in flight
//!   phase == "upgraded"  pg_upgrade succeeded, the directory swap may be
//!                        incomplete — the data directory can be missing
//!   phase == "completed" the swap is done; the TARGET major's image boots
//!   phase == "reseed"    written by the HA workflow onto each REPLICA's
//!                        volume before it pauses failover: boot is ALLOWED,
//!                        and if the on-disk major differs from the image's
//!                        the runner wipes pgdata (with the same safety
//!                        predicate as the incomplete-clone wipe) and deletes
//!                        the marker so Patroni re-clones from the upgraded
//!                        leader. Without this phase the version-mismatch
//!                        guard below would refuse the exact boot the reseed
//!                        depends on.
//!
//! Three things in this image will destroy data if they run during that
//! window, and none of them can see the control plane:
//!
//!   1. Booting at all mid-swap. PGDATA may be absent or half-promoted;
//!      Patroni would treat it as a fresh member and bootstrap over it.
//!   2. Booting the wrong major. Postgres refuses eventually, but only deep
//!      into startup, and `should_wipe_incomplete_clone` may wipe first.
//!   3. The self-heal watcher's `/reinitialize`. It is *not* stopped by a
//!      Patroni DCS pause — the control plane pauses failover for the
//!      upgrade window, and this watcher keeps acting anyway. A replica
//!      reinitialized while the leader is mid-upgrade wipes itself and then
//!      cannot clone: pg_basebackup refuses across majors.
//!
//! So every one of them consults the marker, which is on the volume and needs
//! no network call. Fail-stop and loud beats clever recovery here.

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use serde::Deserialize;
use std::fs::{File, OpenOptions};

pub const MARKER_FILENAME: &str = ".railway-major-upgrade.json";

/// Marker phase written by the HA workflow onto replica volumes; see the
/// module doc. Boot is allowed and the runner consumes the marker.
pub const PHASE_RESEED: &str = "reseed";

/// Terminal phase: the swap is done, the target major's image may boot.
pub const PHASE_COMPLETED: &str = "completed";

#[derive(Debug, Deserialize)]
pub struct UpgradeMarker {
    pub phase: Option<String>,
    #[serde(default, deserialize_with = "de_major")]
    pub from: Option<String>,
    #[serde(default, deserialize_with = "de_major")]
    pub to: Option<String>,
}

impl UpgradeMarker {
    /// True once the swap is done. The one comparison every consumer of a
    /// marker's terminal state needs, kept in one place next to PHASE_RESEED
    /// so the two constants can't drift out of sync with each other again.
    pub fn is_completed(&self) -> bool {
        self.phase.as_deref() == Some(PHASE_COMPLETED)
    }
}

/// Accept `"16"` or `16` for the marker's major fields. More than one producer
/// writes this file (the upgrade job image and the HA workflow's reseed
/// activity); a numeric major must not fail the whole struct parse, because a
/// failed parse degrades to `phase: None` — which refuses boot, and on a
/// reseed marker that would wedge the replica the marker exists to rebuild.
fn de_major<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StrOrNum {
        S(String),
        N(i64),
    }
    Ok(
        Option::<StrOrNum>::deserialize(deserializer)?.map(|v| match v {
            StrOrNum::S(s) => s,
            StrOrNum::N(n) => n.to_string(),
        }),
    )
}

pub fn marker_path(volume_root: &str) -> String {
    format!("{}/{}", volume_root.trim_end_matches('/'), MARKER_FILENAME)
}

/// The marker, or None when absent. An UNREADABLE marker returns
/// `Some(UpgradeMarker { phase: None, .. })` rather than None: a file we
/// cannot parse still means an upgrade touched this volume, and treating that
/// as "nothing in flight" is the one interpretation that can lose data.
pub fn read_marker(volume_root: &str) -> Option<UpgradeMarker> {
    // Read first and branch on the error kind, not on Path::exists():
    // exists() returns false on ANY stat error (EACCES, EIO), which would
    // read a marker we merely cannot stat as "nothing in flight" — the one
    // interpretation the module contract forbids. Only a definite NotFound
    // means absent; every other failure counts as in-flight.
    match std::fs::read_to_string(marker_path(volume_root)) {
        Ok(body) => Some(serde_json::from_str(&body).unwrap_or(UpgradeMarker {
            phase: None,
            from: None,
            to: None,
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(UpgradeMarker {
            phase: None,
            from: None,
            to: None,
        }),
    }
}

/// True while an upgrade is in flight — anything other than a marker that is
/// absent or explicitly `completed`. A `reseed` marker counts as in flight on
/// purpose: it exists exactly for the window where the cluster's leader is
/// being upgraded, which is when the self-heal watcher must stand down.
pub fn upgrade_in_flight(volume_root: &str) -> bool {
    match read_marker(volume_root) {
        None => false,
        Some(marker) => !marker.is_completed(),
    }
}

/// True when the marker requests a replica reseed (phase == "reseed").
pub fn reseed_requested(volume_root: &str) -> bool {
    matches!(
        read_marker(volume_root),
        Some(m) if m.phase.as_deref() == Some(PHASE_RESEED)
    )
}

/// Remove the marker. Absent is success — the consumers (reseed handling,
/// rollback cleanup) only care that no marker remains.
pub fn remove_marker(volume_root: &str) -> std::io::Result<()> {
    match std::fs::remove_file(marker_path(volume_root)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// How long the marker has been sitting on the volume, from its mtime. None
/// when the marker is absent or the clock/mtime is unusable. Observability
/// only — never a gate: the self-heal watcher reports it so a marker that
/// outlives any plausible upgrade window (a boot-time removal that failed,
/// a workflow that died before its cleanup) is distinguishable from a live
/// one without anyone guessing.
pub fn marker_age_secs(volume_root: &str) -> Option<u64> {
    let mtime = std::fs::metadata(marker_path(volume_root))
        .ok()?
        .modified()
        .ok()?;
    // A future mtime (clock skew) reads as age 0, not an error.
    Some(
        std::time::SystemTime::now()
            .duration_since(mtime)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    )
}

/// The data directory's own major, read from PG_VERSION. None on a fresh
/// volume (first init) or an unreadable file.
pub fn data_dir_major(data_dir: &str) -> Option<String> {
    std::fs::read_to_string(format!("{}/PG_VERSION", data_dir.trim_end_matches('/')))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Debian postgres images install exactly one server under
/// /usr/lib/postgresql/<major>/; that directory name is this image's major.
const PG_LIB_DIR: &str = "/usr/lib/postgresql";

/// The image's own PostgreSQL major, for the version-mismatch boot guard.
///
/// Filesystem first, environment second — deliberately in that order. The
/// installed server tree under /usr/lib/postgresql is baked into the image
/// and nothing at deploy time can change it, while `PG_MAJOR` is an env var
/// a service variable can override: trusted alone, a stray user-set
/// `PG_MAJOR=16` on a 17 image would refuse every boot of a perfectly
/// matched data directory. The env is kept as a fallback for a base image
/// that moves the install tree; a disagreement is logged and the filesystem
/// wins. None (no tree, no env) means the version guard abstains.
pub fn image_major() -> Option<String> {
    let from_fs = image_major_from(PG_LIB_DIR);
    let from_env = std::env::var("PG_MAJOR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match (from_fs, from_env) {
        (Some(fs), Some(env)) => {
            if fs != env {
                tracing::warn!(
                    filesystem = %fs,
                    env = %env,
                    "PG_MAJOR disagrees with the installed server tree; trusting the filesystem (a service variable can override the env, not the image contents)"
                );
            }
            Some(fs)
        }
        (Some(fs), None) => Some(fs),
        (None, Some(env)) => {
            tracing::warn!(
                pg_lib_dir = PG_LIB_DIR,
                "no single installed server tree found; falling back to the PG_MAJOR env"
            );
            Some(env)
        }
        (None, None) => None,
    }
}

/// The single numeric entry under `dir`, or None when there are zero or
/// several (ambiguous — e.g. the dual-major upgrade job image, where this
/// code never runs, but abstaining beats guessing).
fn image_major_from(dir: &str) -> Option<String> {
    let mut majors: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()))
        .collect();
    match majors.len() {
        1 => majors.pop(),
        _ => None,
    }
}

/// Why booting must not proceed, or None when it may. Both cases are
/// fail-stop: this image never tries to repair either one, because both are
/// mid-flight states owned by the upgrade workflow.
///
/// A `reseed` marker allows the boot unconditionally — including across a
/// major mismatch, which is the state a replica is deliberately left in after
/// the leader was upgraded and the replica repinned. The runner's reseed
/// handler (patroni_runner) is what resolves the mismatch: it wipes pgdata
/// under the incomplete-clone safety predicate and deletes the marker so
/// Patroni clones from the leader. In standalone mode nothing consumes the
/// marker, but that boot is still non-destructive: docker-entrypoint never
/// initdb's a non-empty PGDATA, and Postgres itself refuses a cross-major
/// data directory during startup without touching the files.
pub fn boot_refusal_reason(
    volume_root: &str,
    data_dir: &str,
    image_major: Option<&str>,
) -> Option<String> {
    if let Some(marker) = read_marker(volume_root) {
        let phase = marker.phase.as_deref().unwrap_or("unreadable");
        if phase == PHASE_RESEED {
            return None;
        }
        if !marker.is_completed() {
            return Some(format!(
                "A major version upgrade is in progress on this volume (marker phase: {phase}). \
                 The database must not start until the upgrade workflow finishes or rolls back."
            ));
        }
    }

    match (data_dir_major(data_dir), image_major) {
        (Some(on_disk), Some(image)) if on_disk != image => Some(format!(
            "This image runs PostgreSQL {image} but the data directory holds major version \
             {on_disk}. Changing the image tag does not upgrade the data files. Set the image \
             back to major {on_disk}, or run a major version upgrade from the service's settings."
        )),
        _ => None,
    }
}

/// Same filename/location postgres-ssl's wrapper.sh already uses — both
/// consume the identical upgrade-job.sh image against the same volume, and
/// the lock is only meaningful if every runtime that might have Postgres
/// running against this volume agrees on which file it is.
pub const LOCK_FILENAME: &str = ".railway-major-upgrade.lock";

pub fn lock_path(volume_root: &str) -> String {
    format!("{}/{}", volume_root.trim_end_matches('/'), LOCK_FILENAME)
}

/// Take a SHARED, non-blocking flock on the volume-root lock file and hold it
/// for as long as the returned `Flock` stays alive — drop it (or let the
/// process exit) and the lock releases.
///
/// This is the runtime half of the mutual exclusion upgrade-job.sh's own
/// exclusive flock (`take_job_lock`) depends on to catch BOTH directions of a
/// job-vs-runtime race: the job's exclusive attempt already refuses when a
/// shared lock is held (a job dispatched against a live database), but that
/// only works if some runtime is actually holding one. Without this call,
/// nothing on the postgres-ha side ever takes the shared lock, so a job
/// dispatched against a live HA member (a control-plane bug — the
/// orchestrator is supposed to stop the leader before dispatching, this is
/// the backstop for when it doesn't) finds the lock file uncontended and
/// proceeds to `pg_upgrade` a data directory Postgres is actively writing to.
///
/// Returns `Ok(None)` (never fails the boot) when the lock cannot be taken for
/// a reason unrelated to a live conflict — `flock` unavailable, or the file
/// can't be opened (permissions, missing directory). The orchestrator's own
/// exclusion remains the first line of defense; the backstop's own absence
/// must not become a second reason to refuse a healthy boot. Returns `Err`
/// only when the lock is genuinely held exclusively by a job right now — the
/// one case the caller must refuse to boot over.
///
/// Also `Ok(None)` — without even creating the file — when PGDATA is the
/// volume root and holds no PG_VERSION yet: creating the lock file inside an
/// empty PGDATA makes docker-entrypoint conclude the database already exists
/// (it checks `ls -A`), so initdb is skipped and the first boot crash-loops
/// over a hidden dotfile. That layout can't take an in-place upgrade anyway
/// (no sibling slot on the volume for the new data dir). Same guard, same
/// two reasons as postgres-ssl's wrapper.sh.
pub fn take_volume_upgrade_lock(
    volume_root: &str,
    pgdata: &str,
) -> Result<Option<Flock<File>>, String> {
    let pgdata_is_volume_root =
        pgdata.trim_end_matches('/') == volume_root.trim_end_matches('/');
    if pgdata_is_volume_root {
        let pg_version = format!("{}/PG_VERSION", pgdata.trim_end_matches('/'));
        if !std::path::Path::new(&pg_version).exists() {
            tracing::info!(
                pgdata = %pgdata,
                "PGDATA is the volume root and uninitialized — skipping the upgrade lock \
                 so the lock file can't make docker-entrypoint skip initdb"
            );
            return Ok(None);
        }
    }

    let path = lock_path(volume_root);
    let file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                path = %path,
                error = %e,
                "could not open the major-upgrade lock file; continuing without the upgrade lock"
            );
            return Ok(None);
        }
    };
    match Flock::lock(file, FlockArg::LockSharedNonblock) {
        Ok(lock) => Ok(Some(lock)),
        Err((_file, Errno::EWOULDBLOCK)) => Err(format!(
            "{path} is held by another process — a major version upgrade job is currently \
             running against this volume. The database must not start until it finishes; retry \
             the deploy once the upgrade completes."
        )),
        Err((_file, errno)) => {
            tracing::warn!(
                path = %path,
                error = %errno,
                "could not take the major-upgrade lock; continuing without it"
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    fn write_marker(root: &Path, body: &str) {
        fs::write(root.join(MARKER_FILENAME), body).unwrap();
    }

    #[test]
    fn no_marker_means_nothing_in_flight() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        assert!(!upgrade_in_flight(root));
        assert!(read_marker(root).is_none());
    }

    #[test]
    fn completed_marker_is_not_in_flight() {
        let dir = tempdir().unwrap();
        write_marker(dir.path(), r#"{"phase":"completed","from":"16","to":"17"}"#);
        assert!(!upgrade_in_flight(dir.path().to_str().unwrap()));
    }

    #[test]
    fn upgraded_marker_is_in_flight() {
        let dir = tempdir().unwrap();
        write_marker(dir.path(), r#"{"phase":"upgraded","from":"16","to":"17"}"#);
        assert!(upgrade_in_flight(dir.path().to_str().unwrap()));
    }

    // An unreadable marker must read as in-flight: a partially written or
    // corrupt marker still means the volume was touched by an upgrade, and
    // "nothing in flight" is the only interpretation that can wipe data.
    #[test]
    fn unparseable_marker_is_in_flight() {
        let dir = tempdir().unwrap();
        write_marker(dir.path(), "{not json");
        assert!(upgrade_in_flight(dir.path().to_str().unwrap()));
    }

    #[test]
    fn refuses_boot_while_marker_is_not_completed() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        write_marker(dir.path(), r#"{"phase":"upgraded","from":"16","to":"17"}"#);
        let reason = boot_refusal_reason(root, root, Some("17")).unwrap();
        assert!(reason.contains("upgrade is in progress"), "{reason}");
    }

    #[test]
    fn refuses_boot_on_major_mismatch() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        fs::write(dir.path().join("PG_VERSION"), "16").unwrap();
        let reason = boot_refusal_reason(root, root, Some("17")).unwrap();
        assert!(reason.contains("holds major version 16"), "{reason}");
    }

    #[test]
    fn allows_boot_on_matching_major_after_completed_upgrade() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        write_marker(dir.path(), r#"{"phase":"completed","from":"16","to":"17"}"#);
        fs::write(dir.path().join("PG_VERSION"), "17").unwrap();
        assert!(boot_refusal_reason(root, root, Some("17")).is_none());
    }

    // A fresh volume has no PG_VERSION; that is first init, not a mismatch.
    #[test]
    fn allows_boot_on_fresh_volume() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        assert!(boot_refusal_reason(root, root, Some("17")).is_none());
    }

    // Without a known image major there is nothing to compare against, so the
    // version guard abstains rather than guessing.
    #[test]
    fn abstains_when_image_major_is_unknown() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        fs::write(dir.path().join("PG_VERSION"), "16").unwrap();
        assert!(boot_refusal_reason(root, root, None).is_none());
    }

    // ---- reseed phase -------------------------------------------------

    // The whole point of the phase: a replica repinned to the new major still
    // holds old-major data, and the boot that performs the wipe-and-reclone
    // must be allowed through both guards.
    #[test]
    fn reseed_marker_allows_boot_across_major_mismatch() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        write_marker(dir.path(), r#"{"phase":"reseed","from":"16","to":"17"}"#);
        fs::write(dir.path().join("PG_VERSION"), "16").unwrap();
        assert!(boot_refusal_reason(root, root, Some("17")).is_none());
    }

    #[test]
    fn reseed_marker_allows_boot_on_matching_major() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        write_marker(dir.path(), r#"{"phase":"reseed","from":"16","to":"17"}"#);
        fs::write(dir.path().join("PG_VERSION"), "17").unwrap();
        assert!(boot_refusal_reason(root, root, Some("17")).is_none());
    }

    // A reseed marker exists exactly for the window where the leader is being
    // upgraded, so the self-heal watcher must treat it as in flight.
    #[test]
    fn reseed_marker_is_in_flight_for_self_heal_standdown() {
        let dir = tempdir().unwrap();
        write_marker(dir.path(), r#"{"phase":"reseed","from":"16","to":"17"}"#);
        let root = dir.path().to_str().unwrap();
        assert!(upgrade_in_flight(root));
        assert!(reseed_requested(root));
    }

    #[test]
    fn reseed_requested_is_false_for_other_phases_and_absence() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        assert!(!reseed_requested(root));
        write_marker(dir.path(), r#"{"phase":"upgraded","from":"16","to":"17"}"#);
        assert!(!reseed_requested(root));
        write_marker(dir.path(), r#"{"phase":"completed","from":"16","to":"17"}"#);
        assert!(!reseed_requested(root));
    }

    // Numeric majors must not fail the whole parse: a failed parse reads as
    // phase None, which refuses boot — the one degradation that would wedge a
    // replica whose marker exists to rebuild it.
    #[test]
    fn reseed_marker_with_numeric_majors_still_parses() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        write_marker(dir.path(), r#"{"phase":"reseed","from":16,"to":17}"#);
        assert!(reseed_requested(root));
        let marker = read_marker(root).unwrap();
        assert_eq!(marker.from.as_deref(), Some("16"));
        assert_eq!(marker.to.as_deref(), Some("17"));
        fs::write(dir.path().join("PG_VERSION"), "16").unwrap();
        assert!(boot_refusal_reason(root, root, Some("17")).is_none());
    }

    #[test]
    fn remove_marker_removes_and_tolerates_absence() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        write_marker(dir.path(), r#"{"phase":"reseed","from":"16","to":"17"}"#);
        remove_marker(root).unwrap();
        assert!(read_marker(root).is_none());
        // Absent is success: nothing left to remove is the desired state.
        remove_marker(root).unwrap();
    }

    #[test]
    fn marker_age_none_when_absent_and_zeroish_when_fresh() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        assert!(marker_age_secs(root).is_none());
        write_marker(dir.path(), r#"{"phase":"reseed","from":"16","to":"17"}"#);
        // Just written: age must exist and be near zero (not a stat error).
        assert!(marker_age_secs(root).unwrap() < 60);
    }

    // The image's major comes from the single installed server tree; zero or
    // several entries are ambiguous and must abstain, and non-numeric noise
    // (e.g. a stray file) must not count as a major.
    #[test]
    fn image_major_from_single_numeric_dir() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("17")).unwrap();
        fs::write(dir.path().join("README"), "not a major").unwrap();
        assert_eq!(
            image_major_from(dir.path().to_str().unwrap()).as_deref(),
            Some("17")
        );
    }

    #[test]
    fn image_major_from_abstains_on_zero_or_many() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        assert!(image_major_from(root).is_none());
        fs::create_dir(dir.path().join("16")).unwrap();
        fs::create_dir(dir.path().join("17")).unwrap();
        // Two majors (the dual-binary job image's layout): ambiguous, abstain.
        assert!(image_major_from(root).is_none());
        assert!(image_major_from("/nonexistent-path-for-test").is_none());
    }

    // ---- volume upgrade lock -------------------------------------------

    #[test]
    fn lock_taken_when_uncontended() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let lock = take_volume_upgrade_lock(root, &format!("{root}/pgdata")).unwrap();
        assert!(lock.is_some());
    }

    // Two shared holders must coexist — this mirrors two runtime containers
    // (e.g. a crash-looping restart racing the previous instance's teardown),
    // never the job, which always takes an EXCLUSIVE lock.
    #[test]
    fn lock_shared_by_two_concurrent_holders() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let first = take_volume_upgrade_lock(root, &format!("{root}/pgdata")).unwrap();
        assert!(first.is_some());
        let second = take_volume_upgrade_lock(root, &format!("{root}/pgdata")).unwrap();
        assert!(second.is_some());
    }

    // The exact scenario this function exists to catch: an upgrade job
    // (exclusive, non-blocking — mirroring upgrade-job.sh's take_job_lock)
    // already holds the lock, so the runtime's shared attempt must refuse
    // instead of silently proceeding to boot Postgres over a volume mid-job.
    #[test]
    fn lock_refused_while_job_holds_it_exclusively() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        let job_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(lock_path(root))
            .unwrap();
        let _job_lock = Flock::lock(job_file, FlockArg::LockExclusiveNonblock).unwrap();

        let result = take_volume_upgrade_lock(root, &format!("{root}/pgdata"));
        assert!(result.is_err(), "expected the shared lock attempt to refuse");
        assert!(result.unwrap_err().contains("held by another process"));
    }

    // Releasing the lock (drop) must let a subsequent exclusive attempt (the
    // job's own take) succeed — proves this isn't a leak.
    #[test]
    fn lock_release_on_drop_unblocks_a_later_exclusive_take() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        {
            let _lock = take_volume_upgrade_lock(root, &format!("{root}/pgdata")).unwrap();
        } // dropped here

        let job_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(lock_path(root))
            .unwrap();
        assert!(Flock::lock(job_file, FlockArg::LockExclusiveNonblock).is_ok());
    }
    // PGDATA at the volume root with no PG_VERSION = a first boot whose
    // initdb docker-entrypoint decides by `ls -A "$PGDATA"` — creating the
    // lock file there would make an empty database directory look non-empty,
    // skip initdb, and crash-loop the container over a hidden dotfile. The
    // guard must skip the lock AND leave no file behind.
    #[test]
    fn root_layout_first_boot_skips_lock_and_creates_no_file() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();

        let lock = take_volume_upgrade_lock(root, root).unwrap();
        assert!(lock.is_none(), "expected the guard to skip the lock");
        assert!(
            !Path::new(&lock_path(root)).exists(),
            "the guard must not create the lock file inside an empty PGDATA"
        );
    }

    // An INITIALIZED root-layout database (PG_VERSION present) is past the
    // initdb hazard: the lock must be taken normally — this is the legacy
    // postgres-ssl layout the backstop still has to protect.
    #[test]
    fn root_layout_initialized_takes_the_lock() {
        let dir = tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        fs::write(format!("{root}/PG_VERSION"), "16\n").unwrap();

        let lock = take_volume_upgrade_lock(root, root).unwrap();
        assert!(lock.is_some(), "initialized root layout must take the lock");
    }
}

