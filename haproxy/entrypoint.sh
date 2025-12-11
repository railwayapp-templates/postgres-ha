#!/bin/sh
set -e

CONFIG_FILE="/usr/local/etc/haproxy/haproxy.cfg"

# Required
if [ -z "${POSTGRES_NODES}" ]; then
    echo "ERROR: POSTGRES_NODES is required"
    echo "Format: hostname:pgport:patroniport,hostname:pgport:patroniport,..."
    echo "Example: postgres-1.railway.internal:5432:8008,postgres-2.railway.internal:5432:8008"
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
    echo "$POSTGRES_NODES" | tr ',' '\n' | while read -r node; do
        # Count colons to detect format
        colon_count=$(echo "$node" | tr -cd ':' | wc -c)

        if [ "$colon_count" -eq 2 ]; then
            # Format: hostname:pgport:patroniport
            host=$(echo "$node" | cut -d: -f1)
            pgport=$(echo "$node" | cut -d: -f2)
            patroniport=$(echo "$node" | cut -d: -f3)
        else
            echo "ERROR: Invalid node format: $node" >&2
            echo "Expected: hostname:pgport:patroniport" >&2
            exit 1
        fi

        # Extract short name from hostname (e.g., postgres-1 from postgres-1.railway.internal)
        name=$(echo "$host" | cut -d. -f1)
        echo "    server ${name} ${host}:${pgport} check port ${patroniport}"
    done
}

PRIMARY_SERVERS=$(generate_servers)
REPLICA_SERVERS=$(generate_servers)

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
    parse-resolv-conf
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
    default-server inter ${HAPROXY_CHECK_INTERVAL} fall 3 rise 2 on-marked-down shutdown-sessions resolvers railway init-addr last,libc,none
${PRIMARY_SERVERS}

# Replica PostgreSQL (read-only)
frontend postgresql_replicas
    bind *:5433
    default_backend postgresql_replicas_backend

backend postgresql_replicas_backend
    balance roundrobin
    option httpchk GET /replica
    http-check expect status 200
    default-server inter ${HAPROXY_CHECK_INTERVAL} fall 3 rise 2 on-marked-down shutdown-sessions resolvers railway init-addr last,libc,none
${REPLICA_SERVERS}
EOF

echo "HAProxy config generated with nodes: ${POSTGRES_NODES}"
cat "$CONFIG_FILE"
echo ""
echo "Starting HAProxy..."

exec haproxy -f "$CONFIG_FILE"
