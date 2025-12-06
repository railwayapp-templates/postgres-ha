#!/bin/bash
set -e

# Patroni mode entrypoint (only runs when PATRONI_ENABLED=true)

DATA_DIR="/var/lib/postgresql/data"
CERTS_DIR="$DATA_DIR/certs"

echo "=== Patroni Entrypoint ==="

# Configuration
SCOPE="${PATRONI_SCOPE:-railway-pg-ha}"
NAME="${PATRONI_NAME:-postgres-1}"
ETCD_HOSTS="${PATRONI_ETCD_HOSTS:-etcd-1.railway.internal:2379,etcd-2.railway.internal:2379,etcd-3.railway.internal:2379}"

# Credentials
SUPERUSER="${PATRONI_SUPERUSER_USERNAME:-${POSTGRES_USER:-postgres}}"
SUPERUSER_PASS="${PATRONI_SUPERUSER_PASSWORD:-${POSTGRES_PASSWORD}}"
REPL_USER="${PATRONI_REPLICATION_USERNAME:-replicator}"
REPL_PASS="${PATRONI_REPLICATION_PASSWORD}"

# Primary is postgres-1
IS_PRIMARY=false
[ "$NAME" = "postgres-1" ] && IS_PRIMARY=true

echo "Node: $NAME (primary: $IS_PRIMARY)"

# Check for valid PostgreSQL data
HAS_VALID_DATA=false
if [ -f "$DATA_DIR/global/pg_control" ]; then
    HAS_VALID_DATA=true
    echo "Found valid pg_control"
else
    echo "No valid pg_control found"
    echo "Data dir contents:"
    ls -la "$DATA_DIR" 2>/dev/null || echo "(empty or doesn't exist)"
fi

# Clean stale data for fresh bootstrap/clone
if [ "$HAS_VALID_DATA" = "false" ]; then
    echo "Cleaning data directory..."
    find "$DATA_DIR" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
    echo "Data dir after cleanup:"
    ls -la "$DATA_DIR"

    # Clear etcd state (use v3 API)
    FIRST_ETCD="${ETCD_HOSTS%%,*}"
    echo "Clearing etcd state at $FIRST_ETCD for scope $SCOPE..."
    etcdctl --endpoints="http://$FIRST_ETCD" del "/service/$SCOPE" --prefix 2>/dev/null || \
        curl -s -X DELETE "http://$FIRST_ETCD/v2/keys/service/$SCOPE?recursive=true" 2>/dev/null || true
fi

# Generate Patroni configuration
cat > /tmp/patroni.yml <<EOF
scope: ${SCOPE}
name: ${NAME}

restapi:
  listen: 0.0.0.0:8008
  connect_address: ${NAME}.railway.internal:8008

etcd:
  hosts: ${ETCD_HOSTS}

bootstrap:
  dcs:
    ttl: ${PATRONI_TTL:-30}
    loop_wait: ${PATRONI_LOOP_WAIT:-10}
    retry_timeout: 10
    maximum_lag_on_failover: 1048576
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

  pg_hba:
    - local all all trust
    - hostssl replication ${REPL_USER} 0.0.0.0/0 md5
    - hostssl all all 0.0.0.0/0 md5
    - host replication ${REPL_USER} 0.0.0.0/0 md5
    - host all all 0.0.0.0/0 md5

  post_bootstrap: /post_bootstrap.sh

postgresql:
  listen: 0.0.0.0:5432
  connect_address: ${NAME}.railway.internal:5432
  data_dir: ${DATA_DIR}
  pgpass: /tmp/pgpass
  remove_data_directory_on_rewind_failure: true
  remove_data_directory_on_diverged_timelines: true
  authentication:
    replication:
      username: ${REPL_USER}
      password: ${REPL_PASS}
    superuser:
      username: ${SUPERUSER}
      password: ${SUPERUSER_PASS}
  parameters:
    unix_socket_directories: /var/run/postgresql
    ssl: "on"
    ssl_cert_file: "${CERTS_DIR}/server.crt"
    ssl_key_file: "${CERTS_DIR}/server.key"
    ssl_ca_file: "${CERTS_DIR}/ca.crt"
EOF

echo "Starting Patroni (scope: $SCOPE, etcd: $ETCD_HOSTS)"

# Cleanup on exit
cleanup() {
    echo "Patroni exiting, stopping PostgreSQL..."
    pkill -9 -f "postgres" 2>/dev/null || true
    exit 1
}
trap cleanup EXIT SIGTERM SIGINT

# Start Patroni
exec patroni /tmp/patroni.yml
