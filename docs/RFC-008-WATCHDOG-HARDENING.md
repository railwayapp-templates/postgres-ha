# RFC-008: Watchdog Hardening for Split-Brain Prevention

## Status
Proposed

## Summary

This RFC proposes improvements to the application-level watchdog implemented in RFC-007. While the current implementation provides reasonable split-brain protection, several risks and edge cases have been identified that could lead to data corruption in production environments. This document analyzes these risks and proposes concrete mitigations.

## Background

RFC-007 implemented a supervisord-based watchdog that monitors Patroni's REST API and triggers container restart when Patroni becomes unresponsive. The current implementation provides a ~15-25 second detection-to-restart window.

### Current Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     CURRENT ARCHITECTURE                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  supervisord (PID 1)                                            │
│       │                                                         │
│       ├──► patroni-runner.sh ──► Patroni ──► PostgreSQL        │
│       │                                                         │
│       ├──► patroni-watchdog.sh                                  │
│       │         │                                               │
│       │         └──► curl /health (every 5s)                   │
│       │         └──► 3 failures → pg_ctl stop → exit(1)        │
│       │                                                         │
│       └──► supervisor-exit-handler.sh                           │
│                 └──► watchdog exits → kill supervisord          │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Current Timing

```
CHECK_INTERVAL    = 5 seconds
FAILURE_THRESHOLD = 3 checks
TTL               = 20 seconds

Detection time:     15 seconds (5s × 3)
pg_ctl stop:        5-10 seconds
Container restart:  2-5 seconds
────────────────────────────────
Total:              22-30 seconds
```

## Problem Statement

The current implementation has several risks that could lead to split-brain scenarios or unnecessary failovers:

### Risk 1: Timing Window Exceeds TTL

The worst-case detection-to-shutdown time (30s) exceeds the DCS TTL (20s), creating a window where two primaries can coexist:

```
T+0s    Patroni crashes
T+5s    Watchdog check 1 fails
T+10s   Watchdog check 2 fails
T+15s   Watchdog check 3 fails, pg_ctl starts
T+20s   ⚠️  Leader key expires, new leader elected
T+25s   ⚠️  SPLIT-BRAIN: Old PostgreSQL still stopping
T+30s   Old PostgreSQL finally stops
```

### Risk 2: curl Hangs on Unresponsive Patroni

The current health check has no timeout:

```bash
# Current implementation
if curl -sf "$PATRONI_API" > /dev/null 2>&1; then
```

If Patroni enters a hung state (accepting TCP connections but not responding), curl blocks indefinitely, and the watchdog fails to detect the problem.

### Risk 3: PostgreSQL Stop Failure

The current implementation swallows pg_ctl errors:

```bash
# Current implementation
pg_ctl -D "$PGDATA" stop -m fast -w -t 10 2>/dev/null || true
pg_ctl -D "$PGDATA" stop -m immediate -w -t 5 2>/dev/null || true
# Script exits, but PostgreSQL might still be running!
```

Scenarios where pg_ctl fails:
- Active long-running transactions
- Stuck I/O operations
- Corrupted postmaster.pid
- PostgreSQL in crash recovery

### Risk 4: Network Partition False Negative

The `/health` endpoint only verifies Patroni is running, not that it can communicate with the DCS:

```
Scenario:
1. Network partition isolates node from etcd
2. Patroni running, /health returns 200
3. Leader key expires (Patroni can't renew)
4. New leader elected elsewhere
5. Watchdog sees "healthy" - no action taken
6. SPLIT-BRAIN: Two primaries accepting writes
```

### Risk 5: Supervisor Race Condition

```ini
# Current configuration
[program:watchdog]
autorestart=true
startretries=999999
```

When watchdog exits, supervisor may restart it before the exit-handler processes the event, leading to a restart loop instead of container termination.

### Risk 6: Watchdog Script Crash

```bash
set -e  # Exit on any error
```

An unexpected error in the watchdog script (e.g., `jq` parse error, missing file) causes immediate exit without stopping PostgreSQL.

## Proposed Solution

### Overview

Implement a defense-in-depth approach with multiple layers of protection:

