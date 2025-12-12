#!/bin/sh
# etcd bootstrap wrapper with leader-based startup and learner mode
#
# Problem: etcd nodes starting at different times fail to form cluster because
# all nodes waiting for each other on TCP creates a deadlock. etcd also has
# hard timeouts that corrupt local state if quorum isn't reached. Additionally,
# adding voting members directly can disrupt quorum during cluster formation.
#
# Solution: Single-node bootstrap with learner mode (etcd v3.4+)
# 1. Determine bootstrap leader (alphabetically first node name)
# 2. Leader bootstraps single-node cluster (instant quorum)
# 3. Other nodes wait for leader, add themselves as LEARNERS (non-voting)
# 4. Learners sync data, then auto-promote to voting members once healthy
#
# Recovery mode (leader volume loss):
# - Before bootstrapping, leader checks if other peers have a healthy cluster
# - If yes: leader removes its stale entry and joins as learner (not bootstrap)
# - This prevents split-brain when leader loses volume but cluster still exists
#
# Benefits of learner mode:
# - Learners don't affect quorum calculations
# - Safe to remove if something goes wrong
# - No risk of disrupting leader elections during join
# - Promotion only happens after data is synced

DATA_DIR=${ETCD_DATA_DIR:-/var/lib/etcd}
MAX_RETRIES=${ETCD_MAX_RETRIES:-60}
RETRY_DELAY=${ETCD_RETRY_DELAY:-5}
BOOTSTRAP_COMPLETE_MARKER="$DATA_DIR/.bootstrap_complete"
PEER_WAIT_TIMEOUT=${ETCD_PEER_WAIT_TIMEOUT:-300}
PEER_CHECK_INTERVAL=${ETCD_PEER_CHECK_INTERVAL:-5}

log() {
  echo "[$(date -Iseconds)] ENTRYPOINT: $1" >&2
}

check_cluster_health() {
  etcdctl endpoint health --endpoints=http://127.0.0.1:2379 >/dev/null 2>&1
}

# Get bootstrap leader (alphabetically first node name)
get_bootstrap_leader() {
  echo "$ETCD_INITIAL_CLUSTER" | tr ',' '\n' | cut -d'=' -f1 | sort | head -1
}

# Get leader's client endpoint (port 2379)
get_leader_endpoint() {
  leader=$1
  # Extract leader's URL from ETCD_INITIAL_CLUSTER and convert peer port to client port
  entry=$(echo "$ETCD_INITIAL_CLUSTER" | tr ',' '\n' | grep "^${leader}=")
  url=$(echo "$entry" | cut -d'=' -f2)
  # Convert http://host:2380 to http://host:2379
  echo "$url" | sed 's/:2380/:2379/'
}

# Get leader's peer host:port for TCP check
get_leader_peer_host() {
  leader=$1
  entry=$(echo "$ETCD_INITIAL_CLUSTER" | tr ',' '\n' | grep "^${leader}=")
  url=$(echo "$entry" | cut -d'=' -f2)
  echo "$url" | sed 's|.*://||'
}

# Get my peer URL from ETCD_INITIAL_CLUSTER
get_my_peer_url() {
  entry=$(echo "$ETCD_INITIAL_CLUSTER" | tr ',' '\n' | grep "^${ETCD_NAME}=")
  echo "$entry" | cut -d'=' -f2
}

