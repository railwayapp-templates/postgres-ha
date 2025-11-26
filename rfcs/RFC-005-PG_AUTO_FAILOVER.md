# RFC-005: pg_auto_failover - PostgreSQL Automatic Failover

## Overview

pg_auto_failover is an open-source extension developed by Citus Data (now Microsoft) for automatic failover and high availability. It uses a novel approach with a dedicated monitor node that implements a finite state machine (FSM) to coordinate failover. Unlike solutions requiring etcd/Consul, pg_auto_failover uses a PostgreSQL-based monitor as its consensus system.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ Connection string with
                              │ multiple hosts + target_session_attrs
                              ▼
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│    Node 1     │     │    Node 2     │     │    Monitor    │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │pg_auto- │  │     │  │pg_auto- │  │     │  │pg_auto- │  │
│  │failover │  │     │  │failover │  │     │  │failover │  │
│  │keeper   │  │     │  │keeper   │  │     │  │monitor  │  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │PostgreSQL│ │     │  │PostgreSQL│ │     │  │PostgreSQL│ │
│  │(Primary) │ │     │  │(Secondary)│ │     │  │(Monitor) │ │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
│               │     │               │     │  Stores FSM  │
│               │     │               │     │  state       │
└───────────────┘     └───────────────┘     └───────────────┘
        │                     │                     │
        │                     │                     │
        └─────── Report state ┴─── Get transitions ─┘
                          to Monitor
```

## Core Components

### 1. pg_autoctl (Keeper)
- Runs alongside each PostgreSQL data node
- Reports node state to monitor
- Receives state transitions from monitor
- Executes PostgreSQL commands (promote, demote, follow)
- Manages local PostgreSQL lifecycle

### 2. Monitor Node
- Dedicated PostgreSQL instance
- Stores cluster state in tables
- Implements FSM for state transitions
- Coordinates failover decisions
- No data replication (metadata only)

### 3. Finite State Machine (FSM)
- Defines all valid node states
- Specifies valid state transitions
- Ensures consistent cluster behavior
- Prevents invalid configurations

## How It Works

### Finite State Machine States

```
                    ┌─────────────────┐
                    │   INIT_STATE    │
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              ▼              │              ▼
       ┌─────────────┐       │       ┌─────────────┐
       │   SINGLE    │       │       │   WAIT_     │
       │             │       │       │   PRIMARY   │
       └──────┬──────┘       │       └──────┬──────┘
              │              │              │
              │              │              │
              ▼              ▼              ▼
       ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
       │   PRIMARY   │ │ CATCHINGUP  │ │  SECONDARY  │
       │             │ │             │ │             │
       └──────┬──────┘ └──────┬──────┘ └──────┬──────┘
              │              │              │
              │              │              │
              ▼              ▼              ▼
       ┌─────────────────────────────────────────────┐
       │         DRAINING / DEMOTE_TIMEOUT           │
       │      PREPARE_PROMOTION / STOP_REPLICATION   │
       │              WAIT_PRIMARY                    │
       └─────────────────────────────────────────────┘

Key States:
- SINGLE: Only node, no HA
- PRIMARY: Active primary, has secondary
- SECONDARY: Streaming from primary
- CATCHINGUP: Replica catching up
- DRAINING: Primary draining connections
- DEMOTE_TIMEOUT: Waiting for old primary
- PREPARE_PROMOTION: Secondary preparing
- STOP_REPLICATION: Stopping replication before promote
- WAIT_PRIMARY: Secondary waiting to become primary
```

### State Transition Example: Normal Failover

```
Primary Failure State Transitions:
─────────────────────────────────────────────────────────────────