```
┌─────────────────────────────────────────────────────────────────┐
│                     HARDENED ARCHITECTURE                       │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Layer 1: Aggressive Health Checks                              │
│  ─────────────────────────────────                              │
│  - 2-3 second intervals                                         │
│  - Strict curl timeouts                                         │
│  - Check /patroni not just /health                             │
│                                                                 │
│  Layer 2: DCS-Aware Monitoring                                  │
│  ────────────────────────────                                   │
│  - Verify leader lock ownership                                 │
│  - Detect network partitions                                    │
│  - Monitor replication lag                                      │
│                                                                 │
│  Layer 3: Guaranteed PostgreSQL Termination                     │
│  ──────────────────────────────────────────                     │
│  - Connection fencing before stop                               │
│  - Escalating stop modes                                        │
│  - SIGKILL fallback with verification                          │
│                                                                 │
│  Layer 4: Robust Process Management                             │
│  ──────────────────────────────────                             │
│  - Proper supervisor exit handling                              │
│  - Heartbeat file for watchdog health                          │
│  - Defensive error handling                                     │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Timing Improvement

```
PROPOSED TIMING
───────────────
CHECK_INTERVAL    = 3 seconds (was 5)
FAILURE_THRESHOLD = 2 checks (was 3)
CURL_TIMEOUT      = 2 seconds (was unlimited)

Detection time:     6 seconds (3s × 2)
Fencing:            1 second
pg_ctl fast:        3 seconds (reduced timeout)
SIGKILL fallback:   2 seconds
Container restart:  2 seconds
────────────────────────────────
Total (worst):      14 seconds

TTL:                20 seconds
Safety margin:      6 seconds ✓
```

## Detailed Design

### Component 1: Enhanced Health Check

Replace simple `/health` check with comprehensive Patroni state verification:

```bash
#!/bin/bash
# check_patroni_health()
#
# Returns:
#   0 - Healthy
#   1 - Unhealthy (transient, may recover)
#   2 - Critical (must stop PostgreSQL immediately)

check_patroni_health() {
    local response http_code

    # Fetch full Patroni state with strict timeout
    response=$(curl -sf \
        --max-time "$CURL_TIMEOUT" \
        --connect-timeout "$CURL_CONNECT_TIMEOUT" \
        "$PATRONI_API_FULL" 2>/dev/null)
    http_code=$?

    # Check 1: Patroni responding?
    if [ $http_code -ne 0 ]; then
        log "Patroni not responding (curl exit: $http_code)"
        return 1
    fi

    # Check 2: Parse state
    local state role timeline pause
    state=$(echo "$response" | jq -r '.state // "unknown"')
    role=$(echo "$response" | jq -r '.role // "unknown"')
    timeline=$(echo "$response" | jq -r '.timeline // 0')
    pause=$(echo "$response" | jq -r '.pause // false')

    # Check 3: Patroni in valid state?
    if [ "$state" != "running" ]; then
        log "Patroni state is '$state', expected 'running'"
        return 1
    fi

    # Check 4: If we're primary, verify lock ownership
    if [ "$role" = "master" ] || [ "$role" = "primary" ]; then
        if ! verify_leader_lock; then
            log "CRITICAL: Primary without leader lock!"
            return 2
        fi
    fi

    # Check 5: Verify DCS connectivity (detect network partition)
    if ! check_dcs_connectivity "$response"; then
        log "CRITICAL: Cannot verify DCS connectivity"
        return 2
    fi

    return 0
}

verify_leader_lock() {
    local cluster_info leader

    cluster_info=$(curl -sf \
        --max-time "$CURL_TIMEOUT" \
        "http://localhost:8008/cluster" 2>/dev/null) || return 1

    leader=$(echo "$cluster_info" | jq -r '.leader // ""')

    if [ "$leader" != "$PATRONI_NAME" ]; then
        log "Leader lock held by '$leader', not us ('$PATRONI_NAME')"
        return 1
    fi

    return 0
}

check_dcs_connectivity() {
    local response="$1"
    local dcs_last_seen

    # Check when Patroni last successfully contacted DCS
    # This is available in the /patroni response
    dcs_last_seen=$(echo "$response" | jq -r '.dcs_last_seen // 0')

    if [ "$dcs_last_seen" -eq 0 ]; then
        # Field not available, fall back to basic check
        return 0
    fi

    local now dcs_age
    now=$(date +%s)
    dcs_age=$((now - dcs_last_seen))

    # If DCS contact is older than TTL, we're partitioned
    if [ "$dcs_age" -gt "$DCS_STALE_THRESHOLD" ]; then
        log "DCS contact stale: ${dcs_age}s ago (threshold: ${DCS_STALE_THRESHOLD}s)"
        return 1
    fi

    return 0
}
```

### Component 2: Guaranteed PostgreSQL Termination

Implement escalating stop mechanism with verification:

```bash
#!/bin/bash
# stop_postgresql()
#
# Guaranteed PostgreSQL termination with escalating force levels.
# Returns only after PostgreSQL is confirmed stopped.

