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
# Exit codes follow restore_command semantics: exit 1 means "segment not
# available here" and standby recovery falls back to streaming, while any
# exit >125 (or death by signal) is treated as FATAL by Postgres and ends
# the startup process.
#
# That FATAL semantic carries the connectivity circuit-breaker: pgBackRest
# exits 0 on a hit and 1 on a genuine miss (repo reachable, file absent) —
# both PROVE the archive endpoint is reachable and clear the breaker. Any
# other exit (25–125: HostConnect/RepoInvalid/TLS/timeout classes) means we
# could not talk to the repo at all; after
# WAL_ARCHIVE_GET_CONNECTIVITY_TRIP consecutive such invocations the
# wrapper exits 126 so Postgres FATALs the startup process instead of
# retrying the dead endpoint forever. Why that is the RIGHT failure mode: a
# standby that needs archived WAL it cannot fetch keeps its postmaster
# alive in "starting" for as long as restore_command keeps returning 1,
# and Patroni parks any force-reinitialize behind that never-finishing
# start — the WAL-too-old self-heal fires but cannot act. Tripping to
# FATAL restores crash-loop dynamics: postgres exits, Patroni restarts it
# (probing the archive afresh each cycle, so a recovered endpoint heals on
# the next boot), and the self-heal reinitialize can land while postgres
# is down. The counter lives in PGDATA (wiped with a re-seed) and resets
# on every trip so each crash-loop cycle re-probes the endpoint a full
# threshold's worth before tripping again.

set -u

WAL_FILE="${1:-}"
WAL_DEST="${2:-}"
if [ -z "$WAL_FILE" ] || [ -z "$WAL_DEST" ]; then
  echo "pgbackrest-archive-get-wrapper: usage: $0 <wal_file> <dest_path>" >&2
  exit 1
fi

PGDATA="${PGDATA:-/var/lib/postgresql/data/pgdata}"
MARKER="$PGDATA/.pgbackrest_repo_path"
BREAKER="$PGDATA/.pgbackrest_archive_get_conn_failures"
TRIP_THRESHOLD="${WAL_ARCHIVE_GET_CONNECTIVITY_TRIP:-30}"

# Success (0) and genuine miss (1) both prove the repo answered: clear the
# breaker and pass the code through. Anything else is a connectivity-class
# failure: count it, trip to 126 at the threshold. Single writer (recovery
# invokes restore_command serially), so a plain overwrite is safe.
finish() {
  rc="$1"
  if [ "$rc" -eq 0 ] || [ "$rc" -eq 1 ]; then
    rm -f "$BREAKER" 2>/dev/null || true
    exit "$rc"
  fi
  fails=$(tr -dc '0-9' <"$BREAKER" 2>/dev/null)
  fails=$((${fails:-0} + 1))
  if [ "$fails" -ge "$TRIP_THRESHOLD" ]; then
    rm -f "$BREAKER" 2>/dev/null || true
    echo "pgbackrest-archive-get-wrapper: archive endpoint unreachable for ${fails} consecutive invocations (last rc=${rc}) — connectivity breaker tripped, exiting 126 so recovery crash-loops instead of waiting on a dead archive forever" >&2
    exit 126
  fi
  printf '%s\n' "$fails" >"$BREAKER" 2>/dev/null || true
  exit "$rc"
}

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
[ "$rc" -eq 0 ] && finish 0

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
    finish 0
  fi
fi

finish "$rc"
