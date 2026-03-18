#!/bin/bash

# exit as soon as any of these commands fail, this prevents starting a database without certificates
set -e

# Set up needed variables
SSL_DIR="/var/lib/postgresql/data/certs"

SSL_SERVER_CRT="$SSL_DIR/server.crt"
SSL_SERVER_KEY="$SSL_DIR/server.key"
SSL_SERVER_CSR="$SSL_DIR/server.csr"

SSL_ROOT_KEY="$SSL_DIR/root.key"
SSL_ROOT_CRT="$SSL_DIR/root.crt"

SSL_V3_EXT="$SSL_DIR/v3.ext"

POSTGRES_CONF_FILE="$PGDATA/postgresql.conf"

# Use sudo to create the directory as root
sudo mkdir -p "$SSL_DIR"

# Use sudo to change ownership as root
sudo chown postgres:postgres "$SSL_DIR"

# Generate self-signed 509v3 certificates
# ref: https://www.postgresql.org/docs/16/ssl-tcp.html#SSL-CERTIFICATE-CREATION

# SSL_CERT_DAYS must be > 30 (our renewal threshold), otherwise certs expire immediately
# Default to 820 days if unset, empty, non-numeric, or too small
if ! [[ "${SSL_CERT_DAYS:-}" =~ ^[0-9]+$ ]] || [ "${SSL_CERT_DAYS:-0}" -le 30 ]; then
    SSL_CERT_DAYS=820
fi

openssl req -new -x509 -days "${SSL_CERT_DAYS}" -nodes -text -out "$SSL_ROOT_CRT" -keyout "$SSL_ROOT_KEY" -subj "/CN=root-ca"

chmod og-rwx "$SSL_ROOT_KEY"

openssl req -new -nodes -text -out "$SSL_SERVER_CSR" -keyout "$SSL_SERVER_KEY" -subj "/CN=localhost"

chown postgres:postgres "$SSL_SERVER_KEY"

chmod og-rwx "$SSL_SERVER_KEY"

cat >| "$SSL_V3_EXT" <<EOF
[v3_req]
authorityKeyIdentifier = keyid, issuer
basicConstraints = critical, CA:TRUE
keyUsage = digitalSignature, nonRepudiation, keyEncipherment, dataEncipherment
subjectAltName = DNS:localhost
EOF

openssl x509 -req -in "$SSL_SERVER_CSR" -extfile "$SSL_V3_EXT" -extensions v3_req -text -days "${SSL_CERT_DAYS}" -CA "$SSL_ROOT_CRT" -CAkey "$SSL_ROOT_KEY" -CAcreateserial -out "$SSL_SERVER_CRT"

chown postgres:postgres "$SSL_SERVER_CRT"

# In Patroni mode, SSL is configured via patroni.yml, not postgresql.conf
# Only modify postgresql.conf for standalone (non-Patroni) PostgreSQL
if [ "${PATRONI_ENABLED:-false}" != "true" ]; then
    cat >> "$POSTGRES_CONF_FILE" <<EOF
ssl = on
ssl_cert_file = '$SSL_SERVER_CRT'
ssl_key_file = '$SSL_SERVER_KEY'
ssl_ca_file = '$SSL_ROOT_CRT'
shared_preload_libraries = 'pg_stat_statements'
EOF
else
    echo "Patroni mode: SSL certs generated, Patroni will configure PostgreSQL SSL settings"
fi
