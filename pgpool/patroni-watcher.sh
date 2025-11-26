#!/bin/bash
# Patroni Watcher - Monitors Patroni cluster state and updates Pgpool-II backends
# Polls Patroni API and uses PCP to detach/attach backends based on leader changes

set -euo pipefail

PATRONI_BACKENDS=(
    "postgres-1:postgres-1.railway.internal:5432:0"
    "postgres-2:postgres-2.railway.internal:5432:1"
    "postgres-3:postgres-3.railway.internal:5432:2"
)

PCP_HOST="localhost"
PCP_PORT=9898
PCP_USER="${PGPOOL_ADMIN_USERNAME:-admin}"
PCP_PASSWORD="${PGPOOL_ADMIN_PASSWORD}"
POLL_INTERVAL=2

log() {
    echo "[patroni-watcher] $*"
}

run_pcp_command() {
    local cmd="$1"
    shift

    PCPPASSWORD="$PCP_PASSWORD" "$cmd" \
        -h "$PCP_HOST" \
        -p "$PCP_PORT" \
        -U "$PCP_USER" \
        -w \
        "$@" 2>&1 || true
}

get_patroni_role() {
    local host="$1"
    local role

    role=$(curl -sf "http://${host}:8008/patroni" 2>/dev/null | jq -r '.role // empty' 2>/dev/null || echo "")
    echo "$role"
}

get_cluster_leader() {
    local name host port index role

    for backend in "${PATRONI_BACKENDS[@]}"; do
        IFS=':' read -r name host port index <<< "$backend"
        role=$(get_patroni_role "$host")

        if [ "$role" = "master" ]; then
            echo "$name"
            return 0
        fi
    done

    echo ""
}

detach_backend() {
    local index="$1"
    log "Detaching backend $index"
    run_pcp_command pcp_detach_node -n "$index" -g
}

attach_backend() {
    local index="$1"
    log "Attaching backend $index"
    run_pcp_command pcp_attach_node -n "$index" -g
}

sync_pgpool_with_patroni() {
    local current_leader="$1"
    local name host port index

    log "Syncing pgpool: leader is $current_leader"

    for backend in "${PATRONI_BACKENDS[@]}"; do
        IFS=':' read -r name host port index <<< "$backend"

        if [ "$name" = "$current_leader" ]; then
            attach_backend "$index"
        else
            detach_backend "$index"
        fi
    done
}

main() {
    log "Starting Patroni watcher"
    log "Monitoring backends: ${PATRONI_BACKENDS[*]}"
    log "Poll interval: ${POLL_INTERVAL}s"

    if [ -z "$PCP_PASSWORD" ]; then
        log "ERROR: PGPOOL_ADMIN_PASSWORD not set"
        exit 1
    fi

    last_leader=""

    while true; do
        current_leader=$(get_cluster_leader)

        if [ -z "$current_leader" ]; then
            log "WARNING: No leader found in cluster"
        elif [ "$current_leader" != "$last_leader" ]; then
            log "Leader change detected: ${last_leader:-none} -> $current_leader"
            sync_pgpool_with_patroni "$current_leader"
            last_leader="$current_leader"
        fi

        sleep "$POLL_INTERVAL"
    done
}

main