Primary (Node1)          Secondary (Node2)        Monitor
     │                         │                      │
  PRIMARY                  SECONDARY                  │
     │                         │                      │
  [CRASH]                      │                      │
     X                         │                      │
     │                         │                      │
     │                    reports state               │
     │                    ─────────────────────────►  │
     │                         │                      │
     │                         │              detects primary
     │                         │              unhealthy
     │                         │                      │
     │                         │              assigns:
     │                         │              STOP_REPLICATION
     │                    ◄─────────────────────────  │
     │                         │                      │
     │                  stops replication             │
     │                  reports done                  │
     │                    ─────────────────────────►  │
     │                         │                      │
     │                         │              assigns:
     │                         │              WAIT_PRIMARY
     │                    ◄─────────────────────────  │
     │                         │                      │
     │                  promotes                      │
     │                  pg_promote()                  │
     │                         │                      │
     │                  reports:                      │
     │                  PRIMARY                       │
     │                    ─────────────────────────►  │
     │                         │                      │
     │                         │              New primary
     │                         │              acknowledged
```

## Configuration

### Monitor Setup

```bash
# Create monitor
pg_autoctl create monitor \
    --pgdata /var/lib/postgresql/monitor \
    --pgport 5000 \
    --hostname monitor.example.com \
    --auth trust \
    --ssl-mode require \
    --ssl-ca-file /etc/ssl/certs/ca.crt \
    --ssl-server-cert /etc/ssl/certs/server.crt \
    --ssl-server-key /etc/ssl/private/server.key

# Start monitor
pg_autoctl run

# Or as systemd service
pg_autoctl -q show systemd --pgdata /var/lib/postgresql/monitor | \
    sudo tee /etc/systemd/system/pgautofailover-monitor.service
sudo systemctl enable --now pgautofailover-monitor
```

### Primary Node Setup

```bash
# Create primary
pg_autoctl create postgres \
    --pgdata /var/lib/postgresql/data \
    --pgport 5432 \
    --hostname node1.example.com \
    --name node1 \
    --formation default \
    --monitor 'postgres://autoctl_node@monitor.example.com:5000/pg_auto_failover?sslmode=require' \
    --auth scram-sha-256 \
    --ssl-mode require

# Start keeper
pg_autoctl run
```

### Secondary Node Setup

```bash
# Create secondary (auto-clones from primary)
pg_autoctl create postgres \
    --pgdata /var/lib/postgresql/data \
    --pgport 5432 \
    --hostname node2.example.com \
    --name node2 \
    --formation default \
    --monitor 'postgres://autoctl_node@monitor.example.com:5000/pg_auto_failover?sslmode=require' \
    --auth scram-sha-256 \
    --ssl-mode require

# Automatically discovers primary from monitor
# Runs pg_basebackup
# Configures streaming replication
# Starts keeper
pg_autoctl run
```

### Client Connection String

```
# Multi-host connection string with automatic primary detection
postgresql://user:pass@node1:5432,node2:5432/mydb?target_session_attrs=read-write

# libpq handles routing:
# - Tries each host in order
# - Uses target_session_attrs to find primary
# - Automatic reconnection on failover
```

### Formation and Group Configuration

```bash
# Show current state
pg_autoctl show state

# Example output:
#  Name |  Node |      Host:Port |       TLI: LSN |   Connection |      Reported State |      Assigned State
# ------+-------+----------------+----------------+--------------+---------------------+--------------------
# node1 |     1 | node1:5432     |   1: 0/3000148 | read-write   | primary             | primary
# node2 |     2 | node2:5432     |   1: 0/3000148 | read-only    | secondary           | secondary

# Configure replication settings
pg_autoctl set formation number-sync-standbys 1
pg_autoctl set formation replication-quorum true
```

## Happy Path Scenarios

### Scenario 1: Cluster Initialization

```
Timeline: Setting up a new HA cluster
─────────────────────────────────────────────────────────────────

t=0s    Create monitor
        $ pg_autoctl create monitor ...
        - New PostgreSQL instance created
        - pg_auto_failover extension installed
        - Monitor tables initialized

t=5s    Create primary node
        $ pg_autoctl create postgres ... (node1)
        - Connects to monitor
        - Registers as first node
        - State: SINGLE (no HA yet)
        - PostgreSQL initialized

t=10s   Create secondary node
        $ pg_autoctl create postgres ... (node2)
        - Connects to monitor
        - Discovers node1 as primary
        - State: INIT

