#!/bin/bash
# supervisor-exit-handler.sh - Handle critical process exits
#
# This script is called by supervisord when processes exit.
# If the watchdog exits (indicating unrecoverable Patroni failure),
# we kill supervisord to trigger container restart.
#
# Part of RFC-007: Split-Brain Prevention

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] EXIT-HANDLER: $1"
}

while read line; do
    # Parse supervisor event
    echo "$line"

    # Check if it's a process state event
    if echo "$line" | grep -q "processname:watchdog"; then
        if echo "$line" | grep -qE "(EXITED|FATAL)"; then
            log "CRITICAL: Watchdog process exited - triggering container restart"

            # Give a moment for logs to flush
            sleep 1

            # Kill supervisord to trigger container restart
            kill -TERM $(cat /var/run/supervisord.pid) 2>/dev/null || true

            # If that doesn't work, force exit
            sleep 2
            kill -KILL $(cat /var/run/supervisord.pid) 2>/dev/null || true
            exit 1
        fi
    fi

    # Acknowledge the event to supervisor
    echo "RESULT 2"
    echo "OK"
done
