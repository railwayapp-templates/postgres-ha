#!/bin/bash
# patroni-runner.sh - Wrapper to run Patroni with proper setup
#
# This script is called by supervisord to start Patroni.
# It generates the configuration and starts Patroni.

set -e

DATA_DIR="${PGDATA:-/var/lib/postgresql/data/pgdata}"
VOLUME_ROOT="${RAILWAY_VOLUME_MOUNT_PATH:-/var/lib/postgresql/data}"
CERTS_DIR="$VOLUME_ROOT/certs"

echo "=== Patroni Runner ==="

# Configuration - all values from environment variables, no defaults
SCOPE="${PATRONI_SCOPE:-railway-pg-ha}"
NAME="${PATRONI_NAME}"
CONNECT_ADDRESS="${RAILWAY_PRIVATE_DOMAIN}"
# Use PATRONI_ETCD3_HOSTS for v3 API (NOT PATRONI_ETCD_HOSTS which is v2)
ETCD_HOSTS="${PATRONI_ETCD3_HOSTS}"

# Validate required env vars
if [ -z "$NAME" ]; then
    echo "ERROR: PATRONI_NAME must be set"
    exit 1
fi
if [ -z "$CONNECT_ADDRESS" ]; then
    echo "ERROR: RAILWAY_PRIVATE_DOMAIN must be set"
    exit 1
fi
if [ -z "$ETCD_HOSTS" ]; then
    echo "ERROR: PATRONI_ETCD3_HOSTS must be set"
    exit 1
fi

# Credentials - use Patroni-specific env vars
SUPERUSER="${PATRONI_SUPERUSER_USERNAME:-postgres}"
SUPERUSER_PASS="${PATRONI_SUPERUSER_PASSWORD}"
REPL_USER="${PATRONI_REPLICATION_USERNAME:-replicator}"
REPL_PASS="${PATRONI_REPLICATION_PASSWORD}"
# App user (standard postgres env vars)
APP_USER="${POSTGRES_USER:-postgres}"
APP_PASS="${POSTGRES_PASSWORD}"

echo "Node: $NAME (address: $CONNECT_ADDRESS)"
echo "DEBUG: REPL_USER=$REPL_USER"
echo "DEBUG: REPL_PASS length=${#REPL_PASS}"
echo "DEBUG: REPL_PASS first4=${REPL_PASS:0:4} last4=${REPL_PASS: -4}"

# Bootstrap completion marker (like etcd pattern)
# pg_control can exist from a failed bootstrap - only trust data if marker exists
BOOTSTRAP_MARKER="$VOLUME_ROOT/.patroni_bootstrap_complete"

HAS_VALID_DATA=false
if [ -f "$DATA_DIR/global/pg_control" ] && [ -f "$BOOTSTRAP_MARKER" ]; then
    HAS_VALID_DATA=true
    echo "Found valid data with bootstrap marker"
elif [ -f "$DATA_DIR/global/pg_control" ]; then
    echo "Found pg_control but NO bootstrap marker - stale data from failed bootstrap"
else
    echo "No PostgreSQL data found"
fi

# Clean stale/incomplete data (keep certs which are at volume root)
if [ "$HAS_VALID_DATA" = "false" ]; then
    echo "Cleaning data directory for fresh bootstrap..."
    if [ -d "$DATA_DIR" ]; then
        rm -rf "$DATA_DIR"/*
    fi
    mkdir -p "$DATA_DIR"
fi

# Generate Patroni configuration
cat > /tmp/patroni.yml <<EOF
scope: ${SCOPE}
name: ${NAME}

restapi:
  listen: 0.0.0.0:8008
  connect_address: ${CONNECT_ADDRESS}:8008

etcd3:
  hosts: ${ETCD_HOSTS}

bootstrap:
  dcs:
    ttl: ${PATRONI_TTL:-45}
    loop_wait: ${PATRONI_LOOP_WAIT:-5}
    retry_timeout: ${PATRONI_RETRY_TIMEOUT:-5}
    maximum_lag_on_failover: 1048576
    failsafe_mode: true
    postgresql:
      use_pg_rewind: true
      use_slots: true
      parameters:
        wal_level: replica
        hot_standby: "on"
        max_wal_senders: 10
        max_replication_slots: 10
        max_connections: 200

  initdb:
    - encoding: UTF8
    - data-checksums
    - username: ${SUPERUSER}

  # Note: bootstrap.users was removed in Patroni 4.0
  # Users are created in post_bootstrap script instead

  pg_hba:
    - local all all trust
    - hostssl replication ${REPL_USER} 0.0.0.0/0 scram-sha-256
    - hostssl all all 0.0.0.0/0 scram-sha-256
    - host replication ${REPL_USER} 0.0.0.0/0 scram-sha-256
    - host all all 0.0.0.0/0 scram-sha-256

  post_bootstrap: /post_bootstrap.sh

postgresql:
  listen: 0.0.0.0:5432
  connect_address: ${CONNECT_ADDRESS}:5432
  data_dir: ${DATA_DIR}
  pgpass: /tmp/pgpass
  remove_data_directory_on_rewind_failure: true
  remove_data_directory_on_diverged_timelines: true
  authentication:
    replication:
      username: "${REPL_USER}"
      password: "${REPL_PASS}"
    superuser:
      username: "${SUPERUSER}"
      password: "${SUPERUSER_PASS}"
  # Custom section for post_bootstrap to read app user credentials
  app_user:
    username: "${APP_USER}"
    password: "${APP_PASS}"
  parameters:
    unix_socket_directories: /var/run/postgresql
EOF

echo "Starting Patroni (scope: $SCOPE, etcd: $ETCD_HOSTS)"

# Start Patroni (exec to replace this shell process)
exec patroni /tmp/patroni.yml