t=15s   Secondary clones from primary
        - pg_basebackup runs automatically
        - State: CATCHINGUP

t=60s   Secondary caught up
        - Replication lag near zero
        - State: SECONDARY
        - Node1 state: PRIMARY (was SINGLE)

t=65s   Cluster operational
        - Both nodes report healthy
        - HA enabled

$ pg_autoctl show state
 Name |  Node |  Host:Port  |   LSN    | Reported | Assigned
------+-------+-------------+----------+----------+-----------
node1 |   1   | node1:5432  | 0/5000  | primary  | primary
node2 |   2   | node2:5432  | 0/5000  | secondary| secondary
```

### Scenario 2: Planned Switchover

```
Timeline: Manual failover
─────────────────────────────────────────────────────────────────

$ pg_autoctl perform switchover

t=0s    Switchover initiated
        Monitor assigns:
        - node1: DRAINING (stop new connections)
        - node2: PREPARE_PROMOTION

t=1s    node1 drains connections
        - Checkpoint
        - Wait for transactions
        - Reports: ready for demotion

t=3s    Monitor assigns:
        - node1: DEMOTE_TIMEOUT
        - node2: STOP_REPLICATION

t=4s    node2 stops replication
        - Ensures all WAL received
        - Reports: ready for promotion

t=5s    Monitor assigns:
        - node2: WAIT_PRIMARY

t=6s    node2 promotes
        - pg_promote() called
        - Now accepting writes

t=7s    Monitor updates state:
        - node2: PRIMARY
        - node1: waits for new role

t=8s    node1 reconfigures
        - Points to node2
        - State: SECONDARY

t=10s   Switchover complete
        - Zero data loss
        - ~10 seconds total

$ pg_autoctl show state
 Name | Reported  | Assigned
------+-----------+-----------
node1 | secondary | secondary
node2 | primary   | primary
```

### Scenario 3: Adding a Third Node

```
Timeline: Scaling out the cluster
─────────────────────────────────────────────────────────────────

Existing: node1 (primary), node2 (secondary)

t=0s    Create node3
        $ pg_autoctl create postgres ... --name node3

t=5s    node3 registers with monitor
        - Monitor assigns: INIT
        - Discovers node1 as primary

t=10s   node3 clones from primary
        - pg_basebackup from node1
        - State: CATCHINGUP

t=60s   node3 caught up
        - State: SECONDARY

t=65s   Cluster has 3 nodes
        - 1 primary
        - 2 secondaries
        - Higher availability

Note: pg_auto_failover supports multi-standby configurations
      with number-sync-standbys for synchronous replication
```

## Unhappy Path Scenarios

### Scenario 1: Primary Node Failure

```
Timeline: Unexpected primary crash
─────────────────────────────────────────────────────────────────

t=0s    Primary (node1) crashes

t=5s    Monitor health check fails
        - Cannot connect to node1
        - health_check_period = 5s (default)

t=10s   Second health check fails
        - health_check_max_retries = 2

t=15s   Monitor declares node1 unhealthy
        - Timeout after health_check_timeout

t=16s   Monitor initiates failover
        - node2 assigned: STOP_REPLICATION

t=17s   node2 stops replication
        - Confirms no more WAL incoming
        - Reports ready

t=18s   Monitor assigns: WAIT_PRIMARY
        - node2 can promote

t=19s   node2 promotes
        - pg_promote() executes
        - New primary

t=20s   node2 reports: PRIMARY
        - Accepting writes

t=21s   Monitor updates cluster state
        - node1: DEMOTED (or UNREACHABLE)
        - node2: PRIMARY

Total failover time: ~20-25 seconds

When node1 recovers:
        - Detects it was primary
        - Uses pg_rewind to rejoin
        - Becomes SECONDARY
```

### Scenario 2: Network Partition

```
Timeline: Network isolates primary
─────────────────────────────────────────────────────────────────

         node1 (Primary)     │     node2 (Secondary)
              │              │           │
              │              │           │
              │   partition  │           │
              │              │      ┌────┴────┐
              │              │      │ Monitor │
              │              │      └─────────┘

