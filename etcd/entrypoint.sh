#!/bin/sh
set -e

MAX_RETRIES=${ETCD_MAX_RETRIES:-60}
RETRY_DELAY=${ETCD_RETRY_DELAY:-5}
STABILIZE_TIME=${ETCD_STABILIZE_TIME:-30}

log() {
  echo "[$(date -Iseconds)] $1"
}

check_health() {
  etcdctl endpoint health --endpoints=http://127.0.0.1:2379 >/dev/null 2>&1
}

for i in $(seq 1 $MAX_RETRIES); do
  log "Attempt $i/$MAX_RETRIES: Starting etcd..."

  # Start etcd in background
  /usr/local/bin/etcd &
  ETCD_PID=$!

  # Wait for etcd to either stabilize or die
  log "Waiting ${STABILIZE_TIME}s for cluster formation..."
  sleep $STABILIZE_TIME

  # Check if process is still running
  if kill -0 $ETCD_PID 2>/dev/null; then
    # etcd is still running, check if cluster is healthy
    if check_health; then
      log "Cluster formed successfully!"
      # Stay attached to the etcd process
      wait $ETCD_PID
      EXIT_CODE=$?
      log "etcd exited with code $EXIT_CODE"
      exit $EXIT_CODE
    else
      log "etcd running but cluster not healthy yet"
    fi
  else
    log "etcd process died"
  fi

  # Kill if still running but unhealthy
  if kill -0 $ETCD_PID 2>/dev/null; then
    log "Stopping unhealthy etcd..."
    kill $ETCD_PID 2>/dev/null || true
    wait $ETCD_PID 2>/dev/null || true
  fi

  if [ $i -lt $MAX_RETRIES ]; then
    log "Discovery failed, retrying in ${RETRY_DELAY}s..."
    sleep $RETRY_DELAY
  fi
done

log "Failed to form cluster after $MAX_RETRIES attempts"
exit 1
