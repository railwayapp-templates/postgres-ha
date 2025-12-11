#!/bin/sh
# etcd bootstrap wrapper with peer-aware startup
#
# Problem: etcd nodes starting at different times fail to form cluster because
# discovery fails when peers aren't reachable. etcd writes to data dir immediately,
# then exits with "discovery failed". Data cleanup isn't enough - we need to wait
# for peers BEFORE starting etcd.
#
# Solution:
# 1. Parse ETCD_INITIAL_CLUSTER to find peer nodes
# 2. Wait for peers to be reachable before starting etcd
# 3. Clean stale data if bootstrap never completed
# 4. Retry with exponential backoff

DATA_DIR=${ETCD_DATA_DIR:-/etcd-data}
MAX_RETRIES=${ETCD_MAX_RETRIES:-60}
RETRY_DELAY=${ETCD_RETRY_DELAY:-5}
BOOTSTRAP_COMPLETE_MARKER="$DATA_DIR/.bootstrap_complete"
PEER_WAIT_TIMEOUT=${ETCD_PEER_WAIT_TIMEOUT:-300}
PEER_CHECK_INTERVAL=${ETCD_PEER_CHECK_INTERVAL:-5}

log() {
  echo "[$(date -Iseconds)] ENTRYPOINT: $1"
}

check_cluster_health() {
  etcdctl endpoint health --endpoints=http://127.0.0.1:2379 >/dev/null 2>&1
}

# Extract peer URLs from ETCD_INITIAL_CLUSTER
# Format: name1=http://host1:2380,name2=http://host2:2380,...
get_peer_hosts() {
  echo "$ETCD_INITIAL_CLUSTER" | tr ',' '\n' | while read -r entry; do
    # Skip our own entry
    node_name=$(echo "$entry" | cut -d'=' -f1)
    if [ "$node_name" = "$ETCD_NAME" ]; then
      continue
    fi
    # Extract host:port from URL (http://host:port -> host:port)
    url=$(echo "$entry" | cut -d'=' -f2)
    host_port=$(echo "$url" | sed 's|.*://||')
    echo "$host_port"
  done
}

# Check if a host:port is reachable
check_peer_reachable() {
  host_port=$1
  host=$(echo "$host_port" | cut -d':' -f1)
  port=$(echo "$host_port" | cut -d':' -f2)

  # Try to connect with nc, timeout after 2 seconds
  # Using /dev/tcp if nc not available
  if command -v nc >/dev/null 2>&1; then
    nc -z -w2 "$host" "$port" >/dev/null 2>&1
  else
    # Fallback: try to resolve DNS at minimum
    getent hosts "$host" >/dev/null 2>&1
  fi
}

# Wait for at least one peer to be reachable (for 3-node cluster, need 2 for quorum)
wait_for_peers() {
  if [ -z "$ETCD_INITIAL_CLUSTER" ]; then
    log "ETCD_INITIAL_CLUSTER not set, skipping peer wait"
    return 0
  fi

  log "Waiting for peer nodes to be reachable..."

  peers=$(get_peer_hosts)
  peer_count=$(echo "$peers" | grep -c . || echo 0)

  if [ "$peer_count" -eq 0 ]; then
    log "No peers found in ETCD_INITIAL_CLUSTER, starting as single node"
    return 0
  fi

  # For a 3-node cluster, we need at least 1 other peer for quorum of 2
  # For 5-node, need 2 others for quorum of 3
  needed=$((peer_count / 2))
  if [ "$needed" -lt 1 ]; then
    needed=1
  fi

  log "Found $peer_count peers, need $needed reachable for quorum"

  elapsed=0
  while [ $elapsed -lt $PEER_WAIT_TIMEOUT ]; do
    reachable=0
    for peer in $peers; do
      if check_peer_reachable "$peer"; then
        log "Peer $peer is reachable"
        reachable=$((reachable + 1))
      else
        log "Peer $peer not reachable yet"
      fi
    done

    if [ $reachable -ge $needed ]; then
      log "Sufficient peers reachable ($reachable/$needed needed), proceeding with etcd start"
      # Give peers a moment to fully initialize
      sleep 2
      return 0
    fi

    log "Waiting for peers... ($reachable/$needed reachable, ${elapsed}s/${PEER_WAIT_TIMEOUT}s)"
    sleep $PEER_CHECK_INTERVAL
    elapsed=$((elapsed + PEER_CHECK_INTERVAL))
  done

  log "WARNING: Peer wait timeout reached, attempting etcd start anyway"
  return 0
}

# Monitor etcd and mark bootstrap complete once healthy
monitor_and_mark_bootstrap() {
  while true; do
    sleep 5
    if check_cluster_health; then
      if [ ! -f "$BOOTSTRAP_COMPLETE_MARKER" ]; then
        echo "1" > "$BOOTSTRAP_COMPLETE_MARKER"
        log "Cluster healthy - bootstrap marked complete"
      fi
    fi
  done
}

# CRITICAL: Clean stale data on startup if bootstrap never completed
# This handles the case where etcd wrote data but cluster never formed
if [ -d "$DATA_DIR" ] && [ "$(ls -A "$DATA_DIR" 2>/dev/null)" ]; then
  if [ ! -f "$BOOTSTRAP_COMPLETE_MARKER" ]; then
    log "Found stale data from incomplete bootstrap - cleaning..."
    rm -rf "${DATA_DIR:?}"/*
    log "Data directory cleaned, starting fresh"
  else
    log "Found data with completed bootstrap marker - preserving"
  fi
fi

attempt=1
while [ $attempt -le $MAX_RETRIES ]; do
  log "Starting etcd (attempt $attempt/$MAX_RETRIES)..."

  # Wait for peers before starting etcd (only on fresh bootstrap)
  if [ ! -f "$BOOTSTRAP_COMPLETE_MARKER" ]; then
    wait_for_peers
  fi

  # Start health monitor in background
  monitor_and_mark_bootstrap &
  MONITOR_PID=$!

  /usr/local/bin/etcd
  EXIT_CODE=$?

  # Stop monitor
  kill $MONITOR_PID 2>/dev/null || true

  # Exit code 0 means clean shutdown (e.g., SIGTERM)
  if [ $EXIT_CODE -eq 0 ]; then
    log "etcd exited cleanly"
    exit 0
  fi

  log "etcd exited with code $EXIT_CODE"

  # Only clean data if bootstrap never completed
  if [ ! -f "$BOOTSTRAP_COMPLETE_MARKER" ]; then
    if [ -d "$DATA_DIR" ] && [ "$(ls -A "$DATA_DIR" 2>/dev/null)" ]; then
      log "Bootstrap incomplete - cleaning data directory..."
      rm -rf "${DATA_DIR:?}"/*
    fi
  else
    log "Bootstrap was complete - preserving data directory"
    # For post-bootstrap failures, just retry without cleaning
  fi

  attempt=$((attempt + 1))
  if [ $attempt -le $MAX_RETRIES ]; then
    log "Retrying in ${RETRY_DELAY}s..."
    sleep $RETRY_DELAY
  fi
done

log "Failed to start etcd after $MAX_RETRIES attempts"
exit 1
