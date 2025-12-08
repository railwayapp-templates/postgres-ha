#!/bin/sh
set -e

CONFIG_FILE="/usr/local/etc/haproxy/haproxy.cfg"

# Required
if [ -z "${POSTGRES_NODES}" ]; then
    echo "ERROR: POSTGRES_NODES is required"
    echo "Format: hostname:pgport:patroniport,hostname:pgport:patroniport,..."
    exit 1
fi

# Optional with defaults
HAPROXY_MAX_CONN="${HAPROXY_MAX_CONN:-1000}"
HAPROXY_TIMEOUT_CONNECT="${HAPROXY_TIMEOUT_CONNECT:-10s}"
HAPROXY_TIMEOUT_CLIENT="${HAPROXY_TIMEOUT_CLIENT:-30m}"
HAPROXY_TIMEOUT_SERVER="${HAPROXY_TIMEOUT_SERVER:-30m}"
HAPROXY_CHECK_INTERVAL="${HAPROXY_CHECK_INTERVAL:-3s}"

# Generate server entries from POSTGRES_NODES
# Format: hostname:pgport:patroniport,hostname:pgport:patroniport,...
generate_servers() {
    local backend_name="$1"
    local i=0
    echo "$POSTGRES_NODES" | tr ',' '\n' | while read -r node; do
        host=$(echo "$node" | cut -d: -f1)
        pgport=$(echo "$node" | cut -d: -f2)
        patroniport=$(echo "$node" | cut -d: -f3)
        name=$(echo "$host" | cut -d. -f1)
        echo "    server ${name} ${host}:${pgport} check port ${patroniport}"
        i=$((i + 1))
    done
}

# Generate HAProxy config
cat > "$CONFIG_FILE" << EOF
global
    maxconn ${HAPROXY_MAX_CONN}
    log stdout format raw local0

defaults
    log global
    mode tcp
    retries 3
    timeout connect ${HAPROXY_TIMEOUT_CONNECT}
    timeout client ${HAPROXY_TIMEOUT_CLIENT}
    timeout server ${HAPROXY_TIMEOUT_SERVER}
    timeout check 5s

resolvers railway
    nameserver dns1 127.0.0.11:53
    resolve_retries 3
    timeout resolve 1s
    timeout retry   1s
    hold other      10s
    hold refused    10s
    hold nx         10s
    hold timeout    10s
    hold valid      10s
    hold obsolete   10s

# Stats page for monitoring
listen stats
    bind *:8404
    mode http
    stats enable
    stats uri /stats
    stats refresh 10s

# Primary PostgreSQL (read-write)
frontend postgresql_primary
    bind *:5432
    default_backend postgresql_primary_backend

backend postgresql_primary_backend
    option httpchk GET /primary
    http-check expect status 200
    default-server inter ${HAPROXY_CHECK_INTERVAL} fall 3 rise 2 on-marked-down shutdown-sessions resolvers railway init-addr none
$(generate_servers primary)

# Replica PostgreSQL (read-only)
frontend postgresql_replicas
    bind *:5433
    default_backend postgresql_replicas_backend

backend postgresql_replicas_backend
    balance roundrobin
    option httpchk GET /replica
    http-check expect status 200
    default-server inter ${HAPROXY_CHECK_INTERVAL} fall 3 rise 2 on-marked-down shutdown-sessions resolvers railway init-addr none
$(generate_servers replica)
EOF

echo "HAProxy config generated with nodes: ${POSTGRES_NODES}"
echo "Starting HAProxy..."

exec haproxy -f "$CONFIG_FILE"