stop_postgresql() {
    local max_attempts=3
    local attempt=0

    # Phase 1: Fence - prevent new connections and terminate existing
    log "Phase 1: Fencing PostgreSQL"
    fence_postgresql

    # Phase 2: Graceful stop (fast mode)
    log "Phase 2: Graceful shutdown (fast mode)"
    if pg_ctl -D "$PGDATA" stop -m fast -w -t 5 2>/dev/null; then
        if verify_postgresql_stopped; then
            log "PostgreSQL stopped gracefully"
            return 0
        fi
    fi

    # Phase 3: Immediate stop
    log "Phase 3: Immediate shutdown"
    if pg_ctl -D "$PGDATA" stop -m immediate -w -t 3 2>/dev/null; then
        if verify_postgresql_stopped; then
            log "PostgreSQL stopped (immediate mode)"
            return 0
        fi
    fi

    # Phase 4: SIGTERM to postmaster
    log "Phase 4: SIGTERM to postmaster"
    local pg_pid
    if [ -f "$PGDATA/postmaster.pid" ]; then
        pg_pid=$(head -1 "$PGDATA/postmaster.pid" 2>/dev/null)
        if [ -n "$pg_pid" ]; then
            kill -TERM "$pg_pid" 2>/dev/null || true
            sleep 2
            if verify_postgresql_stopped; then
                log "PostgreSQL stopped (SIGTERM)"
                return 0
            fi
        fi
    fi

    # Phase 5: SIGKILL - nuclear option
    log "Phase 5: SIGKILL to all postgres processes"
    if [ -n "$pg_pid" ]; then
        kill -KILL "$pg_pid" 2>/dev/null || true
    fi
    # Kill any remaining postgres processes
    pkill -9 -f "postgres: " 2>/dev/null || true
    pkill -9 -f "postgresql" 2>/dev/null || true

    sleep 1

    # Final verification
    if verify_postgresql_stopped; then
        log "PostgreSQL stopped (SIGKILL)"
        return 0
    fi

    # This should never happen
    log "FATAL: PostgreSQL still running after all stop attempts!"
    log "Manual intervention required"
    return 1
}

fence_postgresql() {
    # Terminate active connections to prevent new writes
    psql -U postgres -d postgres -c "
        SELECT pg_terminate_backend(pid)
        FROM pg_stat_activity
        WHERE pid <> pg_backend_pid()
          AND backend_type = 'client backend'
          AND datname IS NOT NULL;
    " 2>/dev/null || true

    # Brief pause to allow terminations to complete
    sleep 0.5
}

verify_postgresql_stopped() {
    # Method 1: pg_isready
    if pg_isready -q -t 1 2>/dev/null; then
        return 1  # Still running
    fi

    # Method 2: Check postmaster.pid
    if [ -f "$PGDATA/postmaster.pid" ]; then
        local pg_pid
        pg_pid=$(head -1 "$PGDATA/postmaster.pid" 2>/dev/null)
        if [ -n "$pg_pid" ] && kill -0 "$pg_pid" 2>/dev/null; then
            return 1  # Process still exists
        fi
    fi

    # Method 3: Check for any postgres processes
    if pgrep -f "postgres: " >/dev/null 2>&1; then
        return 1  # Postgres processes still running
    fi

    return 0  # Confirmed stopped
}
```

### Component 3: Improved Watchdog Script

Complete rewrite with defensive programming:

```bash
#!/bin/bash
# patroni-watchdog.sh - Hardened application-level watchdog for Patroni
#
# Implements RFC-008: Watchdog Hardening for Split-Brain Prevention

#==============================================================================
# Configuration
#==============================================================================

PATRONI_API_HEALTH="${PATRONI_API_URL:-http://localhost:8008/health}"
PATRONI_API_FULL="${PATRONI_API_FULL_URL:-http://localhost:8008/patroni}"
PATRONI_API_CLUSTER="${PATRONI_API_CLUSTER_URL:-http://localhost:8008/cluster}"
PATRONI_NAME="${PATRONI_NAME:-$(hostname)}"

