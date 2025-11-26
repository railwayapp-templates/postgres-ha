# RFC-007: HAProxy + PostgreSQL Streaming Replication

## Overview

HAProxy with PostgreSQL streaming replication is a pattern rather than a single product. HAProxy acts as a TCP/HTTP load balancer that routes PostgreSQL connections based on health checks. Combined with native PostgreSQL streaming replication and an external failover manager (Patroni, repmgr, etc.), it provides a robust HA solution. This RFC covers HAProxy as the connection routing layer.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        │                     │                     │
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│   HAProxy 1   │     │   HAProxy 2   │     │   HAProxy 3   │
│   (Active)    │◄───►│  (Standby)    │◄───►│  (Standby)    │
│               │ VRRP│               │ VRRP│               │
│   VIP Holder  │     │               │     │               │
└───────┬───────┘     └───────┬───────┘     └───────┬───────┘
        │                     │                     │
        └─────────────────────┼─────────────────────┘
                              │
                    Port 5432 (read-write)
                    Port 5433 (read-only)
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│  PostgreSQL   │     │  PostgreSQL   │     │  PostgreSQL   │
│   (Primary)   │────►│   (Standby)   │────►│   (Standby)   │
│               │ WAL │               │ WAL │               │
│  + Patroni    │     │  + Patroni    │     │  + Patroni    │
└───────────────┘     └───────────────┘     └───────────────┘
        │                     │                     │
        └─────────────────────┼─────────────────────┘
                              │
                          etcd / Consul
                    (Patroni DCS - manages failover)
```

## Core Components

### 1. HAProxy
- Layer 4 (TCP) or Layer 7 (HTTP) load balancer
- Health checks against PostgreSQL backends
- Separate frontends for read-write and read-only traffic
- Connection queuing and limiting

### 2. PostgreSQL Streaming Replication
- Native PostgreSQL feature
- WAL-based replication
- Synchronous or asynchronous modes
- Hot standby for read queries

### 3. Failover Manager (Patroni/repmgr/etc.)
- Manages actual PostgreSQL failover
- HAProxy doesn't promote standbys
- HAProxy only routes based on health checks

### 4. Keepalived (Optional)
- VRRP for HAProxy high availability
- Virtual IP management
- Failover between HAProxy instances

## How It Works

### Health Check Mechanism

```
HAProxy Health Check Options:
─────────────────────────────────────────────────────────────────

Option 1: TCP Check (Basic)
        - Just checks port is open
        - Fast but not accurate
        - PostgreSQL might be in recovery

Option 2: PostgreSQL Protocol Check
        - Sends PostgreSQL startup message
        - Checks for valid response
        - Better than TCP only

Option 3: HTTP Check via Patroni/pgBouncer API
        - GET /primary → 200 = primary
        - GET /replica → 200 = replica
        - Most accurate method

Option 4: External Script (agent-check)
        - Custom script runs on backend
        - Reports UP/DOWN status
        - Maximum flexibility
```

### Routing Logic with Patroni REST API

```
HAProxy Query Flow:
─────────────────────────────────────────────────────────────────

1. Client connects to HAProxy port 5432 (read-write)

2. HAProxy checks backend health:
   - GET http://pg1:8008/primary
   - GET http://pg2:8008/primary
   - GET http://pg3:8008/primary

3. Only primary returns 200:
   - pg1:8008/primary → 200 OK (is primary)
   - pg2:8008/primary → 503 Service Unavailable
   - pg3:8008/primary → 503 Service Unavailable

4. HAProxy routes to pg1

5. For read-only (port 5433):
   - GET http://pg1:8008/replica
   - GET http://pg2:8008/replica
   - GET http://pg3:8008/replica

6. Standbys return 200:
   - pg1:8008/replica → 503 (is primary)
   - pg2:8008/replica → 200 OK
   - pg3:8008/replica → 200 OK

7. HAProxy load balances to pg2 or pg3
```

### Failover Sequence

```
Failover with HAProxy + Patroni:
─────────────────────────────────────────────────────────────────

t=0s    Primary (pg1) crashes

t=5s    HAProxy health check fails
        - /primary returns error
        - pg1 marked DOWN

t=10s   HAProxy has no primary backend
        - Read-write port 5432: no backends
        - Connections queued/rejected

t=30s   Patroni promotes pg2
        - Patroni failover completes
        - pg2 now returns 200 on /primary

t=35s   HAProxy health check succeeds
        - pg2:8008/primary → 200 OK
        - pg2 marked UP for read-write

t=36s   Traffic routes to pg2
        - New connections succeed
        - Existing connections must retry

Timeline: HAProxy doesn't control failover
          It reacts to state changes
          Failover speed = Patroni speed + health check interval
