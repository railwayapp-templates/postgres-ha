#!/bin/bash
# pgbackrest-archive-push-wrapper.sh — invoked by Postgres as archive_command.
#
# Wraps `pgbackrest archive-push` so that any kind of archive failure (hard
# repo error, stuck async worker, anything else) cannot fill disk and halt
# Postgres. When pgbackrest fails AND pg_wal/ + pgbackrest-spool/ combined
# have grown past a threshold (WAL_DROP_THRESHOLD_MB; sized by
# patroni_runner's compute_volume_thresholds to min(5 GiB, ~50% of volume)
# with operator override via this env var or the legacy
# PGBACKREST_DROP_THRESHOLD_MB), the wrapper returns success to Postgres
# anyway. Postgres recycles the WAL segment as if archiving were disabled.
# The PITR window gets a coverage gap from this segment forward; the
# dashboard reads pg_stat_archiver to surface "PITR broken — fix archiving
# config" so the underlying issue (bad creds, deleted bucket, expired
# keys, …) gets fixed.
#
# Special case: if the bucket actively does not exist (S3 NoSuchBucket error)
# or its credentials were revoked (InvalidAccessKeyId), there is no recovery
# without operator action — retrying is pointless and letting WAL accumulate
# up to the threshold wastes disk. In that case the wrapper drops immediately
# (returns 0) regardless of pg_wal size.
#
# WAL_DROP_THRESHOLD_MB matches pgBackRest's own archive-push-queue-max
# (also ≤5 GiB, same volume-proportional sizing), and the two are checked as
# ONE combined budget (pg_wal + spool, not each independently), rather than
# the pre-2026-07-01 design of two INDEPENDENT thresholds. Before that date
# this threshold was ~10x smaller (≤500 MiB) on the theory that anything
# reaching this wrapper's non-zero-exit path was a hard, unrecoverable
# failure (bad creds, deleted bucket) not worth holding disk for — but
# that's wrong for transient S3-side errors (500s, timeouts, connection
# resets), which is exactly what pgBackRest's own archive-push-queue-max
# spool is designed to absorb generously, "most segments eventually get
# pushed" once the outage clears. Those errors also return non-zero from
# pgbackrest's foreground and used to trip this threshold 10x sooner than
# the spool's own budget, silently truncating the PITR window during an
# outage the spool could otherwise have ridden out. Simply raising this
# threshold to match the spool's without combining them would have let the
# two accumulate independently — up to ~2x the intended on-disk budget when
# both pg_wal (foreground copy-to-spool failing) and the spool (foreground
# succeeding, background upload stalled) fill up at once. Summing them
# means the combined WAL-related footprint during any outage — transient
# or hard — never exceeds the single configured budget. Only the two
# explicit no-recovery-possible errors above bypass it and drop
# immediately.
#
# Below the threshold the wrapper surfaces pgbackrest's failure to Postgres
# normally, so transient errors retry on the next archive_timeout instead
# of being silently dropped.

set -u

WAL_FILE="${1:-}"
if [ -z "$WAL_FILE" ]; then
  echo "pgbackrest-wrapper: missing WAL file argument" >&2
  exit 1
fi

PGDATA="${PGDATA:-/var/lib/postgresql/data/pgdata}"

# Defensive gate: if WAL_ARCHIVE_BUCKET is unset or empty at the time
# archive_command fires, archiving is not configured for this service.
# The normal path is patroni-runner's disable cleanup (and Patroni's DCS
# reconcile) removing archive_command before postgres starts — but the
# setting can still leak via an older image without that cleanup, a
# Patroni reconcile that hasn't run since the customer blanked the
# variable, or an ALTER SYSTEM SET archive_command parked in
# postgresql.auto.conf. Surfacing pgbackrest's FileMissingError (exit
# 103) to Postgres in that state produces tens of thousands of
# "archive_command failed" lines a day for a service whose PITR is
# intentionally off. Return 0 so pg_wal recycles; the log line below is
# the only signal admins need to clear the stale config (redeploy, or
# unset archive_command in DCS / postgresql.auto.conf).
if [ -z "${WAL_ARCHIVE_BUCKET:-}" ]; then
  echo "pgbackrest-wrapper: WAL_ARCHIVE_BUCKET is unset; archive_command should not be installed. Dropping ${WAL_FILE} to keep Postgres up — redeploy the cluster so the disable cleanup can drop archive_command, or update the source image if a redeploy doesn't fix it." >&2
  exit 0
