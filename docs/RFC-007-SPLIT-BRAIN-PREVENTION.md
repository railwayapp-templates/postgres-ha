# RFC-007: Split-Brain Prevention When Patroni Sidecar Crashes

## Status
Implemented (Phase 1 & 2)

## Summary
This RFC documents the mechanisms for preventing split-brain scenarios in the postgres-ha template when the Patroni sidecar process crashes, becomes unresponsive, or is killed. It analyzes Patroni's built-in protections and recommends additional safeguards for Railway's containerized environment.

## Problem Statement

In a PostgreSQL high-availability cluster managed by Patroni, split-brain occurs when multiple nodes believe they are the primary and accept writes simultaneously. This leads to:
- Data corruption and inconsistency
- Transaction loss
- Divergent database states that are difficult to reconcile

The specific scenario we must address: **What happens if the Patroni process crashes or becomes unresponsive while PostgreSQL continues running?**

Without safeguards, a crashed Patroni on the primary node could:
1. Fail to renew its leader lock in etcd
2. Allow another node to acquire the leader lock and promote
3. Result in two PostgreSQL instances accepting writes simultaneously

## Patroni's Built-in Protection Mechanisms

### 1. Self-Fencing (Primary Protection)

Patroni implements self-fencing as its primary split-brain protection. When running normally:

```
Primary Node:
1. Patroni holds leader lock in etcd (TTL=30s default)
2. Patroni renews lock every loop_wait (10s default)
3. If renewal fails → Patroni demotes PostgreSQL to read-only
4. PostgreSQL stops accepting writes
```

**Limitation**: This only works if Patroni is running. If Patroni crashes, PostgreSQL continues running unmanaged.

### 2. Linux Watchdog (Secondary Protection)

Patroni supports hardware/software watchdog devices that reset the entire system if Patroni stops sending keepalives:

```yaml
watchdog:
  mode: required  # off | automatic | required
  device: /dev/watchdog
  safety_margin: 5  # seconds before TTL expiry
```

**How it works**:
1. Patroni sends keepalive to `/dev/watchdog` during each HA loop
2. If Patroni crashes → no keepalive → system reset after timeout
3. Reset prevents zombie primary from accepting writes

**Timing with defaults** (loop_wait=10, ttl=30):
```
T+0s   Last successful loop iteration
T+10s  Missed loop iteration (Patroni crashed)
T+25s  Watchdog triggers system reset (30-5=25s)
T+30s  Leader key expires in etcd
T+40s  New leader elected
```

### 3. DCS Failsafe Mode

When etcd becomes unavailable (not a Patroni crash, but related):

```yaml
failsafe_mode: true
```

- Primary continues operating if it can reach ALL cluster members via REST API
- If any member unreachable → immediate demotion
- Prevents minority partition from maintaining a primary

### 4. Synchronous Replication Modes

```yaml
synchronous_mode: true         # Prevent promoting stale replicas
synchronous_mode_strict: true  # Block writes if no sync standby
```

These don't prevent split-brain directly but minimize data loss if it occurs.

## Analysis: Current Template Gaps

Our current postgres-ha template has the following gaps:

| Protection | Official Patroni | Our Template | Gap |
|------------|------------------|--------------|-----|
| Self-fencing | ✅ Built-in | ✅ Inherited | None |
| Watchdog | ✅ Supported | ❌ Not configured | **Critical** |
| DCS Failsafe | ✅ Available | ❌ Not enabled | Medium |
| Process supervisor | N/A | ❌ Not present | **Critical** |
| Container restart | N/A | ✅ `restartPolicyType=always` | Partial |

### Critical Gap: No Watchdog

In containerized environments like Railway:
- Hardware watchdog is not available
- Software watchdog (`softdog`) requires privileged containers
- Without watchdog, a crashed Patroni leaves PostgreSQL running unmanaged

### Critical Gap: No Process Supervisor

If Patroni crashes:
- Container stays running (PostgreSQL is PID 1 or entrypoint continues)
- Container restart policy doesn't trigger (container hasn't exited)
- PostgreSQL continues accepting writes while another node becomes leader

## Recommended Solutions

### Solution 1: Supervisord Process Manager (Recommended)

Use supervisord to manage both Patroni and implement a "watchdog" behavior:

```ini
# /etc/supervisor/conf.d/patroni.conf
[program:patroni]
command=patroni /tmp/patroni.yml
user=postgres
autorestart=true
startsecs=10
stopwaitsecs=30

# If Patroni exits, stop PostgreSQL immediately
stopsignal=TERM
stopasgroup=true
killasgroup=true

[program:patroni-watchdog]
command=/usr/local/bin/patroni-watchdog.sh
user=postgres
autorestart=true
startsecs=5
```