```

## Configuration

### HAProxy Configuration (haproxy.cfg)

```haproxy
global
    log /dev/log local0
    chroot /var/lib/haproxy
    stats socket /run/haproxy/admin.sock mode 660 level admin
    stats timeout 30s
    user haproxy
    group haproxy
    daemon

    # SSL settings
    ssl-default-bind-ciphers ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256
    ssl-default-bind-options ssl-min-ver TLSv1.2

defaults
    log     global
    mode    tcp
    option  tcplog
    option  dontlognull
    timeout connect 5000ms
    timeout client  30000ms
    timeout server  30000ms
    retries 3

#-----------------------------------------------------------------------
# Stats Interface
#-----------------------------------------------------------------------
listen stats
    bind *:8404
    mode http
    stats enable
    stats uri /stats
    stats refresh 10s
    stats admin if LOCALHOST

#-----------------------------------------------------------------------
# PostgreSQL Primary (Read-Write)
#-----------------------------------------------------------------------
listen postgres_primary
    bind *:5432
    mode tcp
    option httpchk GET /primary
    http-check expect status 200
    default-server inter 3s fall 3 rise 2 on-marked-down shutdown-sessions

    server pg1 pg1.example.com:5432 check port 8008
    server pg2 pg2.example.com:5432 check port 8008
    server pg3 pg3.example.com:5432 check port 8008

#-----------------------------------------------------------------------
# PostgreSQL Replicas (Read-Only)
#-----------------------------------------------------------------------
listen postgres_replicas
    bind *:5433
    mode tcp
    balance roundrobin
    option httpchk GET /replica
    http-check expect status 200
    default-server inter 3s fall 3 rise 2

    server pg1 pg1.example.com:5432 check port 8008
    server pg2 pg2.example.com:5432 check port 8008
    server pg3 pg3.example.com:5432 check port 8008

#-----------------------------------------------------------------------
# PostgreSQL Any (for maintenance)
#-----------------------------------------------------------------------
listen postgres_any
    bind *:5434
    mode tcp
    balance roundrobin
    option httpchk GET /health
    http-check expect status 200
    default-server inter 3s fall 3 rise 2

    server pg1 pg1.example.com:5432 check port 8008
    server pg2 pg2.example.com:5432 check port 8008
    server pg3 pg3.example.com:5432 check port 8008
```

### HAProxy Configuration for pg_auto_failover

```haproxy
# Using pg_auto_failover (no REST API - use pgsql-check)
listen postgres_primary
    bind *:5432
    mode tcp
    option pgsql-check user haproxy
    default-server inter 3s fall 3 rise 2

    # Primary detection via pg_is_in_recovery()
    server pg1 pg1.example.com:5432 check
    server pg2 pg2.example.com:5432 check backup

# Requires custom check script for accurate primary detection
```

### Keepalived Configuration (keepalived.conf)

```
global_defs {
    router_id HAProxy1
    vrrp_skip_check_adv_addr
    enable_script_security
}

vrrp_script check_haproxy {
    script "/usr/bin/killall -0 haproxy"
    interval 2
    weight 2
    fall 3
    rise 2
}

vrrp_instance VI_1 {
    state MASTER
    interface eth0
    virtual_router_id 51
    priority 100
    advert_int 1

    authentication {
        auth_type PASS
        auth_pass secretpassword
    }

    virtual_ipaddress {
        192.168.1.100/24
    }

    track_script {
        check_haproxy
    }

    notify_master "/etc/keepalived/notify.sh master"
    notify_backup "/etc/keepalived/notify.sh backup"
    notify_fault  "/etc/keepalived/notify.sh fault"
}
```

### HAProxy with Custom Health Check Script

```haproxy
# For setups without REST API
listen postgres_primary
    bind *:5432
    mode tcp

    # Use external check
    option external-check
    external-check path "/usr/bin:/bin"
    external-check command "/etc/haproxy/check_postgres.sh"

    server pg1 pg1.example.com:5432 check
    server pg2 pg2.example.com:5432 check
    server pg3 pg3.example.com:5432 check
```

```bash
#!/bin/bash
# /etc/haproxy/check_postgres.sh
# $1 = address, $2 = port

PGPASSWORD="haproxy_password" psql -h "$1" -p "$2" -U haproxy -d postgres -c "SELECT NOT pg_is_in_recovery();" -tA 2>/dev/null | grep -q 't'

exit $?
```

## Happy Path Scenarios

### Scenario 1: Normal Read-Write Operation

```
Timeline: Application writing data
─────────────────────────────────────────────────────────────────

t=0ms   Client connects to HAProxy:5432 (read-write)

t=1ms   HAProxy selects backend
        - pg1 healthy (primary)
        - Routes to pg1

t=2ms   PostgreSQL connection established
        - Client authenticates
        - Session begins

t=10ms  Client executes:
        INSERT INTO users (name) VALUES ('Alice');

