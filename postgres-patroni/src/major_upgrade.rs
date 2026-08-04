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

use serde::Deserialize;
use std::path::Path;

pub const MARKER_FILENAME: &str = ".railway-major-upgrade.json";

#[derive(Debug, Deserialize)]
pub struct UpgradeMarker {
    pub phase: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

pub fn marker_path(volume_root: &str) -> String {
    format!("{}/{}", volume_root.trim_end_matches('/'), MARKER_FILENAME)
}

/// The marker, or None when absent. An UNREADABLE marker returns
/// `Some(UpgradeMarker { phase: None, .. })` rather than None: a file we
/// cannot parse still means an upgrade touched this volume, and treating that
/// as "nothing in flight" is the one interpretation that can lose data.
pub fn read_marker(volume_root: &str) -> Option<UpgradeMarker> {
    let path = marker_path(volume_root);
    if !Path::new(&path).exists() {
        return None;
    }
    match std::fs::read_to_string(&path) {
        Ok(body) => Some(serde_json::from_str(&body).unwrap_or(UpgradeMarker {
            phase: None,
            from: None,
            to: None,
        })),
        Err(_) => Some(UpgradeMarker {
            phase: None,
            from: None,
            to: None,
        }),
    }
}

/// True while an upgrade is in flight — anything other than a marker that is
/// absent or explicitly `completed`.
pub fn upgrade_in_flight(volume_root: &str) -> bool {
    match read_marker(volume_root) {
        None => false,
        Some(marker) => marker.phase.as_deref() != Some("completed"),
    }
}

/// The data directory's own major, read from PG_VERSION. None on a fresh
/// volume (first init) or an unreadable file.
pub fn data_dir_major(data_dir: &str) -> Option<String> {
    std::fs::read_to_string(format!("{}/PG_VERSION", data_dir.trim_end_matches('/')))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Why booting must not proceed, or None when it may. Both cases are
/// fail-stop: this image never tries to repair either one, because both are
/// mid-flight states owned by the upgrade workflow.
pub fn boot_refusal_reason(
    volume_root: &str,
    data_dir: &str,
    image_major: Option<&str>,
) -> Option<String> {
    if let Some(marker) = read_marker(volume_root) {
        let phase = marker.phase.as_deref().unwrap_or("unreadable");
        if phase != "completed" {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
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
}