```bash
#!/bin/bash
# /usr/local/bin/patroni-watchdog.sh
# Custom watchdog that stops PostgreSQL if Patroni is not responding

PATRONI_API="http://localhost:8008/health"
CHECK_INTERVAL=5
FAILURE_THRESHOLD=3
failures=0

while true; do
    if curl -sf "$PATRONI_API" > /dev/null 2>&1; then
        failures=0
    else
        ((failures++))
        echo "Patroni health check failed ($failures/$FAILURE_THRESHOLD)"

        if [ $failures -ge $FAILURE_THRESHOLD ]; then
            echo "CRITICAL: Patroni unresponsive, stopping PostgreSQL"
            pg_ctl -D "$PGDATA" stop -m fast
            # Exit to trigger container restart
            exit 1
        fi
    fi
    sleep $CHECK_INTERVAL
done
```

**Pros**:
- Works in unprivileged containers
- Provides automatic Patroni restart
- Implements application-level watchdog
- Stops PostgreSQL if Patroni is unrecoverable

**Cons**:
- Additional complexity
- ~15 second detection window

### Solution 2: PostgreSQL `on_stop` Hook with Reverse Check

Configure PostgreSQL to check Patroni health and self-terminate:

```yaml
# patroni.yml
postgresql:
  callbacks:
    on_start: /callbacks/verify-patroni.sh
  parameters:
    # Periodically verify Patroni is alive
    log_destination: 'stderr'
```

This is less reliable as PostgreSQL doesn't have native periodic health checks.

### Solution 3: Sidecar Watchdog Container (Kubernetes-style)

Deploy a separate lightweight container that monitors Patroni:

```yaml
# docker-compose addition
patroni-watchdog:
  image: curlimages/curl:latest
  command: |
    while true; do
      if ! curl -sf http://postgres-1.railway.internal:8008/health; then
        echo "Patroni down, triggering restart"
        # Signal Railway to restart the postgres container
        exit 1
      fi
      sleep 5
    done
  depends_on:
    - postgres-1
```

**Pros**:
- Separation of concerns
- Can trigger Railway restart mechanisms

**Cons**:
- Additional container per postgres node
- Network dependency

### Solution 4: Enable Software Watchdog (If Privileged)

If Railway supports privileged containers:

```dockerfile
# In Dockerfile
RUN apt-get install -y watchdog

# In wrapper.sh (before starting Patroni)
if [ -e /dev/watchdog ]; then
    chown postgres /dev/watchdog
fi
```

```yaml
# patroni.yml
watchdog:
  mode: required
  device: /dev/watchdog
  safety_margin: 5
```

**Pros**:
- Official Patroni-supported method
- Most robust protection

**Cons**:
- Requires privileged container
- May not be available on Railway

### Solution 5: Aggressive Container Health Check

Configure container health check to verify BOTH PostgreSQL and Patroni:

```dockerfile
HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=2 \
  CMD curl -sf http://localhost:8008/health && pg_isready -U postgres
```

Combined with `restartPolicyType=always`, this triggers container restart when Patroni fails.

**Current healthcheck** (problem):
```dockerfile
# Only checks Patroni OR PostgreSQL, not both bound together
CMD if [ "${PATRONI_ENABLED:-false}" = "true" ]; then
      curl -f http://localhost:8008/health || exit 1
    else
      pg_isready -U ${POSTGRES_USER:-postgres} || exit 1
    fi
```

**Improved healthcheck**:
```dockerfile
# Requires BOTH to be healthy when Patroni is enabled
CMD if [ "${PATRONI_ENABLED:-false}" = "true" ]; then
      curl -sf http://localhost:8008/health || exit 1
    fi && pg_isready -U ${POSTGRES_USER:-postgres}
```

## Recommended Implementation

### Phase 1: Immediate (Low Risk)

1. **Enable DCS Failsafe Mode** in Patroni config:
   ```yaml
   failsafe_mode: true
   ```

2. **Improve health check** to fail faster when Patroni crashes:
   ```dockerfile
   HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=2 \
     CMD curl -sf http://localhost:8008/health || exit 1
   ```

3. **Reduce TTL and loop_wait** for faster detection:
   ```yaml
   ttl: 20
   loop_wait: 5
   retry_timeout: 5
   ```

### Phase 2: Medium Term (Recommended)

4. **Add supervisord** with custom watchdog script:
   - Ensures Patroni restarts automatically
   - Stops PostgreSQL if Patroni is unrecoverable
   - Exits container to trigger Railway restart

### Phase 3: Long Term (If Available)

5. **Enable software watchdog** if Railway supports privileged containers

## Timing Analysis

### Current Behavior (No Additional Safeguards)

```
T+0s    Patroni crashes
T+0-∞   PostgreSQL continues running, accepting writes
T+30s   Leader key expires in etcd
T+40s   Another node becomes leader
T+40s+  SPLIT BRAIN: Two primaries accepting writes
```

### With Recommended Safeguards

```
T+0s    Patroni crashes
T+5s    First health check fails
T+10s   Second health check fails (retries=2)
T+10s   Container marked unhealthy, restart triggered
T+15s   Container restarting, PostgreSQL stopped
T+20s   Leader key expires in etcd (if not already released)
T+25s   New leader elected OR original node recovers
```

**Maximum split-brain window**: ~5-15 seconds (vs unlimited without safeguards)

## Configuration Reference

### Recommended patroni.yml Changes