CHECK_INTERVAL="${WATCHDOG_CHECK_INTERVAL:-3}"
FAILURE_THRESHOLD="${WATCHDOG_FAILURE_THRESHOLD:-2}"
CRITICAL_THRESHOLD="${WATCHDOG_CRITICAL_THRESHOLD:-1}"

CURL_TIMEOUT="${WATCHDOG_CURL_TIMEOUT:-2}"
CURL_CONNECT_TIMEOUT="${WATCHDOG_CURL_CONNECT_TIMEOUT:-1}"

DCS_STALE_THRESHOLD="${WATCHDOG_DCS_STALE_THRESHOLD:-15}"

PGDATA="${PGDATA:-/var/lib/postgresql/data}"
HEARTBEAT_FILE="${WATCHDOG_HEARTBEAT_FILE:-/tmp/patroni-watchdog-heartbeat}"

STARTUP_DELAY="${WATCHDOG_STARTUP_DELAY:-10}"

#==============================================================================
# State
#==============================================================================

failures=0
critical_failures=0
consecutive_successes=0
last_healthy_time=$(date +%s)

#==============================================================================
# Logging
#==============================================================================

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] WATCHDOG: $1"
}

log_error() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] WATCHDOG ERROR: $1" >&2
}

#==============================================================================
# Error Handling
#==============================================================================

# Don't use set -e globally - we need fine-grained error handling
# Instead, trap errors and handle them explicitly

cleanup() {
    log "Watchdog shutting down"
    rm -f "$HEARTBEAT_FILE" 2>/dev/null || true
}

trap cleanup EXIT

handle_fatal_error() {
    local error_msg="$1"
    log_error "Fatal error: $error_msg"
    log_error "Attempting PostgreSQL shutdown before exit"

    # Best-effort PostgreSQL stop
    stop_postgresql || true

    exit 2
}

#==============================================================================
# Health Check Functions
#==============================================================================

check_patroni_health() {
    local response http_code

    # Fetch full Patroni state with strict timeout
    if ! response=$(curl -sf \
        --max-time "$CURL_TIMEOUT" \
        --connect-timeout "$CURL_CONNECT_TIMEOUT" \
        "$PATRONI_API_FULL" 2>/dev/null); then
        log "Patroni API not responding"
        return 1
    fi

    # Validate response is valid JSON
    if ! echo "$response" | jq -e . >/dev/null 2>&1; then
        log "Patroni returned invalid JSON"
        return 1
    fi

    # Extract state fields
    local state role
    state=$(echo "$response" | jq -r '.state // "unknown"')
    role=$(echo "$response" | jq -r '.role // "unknown"')

    # Verify Patroni is running
    if [ "$state" != "running" ]; then
        log "Patroni state is '$state', expected 'running'"
        return 1
    fi

    # If primary, verify leader lock ownership
    if [ "$role" = "master" ] || [ "$role" = "primary" ]; then
        if ! verify_leader_lock; then
            log "CRITICAL: Primary role without leader lock ownership"
            return 2  # Critical failure
        fi
    fi

    return 0
}

verify_leader_lock() {
    local cluster_info leader

    if ! cluster_info=$(curl -sf \
        --max-time "$CURL_TIMEOUT" \
        --connect-timeout "$CURL_CONNECT_TIMEOUT" \
        "$PATRONI_API_CLUSTER" 2>/dev/null); then
        log "Cannot fetch cluster info"
        return 1
    fi

    leader=$(echo "$cluster_info" | jq -r '.leader // ""')

    if [ -z "$leader" ]; then
        log "No leader in cluster info"
        return 1
    fi

    if [ "$leader" != "$PATRONI_NAME" ]; then
        log "Leader is '$leader', not '$PATRONI_NAME'"
        return 1
    fi

    return 0
}

#==============================================================================
# PostgreSQL Management Functions
#==============================================================================

fence_postgresql() {
    log "Fencing PostgreSQL - terminating client connections"

    timeout 2 psql -U postgres -d postgres -c "
        SELECT pg_terminate_backend(pid)
        FROM pg_stat_activity
        WHERE pid <> pg_backend_pid()
          AND backend_type = 'client backend'
          AND datname IS NOT NULL;
    " 2>/dev/null || true

    sleep 0.5
}

verify_postgresql_stopped() {
    # Check 1: pg_isready
    if timeout 2 pg_isready -q 2>/dev/null; then
        return 1
    fi

    # Check 2: postmaster.pid process
    if [ -f "$PGDATA/postmaster.pid" ]; then
        local pg_pid
        pg_pid=$(head -1 "$PGDATA/postmaster.pid" 2>/dev/null || echo "")
        if [ -n "$pg_pid" ] && kill -0 "$pg_pid" 2>/dev/null; then
            return 1
        fi
    fi

    return 0
}

