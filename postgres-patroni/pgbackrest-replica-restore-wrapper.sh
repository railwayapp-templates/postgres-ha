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
# warns and does a full restore — harmless). It genuinely resumes only
# when a failed pgbackrest restore is retried before the basebackup
# fallback runs (whose pre-wipe destroys the partial).
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
# Non-zero exit → Patroni tries the next create_replica_method. A fresh
# stanza with no backups fails here before writing anything; Patroni wipes
# the data dir before running basebackup (no keep_data on that method).

set -u

PGDATA="${PGDATA:-/var/lib/postgresql/data/pgdata}"
MARKER="$PGDATA/.pgbackrest_repo_path"

if [ -z "${WAL_ARCHIVE_BUCKET:-}" ]; then
  echo "pgbackrest-replica-restore: WAL_ARCHIVE_BUCKET unset; deferring to next create_replica_method" >&2
  exit 1
fi

REPO_PATH=$(curl -sf --max-time 5 http://localhost:8008/config 2>/dev/null \
  | python3 -c 'import json,sys; v = json.load(sys.stdin).get("pgbackrest_repo1_path") or ""; print(v if isinstance(v, str) else "")' 2>/dev/null)
if [ -z "$REPO_PATH" ] && [ -f "$MARKER" ]; then
  REPO_PATH=$(tr -d '\n\r' <"$MARKER")
fi
if [ -n "$REPO_PATH" ]; then
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

exit "$rc"