t=0s    Partition occurs
        - node1 cannot reach monitor
        - node2 + monitor can communicate

t=5s    Monitor cannot reach node1
        - Health checks fail

t=15s   Monitor declares node1 unhealthy
        - node1 still thinks it's primary
        - But cannot confirm with monitor

t=16s   node1 keeper detects monitor loss
        - Cannot update state
        - Enters degraded mode

t=17s   node1 demotes itself (FENCING)
        - pg_auto_failover keeper prevents split brain
        - Node goes read-only without monitor confirmation

t=18s   node2 promotes
        - Monitor coordinates
        - New primary

Result: No split brain
        Primary self-fences when losing monitor contact

Key: pg_auto_failover uses monitor as consensus
     Node without monitor contact cannot remain primary
```

### Scenario 3: Monitor Failure

```
Timeline: Monitor node crashes
─────────────────────────────────────────────────────────────────

t=0s    Monitor crashes
        - PostgreSQL on monitor stops

t=1s    Keepers detect monitor unavailable
        - Cannot report state
        - Cannot receive transitions

t=10s   Cluster enters "frozen" state
        - No state changes possible
        - Current primary continues serving
        - Current secondary continues streaming
        - NO automatic failover possible

t=60s   Primary crashes (worst case)
        - No monitor to coordinate
        - Secondary cannot promote
        - Manual intervention required

Recovery options:

Option A: Restart monitor
        - If data intact, keepers reconnect
        - Normal operation resumes

Option B: New monitor from backup
        - Restore monitor PostgreSQL
        - Keepers re-register
        - State reconciliation

Option C: Manual failover
        $ pg_autoctl perform failover --force
        - Dangerous, bypasses monitor

Impact: Extended outage if both monitor and primary fail
        Single point of failure = monitor
```

### Scenario 4: Secondary Too Far Behind

```
Timeline: Replication lag prevents safe failover
─────────────────────────────────────────────────────────────────

Cluster state:
        - node1: PRIMARY
        - node2: SECONDARY (1GB behind)

t=0s    Primary crashes

t=15s   Monitor initiates failover

t=16s   Monitor evaluates node2
        - replication_quorum = true
        - node2 is candidate

t=17s   Monitor checks WAL position
        - 1GB replication lag
        - Potential data loss

Behavior depends on settings:

synchronous_commit (if enabled):
        - Lag impossible
        - Zero data loss guaranteed
        - But: latency penalty

replication_quorum (if true):
        - Waits for sync replica
        - May block if no sync available

If async replication:
        - Failover proceeds
        - ~1GB of data loss
        - Monitor logs warning
```

### Scenario 5: Split Brain Prevention Detail

```
pg_auto_failover Split Brain Prevention:
─────────────────────────────────────────────────────────────────

The keeper uses a "self-fencing" approach:

1. Keeper cannot reach monitor
   - Starts timeout counter (monitor-timeout)

2. If timeout expires:
   - Keeper assumes network partition
   - Cannot verify it's still the rightful primary

3. Self-demotion:
   - Demotes PostgreSQL to read-only
   - Prevents writes without monitor consensus

4. Monitor perspective:
   - Sees primary as unavailable
   - Promotes secondary

5. When partition heals:
   - Old primary discovers it's demoted
   - Uses pg_rewind to rejoin as secondary

Key insight: Monitor is the consensus authority
             No node can be primary without monitor agreement
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Simplicity** | No etcd/Consul/ZooKeeper needed |
| **Fast Failover** | 15-25 seconds typical |
| **State Machine** | Clear, predictable behavior |
| **Self-Fencing** | Built-in split-brain prevention |
| **libpq Integration** | Native PostgreSQL client support |
| **pg_rewind** | Fast node recovery |
| **Citus Integration** | Works with Citus distributed PostgreSQL |
| **Single Binary** | pg_autoctl handles everything |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Monitor SPOF** | Single monitor is vulnerability |
| **2-Node Typical** | Designed for primary + 1 secondary |
| **Monitor HA Complex** | Making monitor HA adds complexity |
| **No Built-in VIP** | Client must handle multi-host |
| **Newer Project** | Less battle-tested than Patroni |
| **Limited Proxy** | No connection pooling |

