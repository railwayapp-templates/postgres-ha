#!/bin/bash
# patroni-runner.sh - Wrapper to run Patroni with proper setup
#
# This script is called by supervisord to start Patroni.
# It generates the configuration and starts Patroni.

set -e

DATA_DIR="/var/lib/postgresql/data"
CERTS_DIR="$DATA_DIR/certs"

echo "=== Patroni Runner ==="

# Configuration - all values from environment variables, no defaults
SCOPE="${PATRONI_SCOPE:-railway-pg-ha}"
NAME="${PATRONI_NAME}"
CONNECT_ADDRESS="${RAILWAY_PRIVATE_DOMAIN}"
ETCD_HOSTS="${PATRONI_ETCD_HOSTS}"

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
    echo "ERROR: PATRONI_ETCD_HOSTS must be set"
    exit 1
fi

# Credentials
SUPERUSER="${POSTGRES_USER:-postgres}"
SUPERUSER_PASS="${POSTGRES_PASSWORD}"
REPL_USER="${PATRONI_REPLICATION_USERNAME:-replicator}"
REPL_PASS="${PATRONI_REPLICATION_PASSWORD}"

echo "Node: $NAME (address: $CONNECT_ADDRESS)"

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

# Clean stale data for fresh bootstrap/clone (keep certs)
if [ "$HAS_VALID_DATA" = "false" ]; then
    echo "Cleaning data directory (keeping certs)..."
    find "$DATA_DIR" -mindepth 1 -maxdepth 1 ! -name 'certs' -exec rm -rf {} +
fi

# Self-healing: ensure users exist with correct attributes once PostgreSQL is up
# Runs in background to not block Patroni startup
ensure_users() {
    for i in $(seq 1 60); do
        if pg_isready -h /var/run/postgresql -q 2>/dev/null; then
            sleep 2
            echo "Ensuring PostgreSQL users are correctly configured (superuser: ${SUPERUSER})..."
            # Use the actual superuser, not hardcoded 'postgres'
            # Connect via Unix socket which uses 'local all all trust' from pg_hba
            psql -h /var/run/postgresql -U "${SUPERUSER}" -d postgres <<-EOSQL
                -- Ensure replicator user exists with LOGIN
                DO \$\$
                BEGIN
                    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '${REPL_USER}') THEN
                        CREATE ROLE ${REPL_USER} WITH REPLICATION LOGIN PASSWORD '${REPL_PASS}';
                        RAISE NOTICE 'Created user: ${REPL_USER}';
                    ELSE
                        ALTER ROLE ${REPL_USER} WITH REPLICATION LOGIN PASSWORD '${REPL_PASS}';
                        RAISE NOTICE 'Updated user: ${REPL_USER}';
                    END IF;
                END
                \$\$;
EOSQL
            echo "User configuration complete"
            return 0
        fi
        sleep 2
    done
    echo "WARNING: Timed out waiting for PostgreSQL"
}
ensure_users &

# Generate Patroni configuration
cat > /tmp/patroni.yml <<EOF
scope: ${SCOPE}
name: ${NAME}

restapi:
  listen: 0.0.0.0:8008
  connect_address: ${CONNECT_ADDRESS}:8008

etcd:
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

  users:
    ${SUPERUSER}:
      password: ${SUPERUSER_PASS}
      options:
        - superuser
        - createdb
        - createrole
        - login
    ${REPL_USER}:
      password: ${REPL_PASS}
      options:
        - replication
        - login

  pg_hba:
    - local all all trust
    - hostssl replication ${REPL_USER} 0.0.0.0/0 md5
    - hostssl all all 0.0.0.0/0 md5
    - host replication ${REPL_USER} 0.0.0.0/0 md5
    - host all all 0.0.0.0/0 md5

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
      username: ${REPL_USER}
      password: ${REPL_PASS}
    superuser:
      username: ${SUPERUSER}
      password: ${SUPERUSER_PASS}
  parameters:
    unix_socket_directories: /var/run/postgresql
EOF

echo "Starting Patroni (scope: $SCOPE, etcd: $ETCD_HOSTS)"

# Note: Replicator user is created via bootstrap.users (above) during initdb.
# post_bootstrap.sh provides a safety net to ensure the user exists.

# Start Patroni (exec to replace this shell process)
exec patroni /tmp/patroni.yml