# Check if any other peer has a healthy cluster (for recovery detection)
# Returns 0 if a healthy peer is found, 1 otherwise
# On success, outputs the healthy peer's client endpoint
check_existing_cluster() {
  log "Checking if other peers have an existing cluster..."

  # Try each peer (except ourselves)
  echo "$ETCD_INITIAL_CLUSTER" | tr ',' '\n' | while read -r entry; do
    peer_name=$(echo "$entry" | cut -d'=' -f1)

    # Skip ourselves
    if [ "$peer_name" = "$ETCD_NAME" ]; then
      continue
    fi

    peer_url=$(echo "$entry" | cut -d'=' -f2)
    # Convert peer URL (2380) to client URL (2379)
    client_endpoint=$(echo "$peer_url" | sed 's/:2380/:2379/')

    log "Checking peer $peer_name at $client_endpoint..."

    if etcdctl endpoint health --endpoints="$client_endpoint" >/dev/null 2>&1; then
      log "Found healthy cluster at peer $peer_name"
      echo "$client_endpoint"
      return 0
    fi
  done

  return 1
}

# Remove stale member entry for this node (used during recovery)
remove_stale_self() {
  endpoint=$1

  log "Checking for stale member entry to remove..."

  # Get member ID for our name (if exists)
  member_id=$(etcdctl member list --endpoints="$endpoint" -w simple 2>/dev/null | while read -r line; do
    name=$(echo "$line" | cut -d',' -f3 | tr -d ' ')
    if [ "$name" = "$ETCD_NAME" ]; then
      echo "$line" | cut -d',' -f1 | tr -d ' '
      return
    fi
  done)

  if [ -n "$member_id" ]; then
    log "Found stale member entry (ID: $member_id), removing..."
    if etcdctl member remove "$member_id" --endpoints="$endpoint" 2>&1; then
      log "Successfully removed stale member entry"
      return 0
    else
      log "Failed to remove stale member entry"
      return 1
    fi
  else
    log "No stale member entry found"
    return 0
  fi
}

# Wait for leader to be healthy and accepting connections
wait_for_leader() {
  leader=$1
  endpoint=$(get_leader_endpoint "$leader")
  host_port=$(get_leader_peer_host "$leader")
  host=$(echo "$host_port" | cut -d':' -f1)
  port=$(echo "$host_port" | cut -d':' -f2)

  log "Waiting for bootstrap leader $leader at $endpoint..."

  elapsed=0
  while [ $elapsed -lt $PEER_WAIT_TIMEOUT ]; do
    # First check TCP connectivity
    if command -v nc >/dev/null 2>&1; then
      if nc -z -w2 "$host" "$port" >/dev/null 2>&1; then
        # Then check if cluster is actually healthy
        if etcdctl endpoint health --endpoints="$endpoint" >/dev/null 2>&1; then
          log "Bootstrap leader $leader is healthy"
          return 0
        else
          log "Leader $leader reachable but not healthy yet..."
        fi
      else
        log "Leader $leader not reachable yet (${elapsed}s/${PEER_WAIT_TIMEOUT}s)"
      fi
    else
      # Fallback without nc
      if etcdctl endpoint health --endpoints="$endpoint" >/dev/null 2>&1; then
        log "Bootstrap leader $leader is healthy"
        return 0
      else
        log "Leader $leader not healthy yet (${elapsed}s/${PEER_WAIT_TIMEOUT}s)"
      fi
    fi

    sleep $PEER_CHECK_INTERVAL
    elapsed=$((elapsed + PEER_CHECK_INTERVAL))
  done

  log "ERROR: Timeout waiting for bootstrap leader $leader"
  return 1
}