## Monitor High Availability

### Monitor HA Options

```
Option 1: Secondary Monitor (experimental)
─────────────────────────────────────────────────────────────────

pg_autoctl create monitor --pgdata /var/lib/postgresql/monitor-secondary \
    --monitor 'postgres://monitor.example.com:5000/pg_auto_failover'

- Monitor replication (streaming)
- Manual promotion if primary monitor fails
- NOT automatic failover

Option 2: External HA for Monitor
─────────────────────────────────────────────────────────────────

Use Patroni/repmgr for the monitor PostgreSQL itself:
- Monitor becomes HA PostgreSQL cluster
- Adds operational complexity
- Redundant tooling

Option 3: Cloud Managed PostgreSQL as Monitor
─────────────────────────────────────────────────────────────────

Use RDS/Cloud SQL for monitor:
- Leverages cloud provider HA
- Low operational burden
- Adds external dependency
```

## Configuration Parameters

### Timing Parameters

```bash
# Health check settings (pg_autoctl set formation)
pg_autoctl set formation health_check_period 5
pg_autoctl set formation health_check_timeout 30
pg_autoctl set formation health_check_max_retries 2

# Node timeout
pg_autoctl set formation unhealthy_timeout 20

# Monitor timeout (keeper side)
# How long keeper waits before self-fencing
# Configured in pg_autoctl create
```

### Replication Settings

```bash
# Synchronous replication
pg_autoctl set formation number-sync-standbys 1

# Replication quorum (all syncs must confirm)
pg_autoctl set formation replication-quorum true

# These affect failover behavior and data safety
```

## Comparison with Alternatives

| Feature | pg_auto_failover | Patroni | repmgr |
|---------|------------------|---------|--------|
| Consensus | Monitor node | etcd/Consul/ZK | PostgreSQL |
| Complexity | Low | Medium | Low |
| Failover Time | 15-25s | 30-60s | 60s+ |
| Multi-Standby | Limited | Full | Full |
| Built-in Proxy | No | No (HAProxy) | No |
| pg_rewind | Yes | Yes | Optional |
| State Machine | Explicit FSM | Implicit | Implicit |
| Monitor SPOF | Yes | No (DCS HA) | No |

## Limitations

1. **Monitor Single Point of Failure**: Without monitor HA, cluster cannot failover if monitor fails
2. **Two-Node Focus**: Originally designed for primary + secondary pairs
3. **No Load Balancing**: Client must handle read/write routing
4. **No Connection Pooling**: External pgBouncer needed
5. **Limited Multi-DC**: Not designed for geo-distributed deployments
6. **Newer Ecosystem**: Fewer integrations than Patroni
7. **Client Requirements**: Needs libpq 10+ for multi-host connection strings

## Best Practices

1. **Implement Monitor HA**: Use secondary monitor or external HA
2. **Use Synchronous Mode**: For zero data loss requirements
3. **Configure Proper Timeouts**: Balance speed vs false positives
4. **Enable SSL**: Encrypt all connections including to monitor
5. **Monitor the Monitor**: Alert on monitor health
6. **Test Failover**: Regular drills in staging
7. **Use Multi-Host Connection Strings**: Enable automatic client reconnection
8. **Backup Monitor**: Include monitor in backup strategy

## Conclusion

pg_auto_failover provides a simpler alternative to Patroni by eliminating the external DCS requirement. The explicit finite state machine makes behavior predictable and debuggable. The main weakness is the monitor as a single point of failure, though this can be mitigated. It's particularly well-suited for organizations wanting PostgreSQL HA without the operational complexity of etcd/Consul clusters.

**Recommended for:**
- Teams wanting simpler HA without external DCS
- Two-node (primary + secondary) deployments
- Organizations using Citus
- Environments where monitor HA can be externalized

**Not recommended for:**
- Complex multi-node topologies
- Teams requiring built-in proxy/pooling
- Deployments without monitor HA strategy
- Multi-datacenter active-active requirements
