#!/bin/bash

# exit as soon as any of these commands fail, this prevents starting a database without certificates or with the wrong volume mount path
set -e

EXPECTED_VOLUME_MOUNT_PATH="/var/lib/postgresql/data"
DATA_DIR="$EXPECTED_VOLUME_MOUNT_PATH"
SSL_DIR="$DATA_DIR/certs"
INIT_SSL_SCRIPT="/usr/local/bin/init-ssl.sh"

# check if the Railway volume is mounted to the correct path
# we do this by checking the current mount path (RAILWAY_VOLUME_MOUNT_PATH) against the expected mount path
# if the paths are different, we print an error message and exit
# only perform this check if this image is deployed to Railway by checking for the existence of the RAILWAY_ENVIRONMENT variable
if [ -n "$RAILWAY_ENVIRONMENT" ] && [ "$RAILWAY_VOLUME_MOUNT_PATH" != "$EXPECTED_VOLUME_MOUNT_PATH" ]; then
  echo "Railway volume not mounted to the correct path, expected $EXPECTED_VOLUME_MOUNT_PATH but got $RAILWAY_VOLUME_MOUNT_PATH"
  echo "Please update the volume mount path to the expected path and redeploy the service"
  exit 1
fi

# check if PGDATA starts with the expected volume mount path
# this ensures data files are stored in the correct location
# if not, print error and exit to prevent data loss or access issues
if [ -n "$PGDATA" ] && [[ ! "$PGDATA" =~ ^"$EXPECTED_VOLUME_MOUNT_PATH" ]]; then
  echo "PGDATA variable does not start with the expected volume mount path, expected to start with $EXPECTED_VOLUME_MOUNT_PATH"
  echo "Please update the PGDATA variable to start with the expected volume mount path and redeploy the service"
  exit 1
fi

# Ensure data directory exists and has correct permissions (Railway mounts as root)
if [ ! -d "$DATA_DIR" ]; then
    sudo mkdir -p "$DATA_DIR"
fi
sudo chown postgres:postgres "$DATA_DIR"
sudo chmod 700 "$DATA_DIR"

# Check/renew SSL certificates
check_ssl_certs() {
    # Regenerate if the certificate is not a x509v3 certificate
    if [ -f "$SSL_DIR/server.crt" ] && ! openssl x509 -noout -text -in "$SSL_DIR/server.crt" | grep -q "DNS:localhost"; then
        echo "Did not find a x509v3 certificate, regenerating certificates..."
        bash "$INIT_SSL_SCRIPT"
        return
    fi

    # Regenerate if the certificate has expired or will expire
    # 2592000 seconds = 30 days
    if [ -f "$SSL_DIR/server.crt" ] && ! openssl x509 -checkend 2592000 -noout -in "$SSL_DIR/server.crt"; then
        echo "Certificate has or will expire soon, regenerating certificates..."
        bash "$INIT_SSL_SCRIPT"
        return
    fi
}

# Generate a certificate if the database was initialized but is missing a certificate
# Useful when going from the base postgres image to this ssl image
check_missing_certs() {
    local POSTGRES_CONF_FILE="$PGDATA/postgresql.conf"
    if [ -f "$POSTGRES_CONF_FILE" ] && [ ! -f "$SSL_DIR/server.crt" ]; then
        echo "Database initialized without certificate, generating certificates..."
        bash "$INIT_SSL_SCRIPT"
    fi
}

# Route based on PATRONI_ENABLED
if [ "${PATRONI_ENABLED:-false}" = "true" ]; then
    echo "=== Patroni mode enabled (with supervisord watchdog) ==="

    # Check for required passwords
    if [ ! -f "$DATA_DIR/PG_VERSION" ]; then
        if [ -z "$POSTGRES_PASSWORD" ]; then
            echo "ERROR: POSTGRES_PASSWORD is required for new database initialization."
            exit 1
        fi
        if [ -z "$PATRONI_REPLICATION_PASSWORD" ]; then
            echo "ERROR: PATRONI_REPLICATION_PASSWORD is required for HA mode."
            exit 1
        fi
    fi

    # Check/renew existing SSL certs (new certs generated in post_bootstrap)
    if [ -f "$SSL_DIR/server.crt" ]; then
        check_ssl_certs
    fi

    # Phase 2 (RFC-007): Use supervisord to manage Patroni and watchdog
    # This ensures:
    # 1. Patroni restarts automatically if it crashes
    # 2. PostgreSQL is stopped if Patroni is unrecoverable (preventing split-brain)
    # 3. Container restarts on critical failures
    exec /usr/bin/supervisord -c /etc/supervisor/conf.d/patroni.conf
else
    echo "=== Standalone PostgreSQL mode ==="

    # Check for required password on fresh installs
    if [ ! -f "$DATA_DIR/PG_VERSION" ] && [ -z "$POSTGRES_PASSWORD" ]; then
        echo "ERROR: POSTGRES_PASSWORD is required for new database initialization."
        exit 1
    fi

    # Check SSL certificates
    check_ssl_certs
    check_missing_certs

    # Generate SSL certs if missing entirely
    if [ ! -f "$SSL_DIR/server.crt" ]; then
        bash "$INIT_SSL_SCRIPT"
    fi

    # unset PGHOST to force psql to use Unix socket path
    # this is specific to Railway and allows us to use PGHOST after the init
    unset PGHOST

    # unset PGPORT also specific to Railway
    # since postgres checks for validity of the value in PGPORT we unset it in case it ends up being empty
    unset PGPORT

    # Call the entrypoint script with SSL config and redirect output to stdout if LOG_TO_STDOUT is true
    if [[ "$LOG_TO_STDOUT" == "true" ]]; then
        /usr/local/bin/docker-entrypoint.sh "$@" \
            -c ssl=on \
            -c ssl_cert_file="$SSL_DIR/server.crt" \
            -c ssl_key_file="$SSL_DIR/server.key" \
            -c ssl_ca_file="$SSL_DIR/root.crt" 2>&1
    else
        exec /usr/local/bin/docker-entrypoint.sh "$@" \
            -c ssl=on \
            -c ssl_cert_file="$SSL_DIR/server.crt" \
            -c ssl_key_file="$SSL_DIR/server.key" \
            -c ssl_ca_file="$SSL_DIR/root.crt"
    fi
fi
