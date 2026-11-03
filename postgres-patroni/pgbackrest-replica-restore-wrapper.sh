#!/bin/bash
# pgbackrest-replica-restore-wrapper.sh — Patroni create_replica_method.
# Re-seeds a replica from the S3 archive (`pgbackrest restore`) instead of
# streaming the whole data directory off the live leader with pg_basebackup.
#
# The wrapper exists because the per-cluster repo1-path
# (<WAL_ARCHIVE_PATH>/cluster-<sysid>) cannot come from the environment
# Patroni inherited at spawn: on the exact scenario this method targets — a
# wiped replica volume — there is no pg_control and no
# $PGDATA/.pgbackrest_repo_path marker to derive it from, and the inherited
# PGBACKREST_REPO1_PATH still holds the pre-derivation base path (which
# pgBackRest prefers over pgbackrest.conf). Resolution order at call time:
#
#   1. Patroni DCS `/config` key `pgbackrest_repo1_path` — published by the
#      leader's backup watcher every iteration, so it exists on any cluster
#      that has been archiving; authoritative even after a WAL-regression
#      path migration. Patroni's REST API is up before create_replica runs.
#   2. The volume marker — covers delta re-seeds where the volume survived
#      but DCS is momentarily unreachable.
#   3. The inherited env default (bucket root) — best effort; if wrong the
#      restore fails and Patroni falls back to the next method (basebackup).
#
# `--delta` reuses valid files already on the volume (checksum-verified).
# Don't expect it to engage often: Patroni's reinitialize and its
# invalid-system-ID path both wipe pgdata BEFORE running replica methods,
# so delta usually lands on an empty dir and auto-disables (pgBackRest
# warns and does a full restore — harmless). A retried partial restore
# resumes only when the partial already contains PG_VERSION: without it
# pgBackRest disables delta and then refuses the non-empty dir, so the
# attempt fails and the basebackup fallback (whose pre-wipe destroys the
# partial) takes over.
# `--type=none` keeps pgBackRest from writing restore_command into
# postgresql.auto.conf or creating recovery.signal — Patroni owns the
# standby recovery config (and would sanitize those away regardless).
#
# On success the marker is rewritten with the path actually used — including
# the env-default case, where success proves the inherited path was right:
# the restored backup carries the leader's marker from backup time, which a
# later path migration may have staled; restore_command (the archive-get
# wrapper) reads the marker on every call.
#
# On success the credential pin ($PGDATA/.railway_credentials) and, when one is
# staged, the rotation journal ($PGDATA/.railway_rotation) are materialized
# from the volume's copies (<volume root>/.railway_credentials.volume and
# .railway_rotation.volume, kept outside PGDATA by the runner and the rotation
# endpoint). Backups exclude both files, and a `--delta` restore removes
# excluded files it finds in the target directory, so a restore alone leaves
# the replica without a pin; an older backup taken before the exclusion
# restores the leader's pin as of backup time, stale once a rotation has
# happened since. The member's own copy is at least as fresh as its last
# rotation, so it wins either way. This reproduces what pg_basebackup brings
# from the leader. Without copies (the legacy layout, where PGDATA is the
# volume root and keeps none) nothing is written: the runner's boot
# reconciliation and the rotation endpoint establish the pin as they do today.
#
# Non-zero exit → Patroni tries the next create_replica_method. A fresh
# stanza with no backups fails here before writing anything; Patroni wipes
# the data dir before running basebackup (no keep_data on that method).

set -u

repo_path_is_usable() {
  case "$1" in /*) ;; *) return 1 ;; esac
  case "$1" in *$'\n'*|*$'\r'*) return 1 ;; esac
}

PGDATA="${PGDATA:-/var/lib/postgresql/data/pgdata}"
MARKER="$PGDATA/.pgbackrest_repo_path"
# Same derivation as the runner's volume_root(): the platform's mount path,
# else the image default.
VOLUME_ROOT="${RAILWAY_VOLUME_MOUNT_PATH:-/var/lib/postgresql/data}"

# Copy <volume root>/<name>.volume over $PGDATA/<name>, mode 0600, via tmp +
# rename so a concurrent reader never sees a partial pin. No copy → nothing
# written (and a pin the restore brought is left in place). A failure is
# reported but never fails the restore: the pin is a fallback the boot
# reconciliation and the rotation endpoint can re-establish.
materialize_volume_copy() {
  local name="$1" copy="$VOLUME_ROOT/$1.volume" target="$PGDATA/$1" tmp
  [ -f "$copy" ] || return 0
  tmp="$target.tmp.$$"
  if { cp "$copy" "$tmp" && chmod 600 "$tmp" && mv "$tmp" "$target"; } 2>/dev/null; then
    echo "pgbackrest-replica-restore: $name materialized from the volume's copy" >&2
  else
    rm -f "$tmp" 2>/dev/null
    echo "pgbackrest-replica-restore: could not materialize $name from the volume's copy" >&2
  fi
}

if [ -z "${WAL_ARCHIVE_BUCKET:-}" ]; then
  echo "pgbackrest-replica-restore: WAL_ARCHIVE_BUCKET unset; deferring to next create_replica_method" >&2
  exit 1
fi

REPO_PATH=$(curl -sf --max-time 5 http://localhost:8008/config 2>/dev/null \
  | python3 -c 'import json,sys; v = json.load(sys.stdin).get("pgbackrest_repo1_path") or ""; print(v if isinstance(v, str) else "")' 2>/dev/null)
if ! repo_path_is_usable "$REPO_PATH" && [ -f "$MARKER" ]; then
  REPO_PATH=$(cat "$MARKER")
fi
if repo_path_is_usable "$REPO_PATH"; then
  export PGBACKREST_REPO1_PATH="$REPO_PATH"
fi

echo "pgbackrest-replica-restore: restoring from repo1-path '${PGBACKREST_REPO1_PATH:-<unset>}'" >&2

# pgBackRest does not create a missing data directory on restore; a wiped
# volume has no pgdata at all. An empty dir is safe to pre-create — Patroni's
# "data dir is not empty, but system ID is invalid" gate only fires on
# non-empty dirs.
mkdir -p "$PGDATA" && chmod 700 "$PGDATA"

pgbackrest --stanza=main --delta --type=none restore
rc=$?

if [ "$rc" -eq 0 ] && [ -n "${PGBACKREST_REPO1_PATH:-}" ]; then
  # tmp + rename so concurrent readers (archive-push/archive-get read this
  # file per call) never see a truncated marker; mirrors the backup
  # watcher's apply_active_path.
  MARKER_TMP="$MARKER.tmp.$$"
  { printf '%s\n' "$PGBACKREST_REPO1_PATH" >"$MARKER_TMP" && chmod 640 "$MARKER_TMP" && mv "$MARKER_TMP" "$MARKER"; } 2>/dev/null \
    || rm -f "$MARKER_TMP" 2>/dev/null || true
fi

# On the legacy layout PGDATA is the volume root: no copies are kept there,
# and "<PGDATA>/.railway_credentials.volume" would be a file inside the data
# directory, never the volume's copy.
if [ "$rc" -eq 0 ] && [ "${PGDATA%/}" != "${VOLUME_ROOT%/}" ]; then
  materialize_volume_copy .railway_credentials
  materialize_volume_copy .railway_rotation
fi

exit "$rc"