stop_postgresql() {
    log "Initiating PostgreSQL shutdown sequence"

    # Phase 1: Fence
    fence_postgresql

    # Phase 2: Fast stop
    log "Attempting fast shutdown..."
    timeout 8 pg_ctl -D "$PGDATA" stop -m fast -w -t 5 2>/dev/null || true

    if verify_postgresql_stopped; then
        log "PostgreSQL stopped (fast mode)"
        return 0
    fi

    # Phase 3: Immediate stop
    log "Attempting immediate shutdown..."
    timeout 5 pg_ctl -D "$PGDATA" stop -m immediate -w -t 3 2>/dev/null || true

    if verify_postgresql_stopped; then
        log "PostgreSQL stopped (immediate mode)"
        return 0
    fi

    # Phase 4: SIGKILL
    log "Sending SIGKILL to postgres processes..."
    if [ -f "$PGDATA/postmaster.pid" ]; then
        local pg_pid
        pg_pid=$(head -1 "$PGDATA/postmaster.pid" 2>/dev/null || echo "")
        if [ -n "$pg_pid" ]; then
            kill -KILL "$pg_pid" 2>/dev/null || true
        fi
    fi
    pkill -9 -f "postgres: " 2>/dev/null || true

    sleep 1

    if verify_postgresql_stopped; then
        log "PostgreSQL stopped (SIGKILL)"
        return 0
    fi

    log_error "FATAL: PostgreSQL still running after all stop attempts"
    return 1
}

#==============================================================================
# Heartbeat Functions
#==============================================================================

write_heartbeat() {
    echo "$(date +%s)" > "$HEARTBEAT_FILE" 2>/dev/null || true
}

#==============================================================================
# Main Loop
#==============================================================================

main() {
    log "Starting hardened Patroni watchdog (RFC-008)"
    log "Configuration:"
    log "  Patroni API: $PATRONI_API_FULL"
    log "  Check interval: ${CHECK_INTERVAL}s"
    log "  Failure threshold: $FAILURE_THRESHOLD"
    log "  Critical threshold: $CRITICAL_THRESHOLD"
    log "  Curl timeout: ${CURL_TIMEOUT}s"
    log "  DCS stale threshold: ${DCS_STALE_THRESHOLD}s"

    # Initial delay for Patroni startup
    log "Waiting ${STARTUP_DELAY}s for Patroni to initialize..."
    sleep "$STARTUP_DELAY"

    log "Beginning health check loop"

    while true; do
        write_heartbeat

        local health_status
        check_patroni_health
        health_status=$?

        case $health_status in
            0)  # Healthy
                if [ $failures -gt 0 ]; then
                    log "Patroni recovered after $failures check failures"
                fi
                failures=0
                critical_failures=0
                ((consecutive_successes++)) || true
                last_healthy_time=$(date +%s)

                # Periodic health log (every ~3 minutes)
                if [ $consecutive_successes -eq 1 ] || [ $((consecutive_successes % 60)) -eq 0 ]; then
                    log "Patroni healthy (consecutive checks: $consecutive_successes)"
                fi
                ;;

            1)  # Unhealthy (transient)
                ((failures++)) || true
                consecutive_successes=0
                log "Health check failed ($failures/$FAILURE_THRESHOLD)"

                if [ $failures -ge $FAILURE_THRESHOLD ]; then
                    log "CRITICAL: Patroni unresponsive for $((failures * CHECK_INTERVAL))s"
                    log "Initiating PostgreSQL shutdown to prevent split-brain"

                    stop_postgresql

                    log "Exiting watchdog to trigger container restart"
                    exit 1
                fi
                ;;

            2)  # Critical (immediate action required)
                ((critical_failures++)) || true
                consecutive_successes=0
                log "CRITICAL failure detected ($critical_failures/$CRITICAL_THRESHOLD)"

                if [ $critical_failures -ge $CRITICAL_THRESHOLD ]; then
                    log "CRITICAL: Immediate shutdown required (split-brain risk)"

                    stop_postgresql

                    log "Exiting watchdog to trigger container restart"
                    exit 2
                fi
                ;;

            *)  # Unexpected
                log_error "Unexpected health check status: $health_status"
                ((failures++)) || true
                ;;
        esac

        sleep "$CHECK_INTERVAL"
    done
}

