#!/bin/bash
# post_bootstrap.sh - Runs ONCE after PostgreSQL initialization on primary
#
# Patroni 4.0+ removed bootstrap.users, so users must be created/updated here.
# Patroni passes:
#   $1 = connection string URL (not used - we connect via Unix socket)
#   PGPASSFILE = path to pgpass file for authentication

set -e

echo "Post-bootstrap: starting..."

VOLUME_ROOT="${RAILWAY_VOLUME_MOUNT_PATH:-/var/lib/postgresql/data}"

# CRITICAL: Read credentials from environment variables directly
# This ensures we use the EXACT same values that Patroni uses
# (Patroni reads from PATRONI_* env vars, bypassing YAML)
SUPERUSER="${PATRONI_SUPERUSER_USERNAME:-postgres}"
SUPERUSER_PASS="${PATRONI_SUPERUSER_PASSWORD}"
REPL_USER="${PATRONI_REPLICATION_USERNAME:-replicator}"
REPL_PASS="${PATRONI_REPLICATION_PASSWORD}"
APP_USER="${POSTGRES_USER:-postgres}"
APP_PASS="${POSTGRES_PASSWORD}"

echo "DEBUG: Reading credentials from environment variables (same source as Patroni)"

echo "DEBUG: SUPERUSER=$SUPERUSER"
echo "DEBUG: REPL_USER=$REPL_USER"
echo "DEBUG: REPL_PASS length=${#REPL_PASS}"
echo "DEBUG: REPL_PASS first4=${REPL_PASS:0:4} last4=${REPL_PASS: -4}"

# Validate required credentials
if [ -z "$SUPERUSER" ]; then
    echo "ERROR: Could not extract SUPERUSER from config"
    exit 1
fi
if [ -z "$REPL_USER" ]; then
    echo "ERROR: Could not extract REPL_USER from config"
    exit 1
fi
if [ -z "$REPL_PASS" ]; then
    echo "ERROR: Could not extract REPL_PASS from config"
    exit 1
fi

# Use Unix socket with superuser (trust auth for local)
PSQL="psql -h /var/run/postgresql -U $SUPERUSER -d postgres"

# 1. Set superuser password
echo "Setting superuser password..."
$PSQL -c "ALTER ROLE \"$SUPERUSER\" WITH PASSWORD '$(echo "$SUPERUSER_PASS" | sed "s/'/''/g")'"
echo "Superuser password set"

# 2. Create replicator user (Patroni 4.0+ no longer creates it automatically)
echo "Creating replicator user..."
$PSQL -c "CREATE ROLE \"$REPL_USER\" WITH REPLICATION LOGIN PASSWORD '$(echo "$REPL_PASS" | sed "s/'/''/g")'"
echo "Replicator user created"

# 3. VERIFY: Show password hash type (should be SCRAM-SHA-256$...)
echo "Verifying password storage..."
$PSQL -c "SELECT rolname, LEFT(rolpassword, 14) as hash_prefix FROM pg_authid WHERE rolname IN ('$SUPERUSER', '$REPL_USER')"

# 4. CRITICAL TEST: Verify replicator can authenticate via TCP
echo "Testing replicator authentication via TCP (regular connection)..."
if PGPASSWORD="$REPL_PASS" psql -h 127.0.0.1 -U "$REPL_USER" -d postgres -c "SELECT 1 as auth_test" 2>&1; then
    echo "SUCCESS: Regular connection test PASSED"
else
    echo "ERROR: Regular connection test FAILED!"
fi

# 5. CRITICAL TEST: Verify replication connection works (this is what pg_basebackup uses)
echo "Testing replicator authentication via TCP (replication connection)..."
if PGPASSWORD="$REPL_PASS" psql "host=127.0.0.1 port=5432 dbname=postgres user=$REPL_USER replication=database" -c "IDENTIFY_SYSTEM;" 2>&1; then
    echo "SUCCESS: Replication connection test PASSED"
else
    echo "ERROR: Replication connection test FAILED!"
    echo "--- pg_hba.conf replication lines ---"
    grep -i replication "${PGDATA}/pg_hba.conf" 2>/dev/null || echo "(could not grep pg_hba.conf)"
    echo "--- END ---"
fi

# 6. Create/update app user if different from superuser
if [ -n "$APP_USER" ] && [ "$APP_USER" != "$SUPERUSER" ] && [ -n "$APP_PASS" ]; then
    echo "Setting up app user: $APP_USER"
    $PSQL -c "DO \$\$ BEGIN
        IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '$APP_USER') THEN
            CREATE ROLE \"$APP_USER\" WITH LOGIN PASSWORD '$(echo "$APP_PASS" | sed "s/'/''/g")';
            RAISE NOTICE 'Created app user: $APP_USER';
        ELSE
            ALTER ROLE \"$APP_USER\" WITH PASSWORD '$(echo "$APP_PASS" | sed "s/'/''/g")';
            RAISE NOTICE 'Updated app user password: $APP_USER';
        END IF;
    END \$\$"
fi

# Generate SSL certificates
echo "Generating SSL certificates..."
bash /docker-entrypoint-initdb.d/init-ssl.sh

# Mark bootstrap complete
touch "$VOLUME_ROOT/.patroni_bootstrap_complete"

echo "Post-bootstrap: completed successfully"