```yaml
# Faster failure detection
ttl: 20
loop_wait: 5
retry_timeout: 5

# Enable failsafe mode
failsafe_mode: true

# Watchdog (if available)
watchdog:
  mode: automatic  # or 'required' if guaranteed available
  device: /dev/watchdog
  safety_margin: 5

# Synchronous mode for critical workloads
synchronous_mode: false  # Enable if data loss is unacceptable
synchronous_mode_strict: false  # Enable if availability can be sacrificed
```

### Recommended Dockerfile Changes

```dockerfile
# Stricter health check
HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=2 \
  CMD if [ "${PATRONI_ENABLED:-false}" = "true" ]; then \
        curl -sf http://localhost:8008/health || exit 1; \
      fi && pg_isready -U ${POSTGRES_USER:-postgres}
```

## Trade-offs

| Approach | Split-Brain Risk | Availability Impact | Complexity |
|----------|------------------|---------------------|------------|
| Current (no changes) | High | None | None |
| Faster health checks | Medium | Low (more restarts) | Low |
| Supervisord + watchdog | Low | Low | Medium |
| Software watchdog | Very Low | Low | High (privileged) |
| Sync replication strict | Very Low | High (blocks on no standby) | Low |

## Decision

**Recommended approach**: Implement Phase 1 immediately, Phase 2 as follow-up.

The combination of:
- Aggressive health checks (5s interval, 2 retries)
- DCS failsafe mode
- Reduced TTL (20s)
- Container restart policy

Provides reasonable split-brain protection (~15s maximum window) without requiring privileged containers or significant architectural changes.

## Implementation Status

### Phase 1 - Completed

1. **Patroni config changes** (`docker-entrypoint.sh`, `patroni-runner.sh`):
   - `ttl: 20` (was 30)
   - `loop_wait: 5` (was 10)
   - `retry_timeout: 5` (was 10)
   - `failsafe_mode: true` (new)

2. **Health check** (`Dockerfile`):
   - Interval: 5s (was 10s)
   - Retries: 2 (was 3)
   - Timeout: 3s (was 5s)

### Phase 2 - Completed

3. **Supervisord integration**:
   - Added `supervisor` package to Dockerfile
   - Created `supervisord.conf` to manage Patroni and watchdog
   - Updated `wrapper.sh` to use supervisord in Patroni mode

4. **Watchdog script** (`patroni-watchdog.sh`):
   - Monitors Patroni REST API every 5 seconds
   - After 3 consecutive failures (15s), stops PostgreSQL
   - Exits to trigger container restart

5. **Supporting scripts**:
   - `patroni-runner.sh`: Generates config and runs Patroni under supervisord
   - `supervisor-exit-handler.sh`: Triggers container restart on watchdog exit

6. **Cleanup**:
   - Removed redundant `docker-entrypoint.sh` (replaced by `patroni-runner.sh`)

## References

- [Patroni Watchdog Documentation](https://patroni.readthedocs.io/en/latest/watchdog.html)
- [Patroni FAQ - Split Brain](https://patroni.readthedocs.io/en/latest/faq.html)
- [Patroni DCS Failsafe Mode](https://patroni.readthedocs.io/en/latest/dcs_failsafe_mode.html)
- [Patroni GitHub Issue #1114 - Split Brain Management](https://github.com/patroni/patroni/issues/1114)
- [Stormatics - Split-Brain in PostgreSQL Clusters](https://stormatics.tech/blogs/split-brain-in-postgresql-clusters-causes-prevention-and-resolution)
- [AWS Prescriptive Guidance - Patroni and etcd](https://docs.aws.amazon.com/prescriptive-guidance/latest/migration-databases-postgresql-ec2/ha-patroni-etcd-considerations.html)

## Appendix A: Patroni Self-Fencing Mechanism

From Patroni maintainer (CyberDem0n) in GitHub Issue #1114:

> "Patroni is doing self-fencing (terminates postgres) if the leader failed to update the leader lock in etcd. The watchdog has a different purpose, it will restart the whole node if Patroni for some reason stopped working."

This confirms that:
1. Self-fencing handles normal DCS communication failures
2. Watchdog specifically addresses Patroni process failures
3. Both are needed for comprehensive protection

## Appendix B: Example Supervisord Configuration

```ini
[supervisord]
nodaemon=true
user=root

[program:patroni]
command=gosu postgres patroni /tmp/patroni.yml
autostart=true
autorestart=true
startsecs=10
stopwaitsecs=30
stdout_logfile=/dev/stdout
stdout_logfile_maxbytes=0
stderr_logfile=/dev/stderr
stderr_logfile_maxbytes=0

[program:watchdog]
command=/usr/local/bin/patroni-watchdog.sh
autostart=true
autorestart=true
startsecs=5
stdout_logfile=/dev/stdout
stdout_logfile_maxbytes=0
stderr_logfile=/dev/stderr
stderr_logfile_maxbytes=0

[eventlistener:process_exit]
command=/usr/local/bin/kill-all-on-exit.sh
events=PROCESS_STATE_FATAL,PROCESS_STATE_EXITED
```
