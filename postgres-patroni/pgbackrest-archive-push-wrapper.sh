#!/bin/bash
# pgbackrest-archive-push-wrapper.sh — invoked by Postgres as archive_command.
#
# Wraps `pgbackrest archive-push` so that any kind of archive failure (hard
# repo error, stuck async worker, anything else) cannot fill pg_wal/ and halt
# Postgres. When pgbackrest fails AND pg_wal/ has grown past a threshold
# (WAL_DROP_THRESHOLD_MB; sized by patroni_runner's compute_volume_thresholds
# to min(500 MiB, ~10% of volume) with operator override via this env var
# or the legacy PGBACKREST_DROP_THRESHOLD_MB), the wrapper returns success
# to Postgres anyway. Postgres recycles the WAL segment as if archiving were
# disabled. The PITR window gets a coverage gap from this segment forward;
# the dashboard reads pg_stat_archiver to surface "PITR broken — fix
# archiving config" so the underlying issue (bad creds, deleted bucket,
# expired keys, …) gets fixed.
#
# Why ≤500 MiB here, vs pgBackRest's archive-push-queue-max ≤5GiB:
# the two thresholds gate orthogonal failure regimes. archive-push-queue-max
# governs the SPOOL — graceful absorption of transient S3 stalls, where the
# async worker keeps retrying and most segments eventually get pushed. A
# generous buffer there absorbs hours of outage cleanly. This wrapper-side
# threshold gates the HARD-FAILURE path: bad creds, deleted bucket, expired
# keys — pgbackrest's foreground returns non-zero immediately and there's
# no realistic chance the next retry succeeds without operator intervention.
# Holding 5 GiB of pg_wal hostage waiting for a fix that requires a config
# change wastes data-volume disk; 500 MiB is enough to ride out a multi-
# minute config-redeploy window without eating into customer disk budgets.
# Both ceilings scale down proportionally on small volumes (1 GiB Hobby ⇒
# ~100 MiB pg_wal / ~512 MiB spool) so a tiny volume isn't dominated by
# archive buffers; on ≥25 GiB volumes both caps hold.
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
PGWAL_THRESHOLD_MB="${WAL_DROP_THRESHOLD_MB:-${PGBACKREST_DROP_THRESHOLD_MB:-500}}"
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
pgbackrest --stanza=main archive-push "$WAL_FILE"
PGB_RC=$?
if [ "$PGB_RC" -eq 0 ]; then
  exit 0
fi

PGWAL_BYTES=$(du -sb "$PGDATA/pg_wal" 2>/dev/null | awk '{print $1}')
if [ -z "${PGWAL_BYTES:-}" ]; then
  exit "$PGB_RC"
fi

if [ "$PGWAL_BYTES" -ge "$PGWAL_THRESHOLD_BYTES" ]; then
  PGWAL_MB=$(( PGWAL_BYTES / 1024 / 1024 ))
  echo "pgbackrest-wrapper: pg_wal at ${PGWAL_MB} MiB (threshold ${PGWAL_THRESHOLD_MB} MiB) and archive-push failing; dropping ${WAL_FILE} to keep Postgres up" >&2
  # Mark the gap so the backup watcher takes a fresh full once archiving
  # recovers. Without this, gap detection collapses to single-signal
  # (failed_count growth from foreground archive_command failures), but the
  # wrapper-drop path returns 0 to Postgres so failed_count never grows.
  touch "$PGDATA/.pgbackrest_gap_pending" 2>/dev/null || true
  exit 0
fi

exit "$PGB_RC"