# Add this node to an existing cluster as a learner (non-voting member)
# On success, outputs the ETCD_INITIAL_CLUSTER value to use
add_self_to_cluster() {
  leader=$1
  endpoint=$(get_leader_endpoint "$leader")
  my_peer_url=$(get_my_peer_url)

  log "Adding self ($ETCD_NAME) as learner to cluster via $endpoint..."

  # Check if already a member (in case of restart)
  if etcdctl member list --endpoints="$endpoint" 2>/dev/null | grep -q "$ETCD_NAME"; then
    log "Already a member of the cluster"
    # Build cluster string from member list for existing member
    get_current_cluster "$leader"
    return 0
  fi

  # Add as learner (non-voting) member - safer than direct voting member add
  # Learner mode (v3.4+) prevents new members from disrupting quorum
  output=$(etcdctl member add "$ETCD_NAME" --learner --peer-urls="$my_peer_url" --endpoints="$endpoint" 2>&1)
  result=$?

  if [ $result -eq 0 ]; then
    log "Successfully added as learner to cluster"
    # Extract ETCD_INITIAL_CLUSTER from member add output
    # Output format: ETCD_INITIAL_CLUSTER="name1=url1,name2=url2"
    cluster=$(echo "$output" | grep 'ETCD_INITIAL_CLUSTER=' | head -1 | sed 's/.*ETCD_INITIAL_CLUSTER="//' | sed 's/"$//')
    if [ -n "$cluster" ]; then
      echo "$cluster"
    else
      # Fallback to building from member list
      log "Could not extract cluster from member add output, using member list"
      get_current_cluster "$leader"
    fi
    return 0
  else
    log "Failed to add self as learner to cluster: $output"
    return 1
  fi
}

# Build current cluster membership for joining node
get_current_cluster() {
  leader=$1
  endpoint=$(get_leader_endpoint "$leader")
  my_peer_url=$(get_my_peer_url)

  # Get existing members
  existing=$(etcdctl member list --endpoints="$endpoint" -w simple 2>/dev/null | while read -r line; do
    # Format: id, status, name, peer_urls, client_urls
    name=$(echo "$line" | cut -d',' -f3 | tr -d ' ')
    peer_url=$(echo "$line" | cut -d',' -f4 | tr -d ' ')
    if [ -n "$name" ] && [ -n "$peer_url" ]; then
      echo "${name}=${peer_url}"
    fi
  done | tr '\n' ',' | sed 's/,$//')

  # Add ourselves if not in list
  if ! echo "$existing" | grep -q "$ETCD_NAME="; then
    if [ -n "$existing" ]; then
      existing="${existing},${ETCD_NAME}=${my_peer_url}"
    else
      existing="${ETCD_NAME}=${my_peer_url}"
    fi
  fi

  echo "$existing"
}

# Get my member ID from etcd cluster
get_my_member_id() {
  endpoint=$1
  # Member list format: id, status, name, peer_urls, client_urls, is_learner
  etcdctl member list --endpoints="$endpoint" -w simple 2>/dev/null | while read -r line; do
    name=$(echo "$line" | cut -d',' -f3 | tr -d ' ')
    if [ "$name" = "$ETCD_NAME" ]; then
      echo "$line" | cut -d',' -f1 | tr -d ' '
      return
    fi
  done
}

# Check if this member is a learner
is_learner() {
  endpoint=$1
  member_info=$(etcdctl member list --endpoints="$endpoint" -w simple 2>/dev/null | grep "$ETCD_NAME")
  # The is_learner field is the last field, "true" or "false"
  echo "$member_info" | grep -q "true$"
}

# Promote self from learner to voting member
promote_self() {
  endpoint=$1
  member_id=$(get_my_member_id "$endpoint")

  if [ -z "$member_id" ]; then
    log "ERROR: Could not find my member ID for promotion"
    return 1
  fi

  # Check if we're actually a learner
  if ! is_learner "$endpoint"; then
    log "Already a voting member, no promotion needed"
    return 0
  fi

  log "Promoting self (ID: $member_id) from learner to voting member..."

  output=$(etcdctl member promote "$member_id" --endpoints="$endpoint" 2>&1)
  result=$?

  if [ $result -eq 0 ]; then
    log "Successfully promoted to voting member"
    return 0
  else
    # Check if error is "not a learner" which means we're already promoted
    if echo "$output" | grep -q "is not a learner"; then
      log "Already a voting member"
      return 0
    fi
    log "Failed to promote: $output"
    return 1
  fi
}

