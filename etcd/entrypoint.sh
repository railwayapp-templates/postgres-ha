#!/bin/sh
set -e

mkdir -p /etcd-data/data
chmod 700 /etcd-data/data

exec /usr/local/bin/etcd "$@"
