#!/bin/bash
set -e

DATA_DIR="/var/lib/postgresql/data"
CERTS_DIR="/etc/postgresql/certs"

# Ensure data directory exists and has correct permissions (Railway mounts as root)
if [ ! -d "$DATA_DIR" ]; then
    sudo mkdir -p "$DATA_DIR"
fi
sudo chown postgres:postgres "$DATA_DIR"
sudo chmod 700 "$DATA_DIR"

# Generate/renew SSL certificates
generate_ssl_certs() {
    local CERT_FILE="$CERTS_DIR/server.crt"
    local KEY_FILE="$CERTS_DIR/server.key"
    local CA_FILE="$CERTS_DIR/ca.crt"
    local DAYS="${SSL_CERT_DAYS:-820}"

    # Check if certs exist and are still valid (not expiring within 30 days)
    if [ -f "$CERT_FILE" ] && [ -f "$KEY_FILE" ]; then
        if openssl x509 -checkend 2592000 -noout -in "$CERT_FILE" 2>/dev/null; then
            echo "SSL certificates are valid, skipping generation"
            return 0
        fi
        echo "SSL certificates expired or expiring soon, regenerating..."
    fi

    echo "Generating SSL certificates (valid for $DAYS days)..."

    sudo mkdir -p "$CERTS_DIR"
    sudo chown postgres:postgres "$CERTS_DIR"

    # Generate CA key and certificate
    openssl genrsa -out "$CERTS_DIR/ca.key" 2048
    openssl req -new -x509 -days "$DAYS" -key "$CERTS_DIR/ca.key" -out "$CA_FILE" \
        -subj "/CN=PostgreSQL CA"

    # Generate server key and CSR
    openssl genrsa -out "$KEY_FILE" 2048
    openssl req -new -key "$KEY_FILE" -out "$CERTS_DIR/server.csr" \
        -subj "/CN=postgres"

    # Create extension file for SAN
    cat > "$CERTS_DIR/v3.ext" <<EOF
authorityKeyIdentifier=keyid,issuer
basicConstraints=CA:FALSE
keyUsage = digitalSignature, nonRepudiation, keyEncipherment, dataEncipherment
subjectAltName = DNS:localhost,DNS:*.railway.internal,IP:127.0.0.1
EOF

    # Sign the server certificate
    openssl x509 -req -in "$CERTS_DIR/server.csr" -CA "$CA_FILE" -CAkey "$CERTS_DIR/ca.key" \
        -CAcreateserial -out "$CERT_FILE" -days "$DAYS" -extfile "$CERTS_DIR/v3.ext"

    # Set permissions
    chmod 600 "$KEY_FILE"
    chmod 644 "$CERT_FILE" "$CA_FILE"
    chown postgres:postgres "$CERTS_DIR"/*

    echo "SSL certificates generated successfully"
}

# Generate SSL certs
generate_ssl_certs

# Check for required password on fresh installs
if [ ! -f "$DATA_DIR/PG_VERSION" ] && [ -z "$POSTGRES_PASSWORD" ]; then
    echo ""
    echo "ERROR: POSTGRES_PASSWORD is required for new database initialization."
    echo ""
    echo "Set the following environment variables:"
    echo "  POSTGRES_PASSWORD=<secure_password>  (required)"
    echo "  POSTGRES_USER=<username>             (optional, default: postgres)"
    echo "  POSTGRES_DB=<database>               (optional, default: postgres)"
    echo ""
    echo "For HA mode, also set:"
    echo "  PATRONI_ENABLED=true"
    echo "  PATRONI_NAME=postgres-1"
    echo "  PATRONI_REPLICATION_PASSWORD=<password>"
    echo ""
    exit 1
fi

# Route based on PATRONI_ENABLED
if [ "${PATRONI_ENABLED:-false}" = "true" ]; then
    echo "=== Patroni mode enabled ==="
    # Run Patroni entrypoint as postgres user (PostgreSQL refuses to run as root)
    exec gosu postgres /docker-entrypoint.sh "$@"
else
    echo "=== Standalone PostgreSQL mode ==="
    # Standard postgres entrypoint handles user switching internally
    exec docker-entrypoint.sh "$@" \
        -c ssl=on \
        -c ssl_cert_file="$CERTS_DIR/server.crt" \
        -c ssl_key_file="$CERTS_DIR/server.key" \
        -c ssl_ca_file="$CERTS_DIR/ca.crt"
fi