# Monitor etcd, promote learner if needed, and mark bootstrap complete once healthy
# Args: $1 = "learner" if this node joined as a learner
monitor_and_mark_bootstrap() {
  joined_as_learner=$1
  promoted=false

  while true; do
    sleep 5
    if check_cluster_health; then
      # If we joined as a learner, try to promote ourselves
      if [ "$joined_as_learner" = "learner" ] && [ "$promoted" = "false" ]; then
        log "Node healthy, attempting promotion from learner..."
        if promote_self "http://127.0.0.1:2379"; then
          promoted=true
          log "Learner promotion successful"
        else
          log "Learner promotion failed, will retry..."
        fi
      fi

      # Mark bootstrap complete only after promotion (if applicable)
      if [ ! -f "$BOOTSTRAP_COMPLETE_MARKER" ]; then
        if [ "$joined_as_learner" != "learner" ] || [ "$promoted" = "true" ]; then
          echo "1" > "$BOOTSTRAP_COMPLETE_MARKER"
          log "Cluster healthy and fully joined - bootstrap marked complete"
        fi
      fi
    fi
  done
}

# CRITICAL: Clean stale data on startup if bootstrap never completed
if [ -d "$DATA_DIR" ] && [ "$(ls -A "$DATA_DIR" 2>/dev/null)" ]; then
  if [ ! -f "$BOOTSTRAP_COMPLETE_MARKER" ]; then
    log "Found stale data from incomplete bootstrap - cleaning..."
    rm -rf "${DATA_DIR:?}"/*
    log "Data directory cleaned, starting fresh"
  else
    log "Found data with completed bootstrap marker - preserving"
  fi
fi

# Determine our role
BOOTSTRAP_LEADER=$(get_bootstrap_leader)
IS_LEADER=false
if [ "$ETCD_NAME" = "$BOOTSTRAP_LEADER" ]; then
  IS_LEADER=true
fi

log "Bootstrap leader is: $BOOTSTRAP_LEADER (I am $ETCD_NAME, is_leader=$IS_LEADER)"

attempt=1
while [ $attempt -le $MAX_RETRIES ]; do
  log "Starting etcd (attempt $attempt/$MAX_RETRIES)..."

  JOINED_AS_LEARNER=""

  if [ "$IS_LEADER" = "true" ]; then
    # Bootstrap leader: start single-node cluster if fresh, or normal start if already bootstrapped
    if [ ! -f "$BOOTSTRAP_COMPLETE_MARKER" ]; then
      # RECOVERY CHECK: Before bootstrapping, check if other peers already have a cluster
      # This prevents split-brain if the leader lost its volume but the cluster still exists
      EXISTING_CLUSTER_ENDPOINT=$(check_existing_cluster)
      if [ -n "$EXISTING_CLUSTER_ENDPOINT" ]; then
        log "RECOVERY MODE: Found existing cluster, joining instead of bootstrapping"

        # Remove our stale entry if it exists
        remove_stale_self "$EXISTING_CLUSTER_ENDPOINT"

        # Join as learner like a non-leader would
        my_peer_url=$(get_my_peer_url)
        log "Adding self as learner to existing cluster via $EXISTING_CLUSTER_ENDPOINT..."

        output=$(etcdctl member add "$ETCD_NAME" --learner --peer-urls="$my_peer_url" --endpoints="$EXISTING_CLUSTER_ENDPOINT" 2>&1)
        if [ $? -eq 0 ]; then
          log "Successfully added as learner for recovery"
          CURRENT_CLUSTER=$(echo "$output" | grep 'ETCD_INITIAL_CLUSTER=' | head -1 | sed 's/.*ETCD_INITIAL_CLUSTER="//' | sed 's/"$//')
          if [ -z "$CURRENT_CLUSTER" ]; then
            # Fallback: build from member list
            CURRENT_CLUSTER=$(etcdctl member list --endpoints="$EXISTING_CLUSTER_ENDPOINT" -w simple 2>/dev/null | while read -r line; do
              name=$(echo "$line" | cut -d',' -f3 | tr -d ' ')
              peer_url=$(echo "$line" | cut -d',' -f4 | tr -d ' ')
              if [ -n "$name" ] && [ -n "$peer_url" ]; then
                echo "${name}=${peer_url}"
              fi
            done | tr '\n' ',' | sed 's/,$//')
          fi

          log "Joining existing cluster as learner (recovery): $CURRENT_CLUSTER"
          JOINED_AS_LEARNER="learner"

          export ETCD_INITIAL_CLUSTER="$CURRENT_CLUSTER"
          export ETCD_INITIAL_CLUSTER_STATE="existing"

          # Start health monitor for learner promotion
          monitor_and_mark_bootstrap "learner" &
          MONITOR_PID=$!

          /usr/local/bin/etcd --auto-compaction-retention=1 --max-learners=2
          EXIT_CODE=$?

          # Skip to end of loop for retry handling
          # (need to break out of the if-else chain)
          RECOVERY_MODE=true
        else
          log "Failed to add as learner during recovery: $output"
          log "Will retry..."
          attempt=$((attempt + 1))
          sleep $RETRY_DELAY
          continue
        fi
      else
        # Normal bootstrap: no existing cluster found
        MY_PEER_URL=$(get_my_peer_url)
        log "Bootstrapping as single-node cluster: ${ETCD_NAME}=${MY_PEER_URL}"

        # Override cluster config for single-node bootstrap
        export ETCD_INITIAL_CLUSTER="${ETCD_NAME}=${MY_PEER_URL}"
        export ETCD_INITIAL_CLUSTER_STATE="new"

        # Start health monitor in background (leader is not a learner)
        monitor_and_mark_bootstrap "" &
        MONITOR_PID=$!

        /usr/local/bin/etcd --auto-compaction-retention=1 --max-learners=2
        EXIT_CODE=$?
      fi
    else
      # Already bootstrapped, just start normally
      monitor_and_mark_bootstrap "" &
      MONITOR_PID=$!

      /usr/local/bin/etcd --auto-compaction-retention=1 --max-learners=2
      EXIT_CODE=$?
    fi
  else
    # Non-leader: wait for leader, join existing cluster as learner
    if [ ! -f "$BOOTSTRAP_COMPLETE_MARKER" ]; then
      if ! wait_for_leader "$BOOTSTRAP_LEADER"; then
        log "Failed to reach bootstrap leader, retrying..."
        attempt=$((attempt + 1))
        sleep $RETRY_DELAY
        continue
      fi

      # add_self_to_cluster adds as learner and outputs the ETCD_INITIAL_CLUSTER value
      CURRENT_CLUSTER=$(add_self_to_cluster "$BOOTSTRAP_LEADER")
      if [ $? -ne 0 ] || [ -z "$CURRENT_CLUSTER" ]; then
        log "Failed to add self as learner to cluster, retrying..."
        attempt=$((attempt + 1))
        sleep $RETRY_DELAY
        continue
      fi

      log "Joining existing cluster as learner: $CURRENT_CLUSTER"
      JOINED_AS_LEARNER="learner"

      export ETCD_INITIAL_CLUSTER="$CURRENT_CLUSTER"
      export ETCD_INITIAL_CLUSTER_STATE="existing"
    fi

    # Start health monitor in background (will handle learner promotion)
    monitor_and_mark_bootstrap "$JOINED_AS_LEARNER" &
    MONITOR_PID=$!

    /usr/local/bin/etcd --auto-compaction-retention=1 --max-learners=2
    EXIT_CODE=$?
  fi

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
  fi

  attempt=$((attempt + 1))
  if [ $attempt -le $MAX_RETRIES ]; then
    log "Retrying in ${RETRY_DELAY}s..."
    sleep $RETRY_DELAY
  fi
done

log "Failed to start etcd after $MAX_RETRIES attempts"
exit 1
