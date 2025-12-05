#!/bin/bash
set -e

echo "Post-bootstrap: configuring users..."

# Use environment variables directly (set by docker-entrypoint.sh)
SUPERUSER="${PATRONI_SUPERUSER_USERNAME:-${POSTGRES_USER:-postgres}}"
SUPERUSER_PASS="${PATRONI_SUPERUSER_PASSWORD:-${POSTGRES_PASSWORD}}"
REPL_USER="${PATRONI_REPLICATION_USERNAME:-replicator}"
REPL_PASS="${PATRONI_REPLICATION_PASSWORD}"

if [ -z "$SUPERUSER_PASS" ] || [ -z "$REPL_PASS" ]; then
    echo "ERROR: Missing required passwords"
    exit 1
fi

# Connect via Unix socket (pg_hba.conf has "local all all trust")
psql -v ON_ERROR_STOP=1 -U postgres -d postgres <<-EOSQL
    -- Configure superuser
    DO \$\$
    BEGIN
        IF '${SUPERUSER}' != 'postgres' THEN
            IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '${SUPERUSER}') THEN
                CREATE ROLE ${SUPERUSER} WITH SUPERUSER CREATEDB CREATEROLE LOGIN PASSWORD '${SUPERUSER_PASS}';
            ELSE
                ALTER USER ${SUPERUSER} WITH PASSWORD '${SUPERUSER_PASS}';
            END IF;
        ELSE
            ALTER USER postgres WITH PASSWORD '${SUPERUSER_PASS}';
        END IF;
    END
    \$\$;

    -- Create replicator user
    DO \$\$
    BEGIN
        IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '${REPL_USER}') THEN
            CREATE ROLE ${REPL_USER} WITH REPLICATION PASSWORD '${REPL_PASS}' LOGIN;
        END IF;
    END
    \$\$;
EOSQL

# Create POSTGRES_DB if specified
if [ -n "$POSTGRES_DB" ] && [ "$POSTGRES_DB" != "postgres" ]; then
    echo "Creating database: $POSTGRES_DB"
    psql -v ON_ERROR_STOP=1 -U postgres -d postgres <<-EOSQL
        SELECT 'CREATE DATABASE "${POSTGRES_DB}" OWNER "${SUPERUSER}"'
        WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = '${POSTGRES_DB}')\gexec
EOSQL
fi

echo "Post-bootstrap completed"