# Run main function
main "$@"
```

### Component 4: Updated Supervisor Configuration

```ini
# supervisord.conf - Hardened process manager for Patroni HA
#
# Implements RFC-008: Watchdog Hardening for Split-Brain Prevention

[supervisord]
nodaemon=true
user=root
logfile=/dev/stdout
logfile_maxbytes=0
pidfile=/var/run/supervisord.pid
loglevel=info

[unix_http_server]
file=/var/run/supervisor.sock
chmod=0700

[rpcinterface:supervisor]
supervisor.rpcinterface_factory = supervisor.rpcinterface:make_main_rpcinterface

[supervisorctl]
serverurl=unix:///var/run/supervisor.sock

[program:patroni]
command=/usr/local/bin/patroni-runner.sh
user=postgres
autostart=true
autorestart=true
startsecs=10
startretries=3
stopwaitsecs=30
stopsignal=TERM
stopasgroup=true
killasgroup=true
stdout_logfile=/dev/stdout
stdout_logfile_maxbytes=0
stderr_logfile=/dev/stderr
stderr_logfile_maxbytes=0
environment=HOME="/home/postgres",USER="postgres"

[program:watchdog]
command=/usr/local/bin/patroni-watchdog.sh
user=postgres
autostart=true
# CHANGED: Only restart on unexpected exit, not clean exit
autorestart=unexpected
startsecs=5
# CHANGED: Limited retries to prevent restart loops
startretries=3
stdout_logfile=/dev/stdout
stdout_logfile_maxbytes=0
stderr_logfile=/dev/stderr
stderr_logfile_maxbytes=0
environment=HOME="/home/postgres",USER="postgres"

[eventlistener:watchdog_exit]
command=/usr/local/bin/supervisor-exit-handler.sh
events=PROCESS_STATE_EXITED,PROCESS_STATE_FATAL
stdout_logfile=/dev/stdout
stdout_logfile_maxbytes=0
stderr_logfile=/dev/stderr
stderr_logfile_maxbytes=0
```

### Component 5: Improved Exit Handler

```bash
#!/bin/bash
# supervisor-exit-handler.sh - Hardened critical process exit handler
#
# Implements RFC-008: Watchdog Hardening for Split-Brain Prevention

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] EXIT-HANDLER: $1"
}

log_error() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] EXIT-HANDLER ERROR: $1" >&2
}

kill_supervisord() {
    log "Terminating supervisord to trigger container restart"

    local pid_file="/var/run/supervisord.pid"
    local supervisord_pid

    if [ -f "$pid_file" ]; then
        supervisord_pid=$(cat "$pid_file" 2>/dev/null)
    fi

    # Method 1: Kill by PID file
    if [ -n "$supervisord_pid" ]; then
        log "Sending SIGTERM to supervisord (PID: $supervisord_pid)"
        kill -TERM "$supervisord_pid" 2>/dev/null || true
        sleep 2

        # Verify it's stopping
        if kill -0 "$supervisord_pid" 2>/dev/null; then
            log "SIGTERM ineffective, sending SIGKILL"
            kill -KILL "$supervisord_pid" 2>/dev/null || true
        fi
    fi

    # Method 2: Kill by process name (fallback)
    sleep 1
    if pgrep -x supervisord >/dev/null 2>&1; then
        log "Killing supervisord by name"
        pkill -KILL -x supervisord 2>/dev/null || true
    fi

    exit 1
}

# Supervisor event protocol
# See: http://supervisord.org/events.html

while true; do
    # Read event header
    read -r header

    # Parse header to get event length
    # Format: ver:3.0 server:supervisor serial:21 pool:watchdog_exit poolserial:10 eventname:PROCESS_STATE_EXITED len:84

    # Extract length
    len=$(echo "$header" | sed -n 's/.*len:\([0-9]*\).*/\1/p')

    if [ -z "$len" ] || [ "$len" -eq 0 ]; then
        # Acknowledge and continue if we can't parse
        echo "RESULT 2"
        echo "OK"
        continue
    fi

    # Read event payload
    read -r -n "$len" payload

    log "Event received: $header"
    log "Payload: $payload"

    # Check if this is a watchdog exit event
    # Payload format: processname:watchdog groupname:watchdog from_state:RUNNING expected:0 pid:1234

    if echo "$payload" | grep -q "processname:watchdog"; then
        # Extract exit expectation
        expected=$(echo "$payload" | sed -n 's/.*expected:\([0-9]*\).*/\1/p')
        from_state=$(echo "$payload" | sed -n 's/.*from_state:\([A-Z]*\).*/\1/p')

        log "Watchdog process event detected (from_state: $from_state, expected: $expected)"

        # Trigger container restart for any watchdog exit from RUNNING state
        if [ "$from_state" = "RUNNING" ]; then
            log "CRITICAL: Watchdog exited from RUNNING state"

            # Give a moment for logs to flush
            sleep 1

            kill_supervisord
        fi
    fi

    # Acknowledge event
    echo "RESULT 2"
    echo "OK"
