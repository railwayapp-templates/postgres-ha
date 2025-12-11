#!/bin/bash
# patroni-runner.sh - Wrapper to run Patroni with proper setup
#
# This script generates the Patroni configuration and starts Patroni.
# Runs as PID 1 in container with built-in health monitoring.
# If Patroni dies or becomes unresponsive, exits to trigger container restart.

set -e

DATA_DIR="${PGDATA:-/var/lib/postgresql/data/pgdata}"
VOLUME_ROOT="${RAILWAY_VOLUME_MOUNT_PATH:-/var/lib/postgresql/data}"
CERTS_DIR="$VOLUME_ROOT/certs"

echo "=== Patroni Runner ==="

# Configuration - all values from environment variables, no defaults
SCOPE="${PATRONI_SCOPE:-railway-pg-ha}"
NAME="${PATRONI_NAME}"
CONNECT_ADDRESS="${RAILWAY_PRIVATE_DOMAIN}"
# Use PATRONI_ETCD3_HOSTS for v3 API
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
# App user and database (standard postgres env vars)
APP_USER="${POSTGRES_USER:-postgres}"
APP_PASS="${POSTGRES_PASSWORD}"
APP_DB="${POSTGRES_DB:-${PGDATABASE:-railway}}"

echo "Node: $NAME (address: $CONNECT_ADDRESS)"

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
    chmod 700 "$DATA_DIR"

    # CRITICAL: Check for stale etcd state that would prevent bootstrap
    # If /initialize key exists but no leader, we're stuck - clean it up
    echo "Checking etcd for stale cluster state..."
    FIRST_ETCD_HOST="${ETCD_HOSTS%%,*}"

    # Use Python (available from Patroni install) to check/clean etcd
    python3 << PYEOF
import sys
try:
    import etcd3

    host, port = "${FIRST_ETCD_HOST}".rsplit(":", 1)
    client = etcd3.client(host=host, port=int(port))

    scope = "${SCOPE}"
    init_key = f"/service/{scope}/initialize"
    leader_key = f"/service/{scope}/leader"

    # Check if initialize key exists
    init_value, _ = client.get(init_key)
    if init_value is None:
        print("No /initialize key - fresh cluster, will bootstrap normally")
        sys.exit(0)

    # Initialize exists - check if there's a leader
    leader_value, _ = client.get(leader_key)
    if leader_value is not None:
        print(f"Leader exists: {leader_value.decode()} - will replicate from it")
        sys.exit(0)

    # Stale state: initialize exists but no leader
    print("STALE STATE DETECTED: /initialize exists but no leader!")
    print("This means a previous cluster died without cleanup.")
    print("Cleaning etcd state to allow fresh bootstrap...")

    # Delete all keys under /service/{scope}/
    deleted = client.delete_prefix(f"/service/{scope}/")
    print(f"Deleted Patroni cluster state from etcd (prefix: /service/{scope}/)")

except Exception as e:
    print(f"Warning: Could not check/clean etcd state: {e}")
    print("Proceeding anyway - Patroni may handle it")
PYEOF
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
    loop_wait: ${PATRONI_LOOP_WAIT:-10}
    retry_timeout: ${PATRONI_RETRY_TIMEOUT:-30}
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
        password_encryption: scram-sha-256

  initdb:
    - encoding: UTF8
    - data-checksums
    - username: ${SUPERUSER}

  # Note: bootstrap.users was removed in Patroni 4.0
  # Users are created in post_bootstrap script instead

  pg_hba:
    - local all all trust
    - hostssl replication ${REPL_USER} 0.0.0.0/0 scram-sha-256
    - hostssl replication ${REPL_USER} ::/0 scram-sha-256
    - hostssl all all 0.0.0.0/0 scram-sha-256
    - hostssl all all ::/0 scram-sha-256
    - host replication ${REPL_USER} 0.0.0.0/0 scram-sha-256
    - host replication ${REPL_USER} ::/0 scram-sha-256
    - host all all 0.0.0.0/0 scram-sha-256
    - host all all ::/0 scram-sha-256

  post_bootstrap: /post_bootstrap.sh