t=15ms  Transaction committed on pg1
        - WAL generated
        - Replicated to standbys

t=20ms  Response returned to client
```

### Scenario 2: Read Load Balancing

```
Timeline: Multiple read queries
─────────────────────────────────────────────────────────────────

t=0ms   Client 1 connects to HAProxy:5433 (read-only)
        - HAProxy round-robin
        - Routes to pg2

t=5ms   Client 2 connects to HAProxy:5433
        - Round-robin continues
        - Routes to pg3

t=10ms  Client 3 connects to HAProxy:5433
        - Routes to pg2

t=15ms  Distribution:
        - pg2: 2 connections
        - pg3: 1 connection
        - pg1: 0 (is primary, not in replica pool)

Read scaling achieved across standbys
```

### Scenario 3: Rolling Maintenance

```
Timeline: Updating PostgreSQL nodes one by one
─────────────────────────────────────────────────────────────────

Step 1: Drain pg3 (standby)
$ echo "set server postgres_replicas/pg3 state drain" | socat stdio /run/haproxy/admin.sock

t=0s    pg3 stops receiving new connections
t=60s   Existing connections close
t=65s   Safely update pg3

Step 2: Return pg3
$ echo "set server postgres_replicas/pg3 state ready" | socat stdio /run/haproxy/admin.sock

Step 3: Drain pg2 (standby)
        Repeat process

Step 4: Switchover primary
        Use Patroni: patronictl switchover
        HAProxy automatically detects new primary

Step 5: Update old primary (now standby)

Zero-downtime maintenance achieved
```

## Unhappy Path Scenarios

### Scenario 1: Primary Failure

```
Timeline: Primary PostgreSQL crashes
─────────────────────────────────────────────────────────────────

t=0s    pg1 (primary) crashes

t=3s    HAProxy health check fails
        - GET http://pg1:8008/primary fails
        - Retrying...

t=6s    Second check fails

t=9s    Third check fails (fall 3)
        - pg1 marked DOWN
        - on-marked-down shutdown-sessions triggers
        - Existing connections terminated

t=10s   No backends for postgres_primary
        - New connections queued
        - "503 No server available"

t=30s   Patroni promotes pg2
        - pg2 becomes primary
        - pg2:8008/primary returns 200

t=33s   HAProxy health check succeeds (rise 2)
t=36s   pg2 marked UP

t=37s   postgres_primary has backend
        - Queued connections routed to pg2
        - New connections succeed

Total unavailability: ~27 seconds
        (9s detection + 21s Patroni failover + health check)

Impact:
        - Active transactions lost
        - Connections terminated
        - Clients must retry
```

### Scenario 2: HAProxy Failover (Keepalived)

```
Timeline: Active HAProxy crashes
─────────────────────────────────────────────────────────────────

HAProxy1 (MASTER, VIP holder), HAProxy2 (BACKUP), HAProxy3 (BACKUP)

t=0s    HAProxy1 process crashes

t=1s    Keepalived check_haproxy fails
        - killall -0 haproxy returns 1

t=3s    Multiple failures (fall 3)
        - HAProxy1 marked unavailable

t=4s    VRRP election
        - HAProxy2 higher priority than HAProxy3
        - HAProxy2 becomes MASTER

t=5s    VIP moves to HAProxy2
        - Gratuitous ARP sent
        - Network learns new MAC

t=7s    Clients reconnect
        - TCP connections were lost
        - Applications retry to VIP
        - HAProxy2 serves traffic

Total HAProxy failover: ~7 seconds
Connection impact: All existing connections lost
```

### Scenario 3: Network Partition

```
Timeline: Network isolates primary
─────────────────────────────────────────────────────────────────

        pg1 (Primary)        │      HAProxy + pg2, pg3
              │              │           │
              │   partition  │           │
              │              │           │

HAProxy perspective:
─────────────────────────────────────────────────────────────────

t=0s    Partition occurs

t=3s    HAProxy cannot reach pg1
        - Health checks fail

t=9s    pg1 marked DOWN
        - No primary available

t=10s   Patroni (on pg2/pg3 side) detects pg1 missing
        - Has DCS quorum
        - Can promote

t=30s   Patroni promotes pg2

t=33s   HAProxy detects pg2 as primary
        - Routes to pg2

pg1 perspective:
─────────────────────────────────────────────────────────────────

t=0s    Partition occurs
        - Cannot reach DCS (etcd/Consul)

t=30s   Patroni on pg1 loses DCS connectivity
        - TTL expires
        - Demotes pg1 to read-only

Result: No split brain
        HAProxy routes to accessible primary
        Isolated primary fenced by Patroni
```

### Scenario 4: All Backend Failure

```
Timeline: All PostgreSQL nodes fail
─────────────────────────────────────────────────────────────────