fi

PGWAL_THRESHOLD_MB="${WAL_DROP_THRESHOLD_MB:-${PGBACKREST_DROP_THRESHOLD_MB:-5120}}"
PGWAL_THRESHOLD_BYTES=$(( PGWAL_THRESHOLD_MB * 1024 * 1024 ))

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

# In async mode pgBackRest records the durable failure reason to a spool status
# file. Print it so the actual failure appears in deployment logs instead of
# being invisible.
WAL_BASENAME=$(basename "$WAL_FILE")
SPOOL_ERR="${PGDATA}/pgbackrest-spool/archive/main/out/${WAL_BASENAME}.error"
if [ -f "$SPOOL_ERR" ]; then
  printf 'pgbackrest-wrapper: async spool error for %s (rc=%s):\n' "$WAL_BASENAME" "$PGB_RC" >&2
  cat "$SPOOL_ERR" >&2
fi

# Bucket deleted: when the bucket no longer exists Tigris returns NoSuchBucket
# on read paths, but validates credentials before checking bucket existence on
# write paths (archive-push is a PUT). Railway revokes the bucket credentials
# when the bucket is deleted, so in practice pgBackRest sees InvalidAccessKeyId
# on the PUT. Both errors mean no recovery without operator action — drop
# immediately rather than accumulating WAL up to the threshold.
if printf '%s\n' "$pgb_out" | grep -qE 'NoSuchBucket|InvalidAccessKeyId'; then
  echo "pgbackrest-wrapper: bucket gone or credentials revoked; dropping ${WAL_FILE} immediately" >&2
  touch "$PGDATA/.pgbackrest_gap_pending" 2>/dev/null || true
  exit 0
fi

PGWAL_BYTES=$(du -sb "$PGDATA/pg_wal" 2>/dev/null | awk '{print $1}')
if [ -z "${PGWAL_BYTES:-}" ]; then
  exit "$PGB_RC"
fi

# Shared budget: pg_wal/ and pgbackrest-spool/ are two separate directories
# both holding WAL bytes that haven't reached S3, so they must count against
# the SAME threshold, not each get their own. Without this, a stall could
# hold WAL_DROP_THRESHOLD_MB in pg_wal (foreground copy-to-spool failing)
# AND archive-push-queue-max in the spool (foreground succeeding, background
# upload stalled) at the same time — up to ~2x the intended budget on disk.
SPOOL_BYTES=$(du -sb "$PGDATA/pgbackrest-spool" 2>/dev/null | awk '{print $1}')
: "${SPOOL_BYTES:=0}"
TOTAL_BYTES=$(( PGWAL_BYTES + SPOOL_BYTES ))

if [ "$TOTAL_BYTES" -ge "$PGWAL_THRESHOLD_BYTES" ]; then
  PGWAL_MB=$(( PGWAL_BYTES / 1024 / 1024 ))
  SPOOL_MB=$(( SPOOL_BYTES / 1024 / 1024 ))
  echo "pgbackrest-wrapper: pg_wal+spool at ${PGWAL_MB}+${SPOOL_MB} MiB (threshold ${PGWAL_THRESHOLD_MB} MiB combined) and archive-push failing; dropping ${WAL_FILE} to keep Postgres up" >&2
  # Mark the gap so the backup watcher takes a fresh full once archiving
  # recovers. Without this, gap detection collapses to single-signal
  # (failed_count growth from foreground archive_command failures), but the
  # wrapper-drop path returns 0 to Postgres so failed_count never grows.
  touch "$PGDATA/.pgbackrest_gap_pending" 2>/dev/null || true
  exit 0
fi

exit "$PGB_RC"
