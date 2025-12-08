#!/bin/sh
set -e

# Generate userlist.txt from environment variables
# Format: "username" "md5<md5hash>"
# The md5 hash is: md5 + md5(password + username)

PGBOUNCER_USER="${PGBOUNCER_USER:-postgres}"
PGBOUNCER_PASSWORD="${PGBOUNCER_PASSWORD:-postgres}"

# Generate MD5 hash in PostgreSQL format
generate_md5_password() {
    local user="$1"
    local pass="$2"
    echo "md5$(echo -n "${pass}${user}" | md5sum | cut -d' ' -f1)"
}

# Create userlist.txt
mkdir -p /etc/pgbouncer
cat > /etc/pgbouncer/userlist.txt << EOF
"${PGBOUNCER_USER}" "$(generate_md5_password "${PGBOUNCER_USER}" "${PGBOUNCER_PASSWORD}")"
EOF

# Add additional users if specified (comma-separated USER:PASSWORD pairs)
if [ -n "${PGBOUNCER_EXTRA_USERS}" ]; then
    IFS=',' read -ra USERS <<< "${PGBOUNCER_EXTRA_USERS}"
    for user_pass in "${USERS[@]}"; do
        user=$(echo "$user_pass" | cut -d: -f1)
        pass=$(echo "$user_pass" | cut -d: -f2)
        echo "\"${user}\" \"$(generate_md5_password "${user}" "${pass}")\"" >> /etc/pgbouncer/userlist.txt
    done
fi

# Update pgbouncer.ini with HAProxy host and port if provided
if [ -n "${HAPROXY_HOST}" ]; then
    sed -i "s/host=haproxy/host=${HAPROXY_HOST}/g" /etc/pgbouncer/pgbouncer.ini
fi

if [ -n "${HAPROXY_PORT}" ]; then
    sed -i "s/port=5432/port=${HAPROXY_PORT}/g" /etc/pgbouncer/pgbouncer.ini
fi

# Update pool settings from environment
if [ -n "${PGBOUNCER_MAX_CLIENT_CONN}" ]; then
    sed -i "s/max_client_conn = 1000/max_client_conn = ${PGBOUNCER_MAX_CLIENT_CONN}/g" /etc/pgbouncer/pgbouncer.ini
fi

if [ -n "${PGBOUNCER_DEFAULT_POOL_SIZE}" ]; then
    sed -i "s/default_pool_size = 20/default_pool_size = ${PGBOUNCER_DEFAULT_POOL_SIZE}/g" /etc/pgbouncer/pgbouncer.ini
fi

if [ -n "${PGBOUNCER_POOL_MODE}" ]; then
    sed -i "s/pool_mode = transaction/pool_mode = ${PGBOUNCER_POOL_MODE}/g" /etc/pgbouncer/pgbouncer.ini
fi

if [ -n "${PGBOUNCER_CLIENT_TLS_SSLMODE}" ]; then
    sed -i "s/client_tls_sslmode = allow/client_tls_sslmode = ${PGBOUNCER_CLIENT_TLS_SSLMODE}/g" /etc/pgbouncer/pgbouncer.ini
fi

if [ -n "${PGBOUNCER_SERVER_TLS_SSLMODE}" ]; then
    sed -i "s/server_tls_sslmode = disable/server_tls_sslmode = ${PGBOUNCER_SERVER_TLS_SSLMODE}/g" /etc/pgbouncer/pgbouncer.ini
fi

echo "PgBouncer userlist.txt created with user: ${PGBOUNCER_USER}"

# Generate SSL certificates for client connections
SSL_DIR="/etc/pgbouncer/certs"
SSL_SERVER_CRT="$SSL_DIR/server.crt"
SSL_SERVER_KEY="$SSL_DIR/server.key"
SSL_SERVER_CSR="$SSL_DIR/server.csr"
SSL_ROOT_KEY="$SSL_DIR/root.key"
SSL_ROOT_CRT="$SSL_DIR/root.crt"
SSL_V3_EXT="$SSL_DIR/v3.ext"

if [ ! -f "$SSL_SERVER_CRT" ]; then
    echo "Generating SSL certificates..."

    mkdir -p "$SSL_DIR"

    # Generate root CA
    openssl req -new -x509 -days "${SSL_CERT_DAYS:-820}" -nodes -text -out "$SSL_ROOT_CRT" -keyout "$SSL_ROOT_KEY" -subj "/CN=root-ca"
    chmod og-rwx "$SSL_ROOT_KEY"

    # Generate server key and CSR
    openssl req -new -nodes -text -out "$SSL_SERVER_CSR" -keyout "$SSL_SERVER_KEY" -subj "/CN=localhost"
    chmod og-rwx "$SSL_SERVER_KEY"

    # Create v3 extension file
    cat > "$SSL_V3_EXT" <<EOF
[v3_req]
authorityKeyIdentifier = keyid, issuer
basicConstraints = critical, CA:TRUE
keyUsage = digitalSignature, nonRepudiation, keyEncipherment, dataEncipherment
subjectAltName = DNS:localhost
EOF

    # Sign server certificate
    openssl x509 -req -in "$SSL_SERVER_CSR" -extfile "$SSL_V3_EXT" -extensions v3_req -text -days "${SSL_CERT_DAYS:-820}" -CA "$SSL_ROOT_CRT" -CAkey "$SSL_ROOT_KEY" -CAcreateserial -out "$SSL_SERVER_CRT"

    echo "SSL certificates generated"
fi

echo "Starting PgBouncer..."

exec pgbouncer /etc/pgbouncer/pgbouncer.ini
