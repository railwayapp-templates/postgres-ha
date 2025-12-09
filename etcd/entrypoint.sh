#!/bin/sh
# etcd bootstrap wrapper
#
# Problem: etcd writes to data directory immediately on startup, before cluster forms.
# If peers aren't reachable, etcd exits with "discovery failed" but data is already
# written. On restart, etcd sees "already bootstrapped" data but cluster never formed.
#
# Solution: If etcd exits during bootstrap (before cluster formed), clean data dir and retry.
# Once cluster has formed successfully, preserve data on restarts.
# This follows the official etcd recommendation.

DATA_DIR=${ETCD_DATA_DIR:-/etcd-data}
MAX_RETRIES=${ETCD_MAX_RETRIES:-60}
RETRY_DELAY=${ETCD_RETRY_DELAY:-5}
BOOTSTRAP_COMPLETE_MARKER="$DATA_DIR/.bootstrap_complete"

log() {
  echo "[$(date -Iseconds)] ENTRYPOINT: $1"
}

check_cluster_health() {
  etcdctl endpoint health --endpoints=http://127.0.0.1:2379 >/dev/null 2>&1
}

# Monitor etcd and mark bootstrap complete once healthy
monitor_and_mark_bootstrap() {
  while true; do
    sleep 5
    if check_cluster_health; then
      if [ ! -f "$BOOTSTRAP_COMPLETE_MARKER" ]; then
        touch "$BOOTSTRAP_COMPLETE_MARKER"
        log "Cluster healthy - bootstrap marked complete"
      fi
    fi
  done
}

attempt=1
while [ $attempt -le $MAX_RETRIES ]; do
  log "Starting etcd (attempt $attempt/$MAX_RETRIES)..."

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