done
```

## Migration Path

### Phase 1: Non-Breaking Changes

1. Add curl timeouts to existing watchdog
2. Reduce CHECK_INTERVAL to 3s
3. Update supervisor to `autorestart=unexpected`

**Risk**: Low - improves detection time without changing behavior

### Phase 2: Enhanced Health Checks

1. Replace `/health` check with `/patroni` endpoint parsing
2. Add leader lock verification for primary nodes
3. Implement proper jq-based JSON parsing

**Risk**: Medium - requires jq dependency, more complex logic

### Phase 3: Guaranteed Termination

1. Implement fencing (connection termination)
2. Add SIGKILL fallback
3. Add verification after each stop attempt

**Risk**: Medium - more aggressive PostgreSQL shutdown

### Phase 4: Full Implementation

1. Deploy complete hardened watchdog
2. Update supervisor configuration
3. Deploy improved exit handler

**Risk**: Low - full replacement with tested code

## Testing Plan

### Unit Tests

1. **Health check parsing**: Various `/patroni` response formats
2. **Leader verification**: Primary with/without lock
3. **PostgreSQL stop**: Each escalation phase
4. **Exit handler**: Event parsing accuracy

### Integration Tests

1. **Patroni crash simulation**: Kill -9 patroni process
2. **Network partition**: Block etcd connectivity
3. **Slow PostgreSQL**: Long-running transaction during shutdown
4. **Hung Patroni**: Process alive but not responding

### Chaos Tests

1. **Random Patroni kills** during load
2. **Network partitions** of various durations
3. **Resource exhaustion** (CPU, memory, disk)
4. **Clock skew** between nodes

### Timing Verification

```bash
# Test script to measure actual detection-to-stop time
test_timing() {
    local start_time=$(date +%s.%N)

    # Kill Patroni
    pkill -9 patroni

    # Wait for PostgreSQL to stop
    while pg_isready -q 2>/dev/null; do
        sleep 0.1
    done

    local end_time=$(date +%s.%N)
    local elapsed=$(echo "$end_time - $start_time" | bc)

    echo "Detection-to-stop time: ${elapsed}s"

    # Should be < 15s (TTL - safety margin)
    if (( $(echo "$elapsed > 15" | bc -l) )); then
        echo "FAIL: Exceeded safety threshold"
        return 1
    fi

    echo "PASS: Within safety threshold"
    return 0
}
```

## Metrics and Observability

### New Metrics

```
# Watchdog health
patroni_watchdog_consecutive_successes gauge
patroni_watchdog_failures_total counter
patroni_watchdog_critical_failures_total counter
patroni_watchdog_last_healthy_timestamp gauge

# PostgreSQL stop operations
patroni_watchdog_pg_stop_duration_seconds histogram
patroni_watchdog_pg_stop_phase gauge  # 1=fast, 2=immediate, 3=sigkill
patroni_watchdog_pg_stop_success_total counter
patroni_watchdog_pg_stop_failure_total counter