postgresql:
  listen: "*:5432"
  connect_address: ${CONNECT_ADDRESS}:5432
  data_dir: ${DATA_DIR}
  pgpass: /tmp/pgpass
  remove_data_directory_on_rewind_failure: true
  remove_data_directory_on_diverged_timelines: true
  create_replica_methods:
    - basebackup
  basebackup:
    checkpoint: "fast"
    wal-method: "stream"
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
    database: "${APP_DB}"
  parameters:
    unix_socket_directories: /var/run/postgresql
EOF

echo "Starting Patroni (scope: $SCOPE, etcd: $ETCD_HOSTS)"

# Ensure data directory has correct permissions (PostgreSQL requires 0700)
mkdir -p "$DATA_DIR"
chmod 700 "$DATA_DIR"

# Set umask so pg_basebackup creates files with correct permissions
# Without this, container environments may create files too permissive
umask 0077

# CRITICAL: Unset PG* environment variables that would override Patroni's pgpass
# PGPASSWORD takes precedence over pgpass file, causing pg_basebackup to use
# the wrong password (app user's password instead of replicator's)
# See: https://github.com/patroni/patroni/issues/1489
unset PGPASSWORD PGUSER PGHOST PGPORT PGDATABASE

# Health monitoring configuration
HEALTH_CHECK_INTERVAL="${PATRONI_HEALTH_CHECK_INTERVAL:-5}"
HEALTH_CHECK_TIMEOUT="${PATRONI_HEALTH_CHECK_TIMEOUT:-5}"
MAX_FAILURES="${PATRONI_MAX_HEALTH_FAILURES:-3}"
STARTUP_GRACE_PERIOD="${PATRONI_STARTUP_GRACE_PERIOD:-60}"

# Start Patroni as child process (not exec, so we can monitor it)
patroni /tmp/patroni.yml &
PATRONI_PID=$!
echo "Patroni started with PID $PATRONI_PID"

# Forward termination signals to Patroni for graceful shutdown
cleanup() {
    echo "Received shutdown signal, stopping Patroni..."
    kill -TERM "$PATRONI_PID" 2>/dev/null || true
    wait "$PATRONI_PID" 2>/dev/null || true
    exit 0
}
trap cleanup TERM INT

# Wait for startup grace period before health checking
echo "Waiting ${STARTUP_GRACE_PERIOD}s for Patroni to initialize..."
startup_elapsed=0
while [ $startup_elapsed -lt $STARTUP_GRACE_PERIOD ]; do
    # Check if process died during startup
    if ! kill -0 "$PATRONI_PID" 2>/dev/null; then
        echo "Patroni process died during startup"
        wait "$PATRONI_PID" 2>/dev/null || true
        exit 1
    fi
    sleep 5
    startup_elapsed=$((startup_elapsed + 5))

    # Try health check - if it succeeds early, we can start monitoring
    if curl -sf --max-time "$HEALTH_CHECK_TIMEOUT" http://localhost:8008/health > /dev/null 2>&1; then
        echo "Patroni healthy after ${startup_elapsed}s, starting health monitoring"
        break
    fi
done

# Main health monitoring loop
failures=0
echo "Health monitoring active (interval=${HEALTH_CHECK_INTERVAL}s, max_failures=${MAX_FAILURES})"

while true; do
    sleep "$HEALTH_CHECK_INTERVAL"

    # Check if Patroni process is alive
    if ! kill -0 "$PATRONI_PID" 2>/dev/null; then
        echo "Patroni process died unexpectedly"
        wait "$PATRONI_PID" 2>/dev/null || true
        exit 1
    fi

    # Check Patroni health endpoint (detects hangs)
    if curl -sf --max-time "$HEALTH_CHECK_TIMEOUT" http://localhost:8008/health > /dev/null 2>&1; then
        if [ $failures -gt 0 ]; then
            echo "Patroni recovered after $failures failed health checks"
        fi
        failures=0
    else
        failures=$((failures + 1))
        echo "Health check failed ($failures/$MAX_FAILURES)"

        if [ $failures -ge $MAX_FAILURES ]; then
            echo "CRITICAL: Patroni unresponsive after $MAX_FAILURES checks - exiting to trigger restart"
            kill -TERM "$PATRONI_PID" 2>/dev/null || true
            sleep 2
            kill -KILL "$PATRONI_PID" 2>/dev/null || true
            exit 1
        fi
    fi
done
