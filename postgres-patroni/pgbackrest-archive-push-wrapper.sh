#!/bin/bash
# pgbackrest-archive-push-wrapper.sh — invoked by Postgres as archive_command.
#
# Wraps `pgbackrest archive-push` to keep Postgres alive when archiving can't
# keep up with WAL generation. Three drop paths, all touch the gap_marker so
# pgbackrest-backup-watcher.sh takes a fresh diff once archiving recovers
# (sealing forward-restore coverage; the dropped segments themselves are
# unrestorable, by design):
#
#   1. PRE-ARCHIVE size check — before calling pgbackrest, drop the incoming
#      segment if pg_wal is already past WAL_DROP_THRESHOLD_MB. Bounds the
#      backlog when archiving succeeds individually but the async-push rate
#      lags WAL generation (the failure mode that crashed junior.mtdn.dev:
#      ~28 GiB pg_wal accumulated while pg_stat_archiver.failed_count stayed
#      at 0 and every archive_command returned 0; the reactive check below
#      never fired until disk filled and Postgres was already crashing).
#
#   2. PRE-ARCHIVE free-disk check — drop if the data volume has less free
#      space than WAL_DROP_THRESHOLD_MB, regardless of what's filling it.
#      Mirrors the pg_wal cap so both protect Postgres uptime symmetrically:
#      the cap is both the max pg_wal we retain AND the minimum free disk we
#      keep available for Postgres to operate.
#
#   3. POST-FAILURE check — if pgbackrest exited non-zero AND pg_wal is past
#      threshold, drop. Catches hard failures (bad creds, deleted bucket,
#      expired keys) where the foreground returns non-zero immediately and
#      retrying without operator intervention has zero chance of success.
#      Below threshold, pgbackrest's non-zero exit surfaces to Postgres so
#      pg_stat_archiver.failed_count climbs and the dashboard surfaces
#      "PITR broken — fix archiving config" before the threshold trips and
#      the failure signal disappears.
#
# Special case: NoSuchBucket / InvalidAccessKeyId always drop immediately
# regardless of pg_wal size — no recovery is possible without operator
# action, so holding any pg_wal hostage just wastes disk.
#
# Threshold sizing (WAL_DROP_THRESHOLD_MB; defaults set by patroni_runner's
# compute_volume_thresholds): ~10% of volume, capped at 5 GiB, floor 64 MiB.
# Matches pgBackRest's archive-push-queue-max=5 GiB spool cap so pg_wal and
# spool consume symmetric on-disk budgets under sustained outage. Operator
# override via the WAL_DROP_THRESHOLD_MB env var (or the legacy
# PGBACKREST_DROP_THRESHOLD_MB).

set -u

WAL_FILE="${1:-}"
if [ -z "$WAL_FILE" ]; then
  echo "pgbackrest-wrapper: missing WAL file argument" >&2
  exit 1
fi

PGDATA="${PGDATA:-/var/lib/postgresql/data/pgdata}"
PGWAL_THRESHOLD_MB="${WAL_DROP_THRESHOLD_MB:-${PGBACKREST_DROP_THRESHOLD_MB:-5120}}"
PGWAL_THRESHOLD_BYTES=$(( PGWAL_THRESHOLD_MB * 1024 * 1024 ))

mark_gap_and_log() {
  # $1 = reason string; logs + touches the gap marker so the watcher takes
  # a fresh diff once archiving recovers, anchoring forward-restore coverage.
  echo "pgbackrest-wrapper: $1; dropping ${WAL_FILE} to keep Postgres up" >&2
  touch "$PGDATA/.pgbackrest_gap_pending" 2>/dev/null || true
}

# Pre-archive safety. Drop without invoking pgbackrest when EITHER pg_wal is
# already past threshold OR free disk is below threshold. The post-failure
# check below only fires on pgbackrest non-zero exit, which leaves the
# "archive succeeds individually but throughput < WAL generation" regime
# undefended — every call returned 0, the reactive du never ran, pg_wal grew
# to 28 GiB before Postgres ran out of disk.
PGWAL_BYTES=$(du -sb "$PGDATA/pg_wal" 2>/dev/null | awk '{print $1}')
FREE_BYTES=$(df -P -k "$PGDATA" 2>/dev/null | awk 'NR==2 {print $4 * 1024}')

