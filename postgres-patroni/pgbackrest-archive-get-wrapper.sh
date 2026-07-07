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
# never find a segment. Re-reading the marker here mirrors
# pgbackrest-archive-push-wrapper.sh.
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

# Defensive gate, mirroring the push wrapper: if WAL_ARCHIVE_BUCKET is unset
# at call time there is no repo to fetch from — the setting leaked via a
# stale DCS config or an image that predates the disable cleanup. Report
# "not available" so recovery proceeds via streaming.
if [ -z "${WAL_ARCHIVE_BUCKET:-}" ]; then
  exit 1
fi

if [ -f "$PGDATA/.pgbackrest_repo_path" ]; then
  PGBACKREST_REPO1_PATH=$(cat "$PGDATA/.pgbackrest_repo_path")
  export PGBACKREST_REPO1_PATH
fi

exec pgbackrest --stanza=main archive-get "$WAL_FILE" "$WAL_DEST"