t=0s    Cascading failures
        - pg1 fails (OOM)
        - pg2 fails (disk full)
        - pg3 fails (network)

t=9s    All backends marked DOWN
        - postgres_primary: no backends
        - postgres_replicas: no backends

t=10s   HAProxy behavior:
        - Queues connections (default)
        - Or returns 503 immediately (option redispatch disabled)

        Client sees:
        - Connection timeout
        - "No server available" in logs

t=???   Recovery requires:
        - Fix underlying issues
        - Restart PostgreSQL
        - Backends auto-detected by health checks

Impact: Complete outage
        HAProxy cannot help without backends
```

### Scenario 5: Flapping Backend

```
Timeline: Unstable backend
─────────────────────────────────────────────────────────────────

t=0s    pg2 becomes unstable
        - Health check passes
        - Query execution fails

t=3s    Health check passes (UP)
t=6s    Health check fails (DOWN)
t=9s    Health check passes (UP)
t=12s   Health check fails (DOWN)
        ... continues flapping

Impact:
        - Connections routed to pg2 fail
        - User experience degraded
        - Logs flooded with state changes

Mitigation:
        rise 2, fall 3:
        - Need 2 successes to mark UP
        - Need 3 failures to mark DOWN
        - Reduces flapping impact

        slowstart 60s:
        - Gradually increase traffic to recovered node
        - Prevents thundering herd
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Proven Technology** | HAProxy is battle-tested at scale |
| **Flexibility** | Works with any failover manager |
| **Performance** | Minimal latency overhead |
| **Observability** | Rich stats and monitoring |
| **Connection Handling** | Graceful draining, queuing |
| **Read Scaling** | Easy read replica distribution |
| **Protocol Agnostic** | Works with any PostgreSQL client |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Not PostgreSQL-Aware** | No query routing intelligence |
| **Extra Component** | Another system to manage |
| **Health Check Dependency** | Relies on external endpoint |
| **No Failover Control** | Only routes, doesn't promote |
| **Connection Pooling** | Separate PgBouncer needed |
| **Session Awareness** | Limited without application changes |

## Key Configuration Parameters

### Health Check Tuning

```haproxy
# Fast detection, risk of false positives
default-server inter 1s fall 2 rise 1

# Conservative, slower detection
default-server inter 5s fall 3 rise 2

# Recommended balance
default-server inter 3s fall 3 rise 2

# inter: Check interval
# fall: Consecutive failures to mark DOWN
# rise: Consecutive successes to mark UP
```

### Connection Handling

```haproxy
# Terminate connections immediately on backend down
on-marked-down shutdown-sessions

# Graceful connection draining on maintenance
slowstart 60s  # Ramp up traffic over 60s

# Connection limits
maxconn 1000           # Total HAProxy connections
default-server maxconn 100  # Per backend
```

## Comparison with Alternatives

| Feature | HAProxy | Pgpool-II | PgBouncer |
|---------|---------|-----------|-----------|
| Load Balancing | TCP/HTTP | Query-aware | No |
| Connection Pooling | No | Yes | Yes |
| Read/Write Split | Backend-based | Query-based | No |
| Query Caching | No | Yes | No |
| Failover | Health check only | Script-based | No |
| Performance | Excellent | Good | Excellent |
| Complexity | Low | High | Low |

## Best Practices

1. **Use Patroni REST API**: Most reliable health check method
2. **Separate Ports**: Different ports for RW and RO traffic
3. **Configure Keepalived**: Make HAProxy itself highly available
4. **Tune Health Checks**: Balance speed vs stability
5. **Enable Stats**: Monitor via stats page
6. **Use on-marked-down**: Clean up connections on failure
7. **Add PgBouncer**: For connection pooling needs
8. **Log Everything**: Essential for debugging

## Limitations

1. **Not a Failover Manager**: HAProxy doesn't promote standbys
2. **No Query Routing**: Cannot route by query type
3. **Connection Pooling**: Requires separate solution
4. **Health Check Latency**: Adds to failover detection time
5. **VIP Dependency**: Needs Keepalived for HAProxy HA
6. **No Read-Your-Writes**: Cannot ensure consistency
7. **TCP Mode Limitations**: No HTTP features for PostgreSQL

## Conclusion

HAProxy with PostgreSQL streaming replication is a flexible, proven pattern for routing PostgreSQL traffic. HAProxy excels at load balancing and health checking but requires a separate failover manager (Patroni, repmgr, etc.) for actual PostgreSQL promotion. The combination provides excellent observability and control over connection routing.

**Recommended for:**
- Organizations already using HAProxy
- Deployments needing read scaling
- Teams wanting separation of concerns (routing vs failover)
- Environments requiring detailed traffic control

**Not recommended for:**
- Simple deployments (consider pg_auto_failover)
- Query-level routing needs (consider Pgpool-II)
- Teams wanting all-in-one solution
