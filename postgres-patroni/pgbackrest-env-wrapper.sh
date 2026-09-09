#!/bin/sh
# These are image settings, not pgBackRest options. patroni-runner renders
# the worker counts into command-specific sections of pgbackrest.conf.
# Inheriting them into pgBackRest produces early warnings on stdout, even
# for `info --output=json`, breaking the backup watcher's catalog probes.
unset PGBACKREST_BACKUP_PROCESS_MAX
unset PGBACKREST_ARCHIVE_PUSH_PROCESS_MAX
unset PGBACKREST_ARCHIVE_GET_PROCESS_MAX
unset PGBACKREST_RESTORE_PROCESS_MAX
# Legacy alias consumed by pgbackrest-archive-push-wrapper.sh before this
# launcher runs. Keep it available to that wrapper, but not to pgBackRest.
unset PGBACKREST_DROP_THRESHOLD_MB

# Keep native PGBACKREST_* options, argument boundaries, exit status, and
# signal delivery intact. The absolute path avoids recursing through PATH.
exec /usr/bin/pgbackrest "$@"