# Health check latency
patroni_watchdog_health_check_duration_seconds histogram
patroni_watchdog_health_check_timeout_total counter
```

### Log Messages

All log messages follow the format:
```
[YYYY-MM-DD HH:MM:SS] WATCHDOG: <message>
[YYYY-MM-DD HH:MM:SS] WATCHDOG ERROR: <message>
```

Critical events that should trigger alerts:
- `CRITICAL: Primary role without leader lock ownership`
- `CRITICAL: Patroni unresponsive for Xs`
- `CRITICAL: Immediate shutdown required`
- `FATAL: PostgreSQL still running after all stop attempts`

## Security Considerations

1. **Privilege escalation**: Watchdog runs as postgres user, uses pg_ctl (not sudo)
2. **Signal handling**: Only sends signals to postgres processes, verified by PID
3. **Connection termination**: Uses PostgreSQL's pg_terminate_backend, not OS-level kills
4. **Heartbeat file**: Written to /tmp, readable by monitoring systems

## Rollback Plan

If issues are detected after deployment:

1. **Immediate**: Set `WATCHDOG_FAILURE_THRESHOLD=999` to effectively disable
2. **Short-term**: Revert to RFC-007 watchdog script
3. **Long-term**: Address specific issues and redeploy

## Success Criteria

1. **Detection time**: < 10 seconds from Patroni failure to PostgreSQL stop initiation
2. **Total time**: < 15 seconds from Patroni failure to container restart
3. **False positive rate**: < 1 unnecessary restart per week per node
4. **Split-brain incidents**: Zero after deployment

## Timeline

- **Week 1**: Phase 1 (non-breaking changes)
- **Week 2**: Phase 2 (enhanced health checks) + testing
- **Week 3**: Phase 3 (guaranteed termination) + testing
- **Week 4**: Phase 4 (full deployment) + monitoring

## References

- [RFC-007: Split-Brain Prevention When Patroni Sidecar Crashes](./RFC-007-SPLIT-BRAIN-PREVENTION.md)
- [Patroni Watchdog Documentation](https://patroni.readthedocs.io/en/latest/watchdog.html)
- [Patroni Source: watchdog/base.py](https://github.com/patroni/patroni/blob/master/patroni/watchdog/base.py)
- [Patroni Source: ha.py](https://github.com/patroni/patroni/blob/master/patroni/ha.py)
- [Linux Watchdog Driver API](https://www.kernel.org/doc/html/latest/watchdog/watchdog-api.html)
- [Supervisor Event Protocol](http://supervisord.org/events.html)

## Appendix A: Configuration Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `WATCHDOG_CHECK_INTERVAL` | 3 | Seconds between health checks |
| `WATCHDOG_FAILURE_THRESHOLD` | 2 | Consecutive failures before action |
| `WATCHDOG_CRITICAL_THRESHOLD` | 1 | Critical failures before immediate action |
| `WATCHDOG_CURL_TIMEOUT` | 2 | HTTP request timeout (seconds) |
| `WATCHDOG_CURL_CONNECT_TIMEOUT` | 1 | TCP connect timeout (seconds) |
| `WATCHDOG_DCS_STALE_THRESHOLD` | 15 | Max age of DCS contact (seconds) |
| `WATCHDOG_STARTUP_DELAY` | 10 | Initial delay before monitoring |
| `WATCHDOG_HEARTBEAT_FILE` | /tmp/patroni-watchdog-heartbeat | Heartbeat file path |

## Appendix B: Comparison with Kernel Watchdog

| Aspect | Kernel Watchdog | Application Watchdog (RFC-008) |
|--------|-----------------|-------------------------------|
| Trigger mechanism | Kernel timer | Process monitoring |
| Action | Machine reboot | Container restart |
| Timing guarantee | Hardware-level | Software-level |
| Maximum detection time | Configurable (usually 15s) | 6 seconds |
| Maximum total time | 15s + boot time | 14s + container restart |
| Protects against kernel panic | Yes | No |
| Protects against Patroni crash | Yes | Yes |
| Protects against network partition | Yes (via DCS) | Yes (via API checks) |
| Container-friendly | No | Yes |
| Requires privileges | Yes (/dev/watchdog) | No |

## Appendix C: State Machine

```
                    ┌─────────────┐
                    │   START     │
                    └──────┬──────┘
                           │
                           ▼
                    ┌─────────────┐
              ┌────▶│   HEALTHY   │◀────┐
              │     └──────┬──────┘     │
              │            │            │
              │     health check        │
              │     fails               │ health check
              │            │            │ succeeds
              │            ▼            │
              │     ┌─────────────┐     │
              │     │  DEGRADED   │─────┘
              │     │ (failures<  │
              │     │  threshold) │
              │     └──────┬──────┘
              │            │
              │     failures >=
              │     threshold
              │            │
              │            ▼
              │     ┌─────────────┐
              │     │  CRITICAL   │
              │     └──────┬──────┘
              │            │
              │     stop PostgreSQL
              │            │
              │            ▼
              │     ┌─────────────┐
              │     │ TERMINATED  │
              │     └──────┬──────┘
              │            │
              │     exit(1)
              │            │
              │            ▼
              │     ┌─────────────┐
              └─────│  RESTARTED  │ (container restart)
                    └─────────────┘
```
