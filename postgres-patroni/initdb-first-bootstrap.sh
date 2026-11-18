#!/bin/sh
# Patroni runs this in place of initdb (postgresql.bin_name.initdb in
# patroni.yml), which it only does to bootstrap a new cluster on an empty
# PGDATA. It records on the volume that the initdb is starting and then
# becomes initdb. post-bootstrap removes the record when the bootstrap
# completes; patroni-runner treats a record that outlived its bootstrap as
# "PGDATA holds an interrupted first bootstrap" and discards that directory.
#
# The record sits at the volume root, beside .patroni_bootstrap_complete,
# because initdb requires an empty PGDATA. initdb's last argument is PGDATA.
set -eu

volume_root="${RAILWAY_VOLUME_MOUNT_PATH:-/var/lib/postgresql/data}"
data_dir=""
for data_dir in "$@"; do :; done

if [ -n "$data_dir" ] && [ "${data_dir%/}" != "${volume_root%/}" ]; then
  marker="${volume_root}/.patroni_initdb_started"
  : > "$marker"
  sync "$marker"
fi

exec initdb "$@"
