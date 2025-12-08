#!/bin/bash
# patroni-watchdog.sh - Application-level watchdog for Patroni
#
# This script monitors Patroni's health and stops PostgreSQL if Patroni
# becomes unresponsive, preventing split-brain scenarios where PostgreSQL
# continues accepting writes after another node becomes leader.
#
# Part of RFC-007: Split-Brain Prevention

set -e

PATRONI_API="${PATRONI_API_URL:-http://localhost:8008/health}"
CHECK_INTERVAL="${WATCHDOG_CHECK_INTERVAL:-5}"
FAILURE_THRESHOLD="${WATCHDOG_FAILURE_THRESHOLD:-3}"
PGDATA="${PGDATA:-/var/lib/postgresql/data}"

failures=0
consecutive_successes=0

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] WATCHDOG: $1"
}

log "Starting Patroni watchdog"
log "  API endpoint: $PATRONI_API"
log "  Check interval: ${CHECK_INTERVAL}s"
log "  Failure threshold: $FAILURE_THRESHOLD"

# Wait for Patroni to start initially
sleep 10

while true; do
    if curl -sf "$PATRONI_API" > /dev/null 2>&1; then
        if [ $failures -gt 0 ]; then
            log "Patroni recovered after $failures failures"
        fi
        failures=0
        ((consecutive_successes++)) || true

        # Only log periodically after recovery
        if [ $consecutive_successes -eq 1 ] || [ $((consecutive_successes % 60)) -eq 0 ]; then
            log "Patroni healthy (checks: $consecutive_successes)"
        fi
    else
        ((failures++)) || true
        consecutive_successes=0
        log "Patroni health check failed ($failures/$FAILURE_THRESHOLD)"

        if [ $failures -ge $FAILURE_THRESHOLD ]; then
            log "CRITICAL: Patroni unresponsive for $((failures * CHECK_INTERVAL))s"
            log "Initiating PostgreSQL shutdown to prevent split-brain..."

            # Try graceful stop first
            if [ -f "$PGDATA/postmaster.pid" ]; then
                log "Stopping PostgreSQL (fast mode)..."
                pg_ctl -D "$PGDATA" stop -m fast -w -t 10 2>/dev/null || true

                # If still running, force immediate shutdown
                if [ -f "$PGDATA/postmaster.pid" ]; then
                    log "Forcing immediate PostgreSQL shutdown..."
                    pg_ctl -D "$PGDATA" stop -m immediate -w -t 5 2>/dev/null || true
                fi
            fi

            log "Exiting watchdog to trigger container restart"
            exit 1
        fi
    fi

    sleep "$CHECK_INTERVAL"
done