if [ -n "${PGWAL_BYTES:-}" ] && [ "$PGWAL_BYTES" -ge "$PGWAL_THRESHOLD_BYTES" ]; then
  PGWAL_MB=$(( PGWAL_BYTES / 1024 / 1024 ))
  mark_gap_and_log "pg_wal at ${PGWAL_MB} MiB before archive-push (threshold ${PGWAL_THRESHOLD_MB} MiB)"
  exit 0
fi
if [ -n "${FREE_BYTES:-}" ] && [ "$FREE_BYTES" -lt "$PGWAL_THRESHOLD_BYTES" ]; then
  FREE_MB=$(( FREE_BYTES / 1024 / 1024 ))
  mark_gap_and_log "${FREE_MB} MiB free on ${PGDATA} (threshold ${PGWAL_THRESHOLD_MB} MiB)"
  exit 0
fi

# Per-cluster repo-path: read the marker written by patroni-runner's
# bootstrap subshell. Without this, every archive-push would go to the
# ${WAL_ARCHIVE_PATH} root and a wipe-and-reuse-bucket scenario would
# collide on stanza identity. With it, archive-push targets
# ${WAL_ARCHIVE_PATH}/cluster-<sysid>.
if [ -f "$PGDATA/.pgbackrest_repo_path" ]; then
  PGBACKREST_REPO1_PATH=$(cat "$PGDATA/.pgbackrest_repo_path")
  export PGBACKREST_REPO1_PATH
fi

# pgBackRest 2.58 rejects --repo on archive-push (it pushes to whatever
# repos are configured). The default /etc/pgbackrest/pgbackrest.conf has
# only repo1 (the service's own bucket); the recovery-source conf is
# isolated under a separate file referenced via --config exclusively for
# archive-get during recovery. So archive-push naturally only touches
# the service's own bucket.
pgb_out=$(pgbackrest --stanza=main archive-push "$WAL_FILE" 2>&1)
PGB_RC=$?
[ -n "$pgb_out" ] && printf '%s\n' "$pgb_out" >&2
if [ "$PGB_RC" -eq 0 ]; then
  exit 0
fi

# Bucket deleted: when the bucket no longer exists Tigris returns NoSuchBucket
# on read paths, but validates credentials before checking bucket existence on
# write paths (archive-push is a PUT). Railway revokes the bucket credentials
# when the bucket is deleted, so in practice pgBackRest sees InvalidAccessKeyId
# on the PUT. Both errors mean no recovery without operator action — drop
# immediately rather than accumulating WAL up to the threshold.
if printf '%s\n' "$pgb_out" | grep -qE 'NoSuchBucket|InvalidAccessKeyId'; then
  mark_gap_and_log "bucket gone or credentials revoked"
  exit 0
fi

# Post-failure check: re-read pg_wal size in case it grew during the
# pgbackrest invocation. Drop if past threshold; otherwise surface
# pgbackrest's non-zero exit so pg_stat_archiver.failed_count climbs and
# operators see the underlying issue while there's still time to fix it.
PGWAL_BYTES=$(du -sb "$PGDATA/pg_wal" 2>/dev/null | awk '{print $1}')
if [ -z "${PGWAL_BYTES:-}" ]; then
  exit "$PGB_RC"
fi

if [ "$PGWAL_BYTES" -ge "$PGWAL_THRESHOLD_BYTES" ]; then
  PGWAL_MB=$(( PGWAL_BYTES / 1024 / 1024 ))
  mark_gap_and_log "pg_wal at ${PGWAL_MB} MiB (threshold ${PGWAL_THRESHOLD_MB} MiB) and archive-push failing"
  exit 0
fi

exit "$PGB_RC"
