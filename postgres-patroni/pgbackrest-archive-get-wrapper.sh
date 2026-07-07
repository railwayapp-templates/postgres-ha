#!/bin/bash
# pgbackrest-archive-get-wrapper.sh — invoked by Postgres as restore_command
# on Patroni-managed standbys (installed via postgresql.parameters in
# patroni.yml when WAL_ARCHIVE_BUCKET is set).
#
# Wraps `pgbackrest archive-get` for one reason: repo1-path must be resolved
# at CALL time, not at process-spawn time. The per-cluster repo path
# (<WAL_ARCHIVE_PATH>/cluster-<sysid>) is derived by patroni-runner only
# once pg_control exists and is persisted in $PGDATA/.pgbackrest_repo_path;
# the PGBACKREST_REPO1_PATH env var Postgres inherited from Patroni still
# holds the pre-derivation base path. pgBackRest gives environment variables
# precedence over pgbackrest.conf, so calling pgbackrest directly from
# restore_command would silently point archive-get at the bucket root and
# never find a segment.
#
# Resolution is marker-first, DCS-on-miss — the inverse of the replica
# restore wrapper's DCS-first order, because the cost profile is inverted:
# restore runs once per re-seed (one curl is nothing) and its headline
# scenario has no marker at all, while restore_command runs once per WAL
# segment with a marker that is almost always right. The marker can still
# go stale: the backup watcher's DCS converge runs on the LEADER only, so a
# standby that lived through a leader-side WAL-regression path migration
# keeps the old path — and that standby is exactly the one that needs the
# archive fallback. So on an archive-get miss, ask Patroni DCS
# (`pgbackrest_repo1_path`, published by the leader's backup watcher) for
# the authoritative path; if it differs, retry there and rewrite the marker
# on success so subsequent calls resolve correctly without the extra hop.
# A genuine miss (segment not yet archived — the normal end-of-catch-up
# signal to switch to streaming) finds DCS agreeing with the marker and
# adds only one localhost curl, no second S3 round-trip.
#
# Exit codes follow restore_command semantics: non-zero means "segment not
# available here" and standby recovery falls back to streaming; only death
# by signal is treated as FATAL by Postgres. No drop/threshold logic is
# needed on the read path.

set -u

WAL_FILE="${1:-}"
WAL_DEST="${2:-}"
if [ -z "$WAL_FILE" ] || [ -z "$WAL_DEST" ]; then
  echo "pgbackrest-archive-get-wrapper: usage: $0 <wal_file> <dest_path>" >&2
  exit 1
fi

PGDATA="${PGDATA:-/var/lib/postgresql/data/pgdata}"
MARKER="$PGDATA/.pgbackrest_repo_path"

# Defensive gate, mirroring the push wrapper: if WAL_ARCHIVE_BUCKET is unset
# at call time there is no repo to fetch from — the setting leaked via a
# stale DCS config or an image that predates the disable cleanup. Report
# "not available" so recovery proceeds via streaming.
if [ -z "${WAL_ARCHIVE_BUCKET:-}" ]; then
  exit 1
fi

# Marker wins only when it actually holds a path: an empty read (truncated
# write, fresh file) falls back to the inherited env so USED_PATH always
# names the path the first attempt really runs with.
USED_PATH="${PGBACKREST_REPO1_PATH:-}"
if [ -f "$MARKER" ]; then
  MARKER_PATH=$(tr -d '\n\r' <"$MARKER")
  [ -n "$MARKER_PATH" ] && USED_PATH="$MARKER_PATH"
fi
if [ -n "$USED_PATH" ]; then
  export PGBACKREST_REPO1_PATH="$USED_PATH"
fi

pgbackrest --stanza=main archive-get "$WAL_FILE" "$WAL_DEST"
rc=$?
[ "$rc" -eq 0 ] && exit 0

DCS_PATH=$(curl -sf --max-time 2 http://localhost:8008/config 2>/dev/null \
  | python3 -c 'import json,sys; v = json.load(sys.stdin).get("pgbackrest_repo1_path") or ""; print(v if isinstance(v, str) else "")' 2>/dev/null)
if [ -n "$DCS_PATH" ] && [ "$DCS_PATH" != "$USED_PATH" ]; then
  if PGBACKREST_REPO1_PATH="$DCS_PATH" pgbackrest --stanza=main archive-get "$WAL_FILE" "$WAL_DEST"; then
    echo "pgbackrest-archive-get-wrapper: repo path '${USED_PATH:-<unset>}' is stale; ${WAL_FILE} found at DCS path '${DCS_PATH}' — rewriting marker" >&2
    # tmp + rename so concurrent readers (archive-push cats this file on
    # every WAL segment) never see a truncated marker; mirrors the backup
    # watcher's apply_active_path.
    MARKER_TMP="$MARKER.tmp.$$"
    { printf '%s\n' "$DCS_PATH" >"$MARKER_TMP" && chmod 640 "$MARKER_TMP" && mv "$MARKER_TMP" "$MARKER"; } 2>/dev/null \
      || rm -f "$MARKER_TMP" 2>/dev/null || true
    exit 0
  fi
fi

exit "$rc"
