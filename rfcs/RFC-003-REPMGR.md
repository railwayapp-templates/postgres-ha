# RFC-003: repmgr - PostgreSQL Replication Manager

## Overview

repmgr is a lightweight open-source tool suite for managing PostgreSQL replication and failover. Developed by EDB (formerly 2ndQuadrant), it provides a simple approach to HA by building on PostgreSQL's native streaming replication. Unlike Patroni or Stolon, repmgr does not require an external DCS; it stores cluster metadata in PostgreSQL itself.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│           Load Balancer / pgBouncer / HAProxy                   │
│              (External - not part of repmgr)                   │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│    Node 1     │     │    Node 2     │     │    Node 3     │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │ repmgrd │  │     │  │ repmgrd │  │     │  │ repmgrd │  │
│  │ (daemon)│  │     │  │ (daemon)│  │     │  │ (daemon)│  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │PostgreSQL│ │     │  │PostgreSQL│ │     │  │PostgreSQL│ │
│  │(Primary) │ │     │  │(Standby) │ │     │  │(Standby) │ │
│  │         │◀├─────WAL Streaming────┤─────┤▶│          │ │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
│               │     │               │     │               │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │repmgr   │  │     │  │repmgr   │  │     │  │repmgr   │  │
│  │metadata │  │     │  │metadata │  │     │  │metadata │  │
│  │(tables) │  │     │  │(replica)│  │     │  │(replica)│  │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
└───────────────┘     └───────────────┘     └───────────────┘
        │                     │                     │
        └─────────────────────┴─────────────────────┘
                    SSH (optional, for pg_rewind/clone)
```

## Core Components

### 1. repmgr CLI Tool
- `repmgr primary register` - Register primary node
- `repmgr standby clone` - Clone standby from primary
- `repmgr standby register` - Register standby node
- `repmgr standby promote` - Promote standby to primary
- `repmgr standby follow` - Reconfigure standby to follow new primary
- `repmgr standby switchover` - Planned switchover
- `repmgr cluster show` - Display cluster status
- `repmgr node check` - Check node health

### 2. repmgrd Daemon
- Monitors PostgreSQL and other nodes
- Performs automatic failover when enabled
- Handles notifications and event logging
- Executes user-defined scripts on events

### 3. repmgr Metadata
- Stored in `repmgr` schema in PostgreSQL
- `repmgr.nodes` - Node information
- `repmgr.events` - Cluster event log
- Replicated to standbys via streaming replication

## How It Works

### Cluster Metadata Storage

```
Unlike Patroni/Stolon:
─────────────────────────────────────────────────────────────────

Patroni/Stolon:     External DCS (etcd/Consul/ZooKeeper)
                    ↕ All nodes read/write cluster state

repmgr:             PostgreSQL database itself
                    ↕ Metadata replicated with data
                    ↕ Each node has local copy

Implications:
- No external dependency
- Simpler deployment
- But: consensus during partition is harder
```

### Node Discovery and Communication

```
Node Communication:
─────────────────────────────────────────────────────────────────

1. Each repmgrd connects to local PostgreSQL
2. Reads node list from repmgr.nodes table
3. Establishes connections to other nodes' PostgreSQL
4. Monitors via pg_stat_replication and connection checks

No gossip protocol - direct PostgreSQL connections
```

### Automatic Failover Process

```
Primary Failure Detection:
─────────────────────────────────────────────────────────────────

        Primary                Standby 1            Standby 2
           │                       │                    │
     [CRASHES]                     │                    │
           X                       │                    │
                                   │                    │
t=0s                        detect connection          detect
                            failure to primary         failure
                                   │                    │
t=5s                        retry attempts (reconnect_attempts)
                                   │                    │
t=30s                       exceed reconnect_interval * reconnect_attempts
                                   │                    │
t=31s                       begin election             begin election
                                   │                    │
                            ┌──────┴──────┐
                            │   VOTING    │
                            └──────┬──────┘
                                   │
                            Node with highest
                            priority + least lag
                                   │
                            ┌──────▼──────┐
                            │  PROMOTED   │
                            └─────────────┘
```

### Election Mechanism

```
Election Process (without external DCS):
─────────────────────────────────────────────────────────────────

1. Each standby independently detects primary failure
2. Each standby evaluates its eligibility:
   - Check own replication lag
   - Check visibility of other standbys
   - Check own priority setting

3. Location-based voting (if configured):
   - Nodes in same location can see each other
   - Location with majority can proceed

4. Promotion decision:
   - Highest priority wins
   - If tie, lowest replication lag wins
   - If tie, lowest node_id wins

5. Winner promotes, losers follow new primary
```

## Configuration

### repmgr.conf (Primary)

```ini
node_id=1
node_name='node1'
conninfo='host=node1 dbname=repmgr user=repmgr connect_timeout=2'
data_directory='/var/lib/postgresql/16/main'

# Replication settings
use_replication_slots=true
pg_bindir='/usr/lib/postgresql/16/bin'

# Monitoring
monitor_interval_secs=2
connection_check_type=ping
reconnect_attempts=6
reconnect_interval=10

# Failover
failover=automatic
promote_command='/usr/bin/repmgr standby promote -f /etc/repmgr.conf --log-to-file'
follow_command='/usr/bin/repmgr standby follow -f /etc/repmgr.conf --log-to-file --upstream-node-id=%n'

# Notifications
event_notification_command='/usr/local/bin/notify.sh %n %e %s "%t" "%d"'
event_notifications=standby_promote,standby_follow,repmgrd_failover_promote

# Logging
log_file='/var/log/repmgr/repmgr.log'
log_level=INFO
```

### repmgr.conf (Standby)

```ini
node_id=2
node_name='node2'
conninfo='host=node2 dbname=repmgr user=repmgr connect_timeout=2'
data_directory='/var/lib/postgresql/16/main'

use_replication_slots=true
pg_bindir='/usr/lib/postgresql/16/bin'

# Standby specific
primary_follow_timeout=60
standby_disconnect_on_failover=true

# Same failover settings as primary
failover=automatic
promote_command='/usr/bin/repmgr standby promote -f /etc/repmgr.conf --log-to-file'
follow_command='/usr/bin/repmgr standby follow -f /etc/repmgr.conf --log-to-file --upstream-node-id=%n'

# Priority (higher = more likely to be promoted)
priority=100
```

### PostgreSQL Configuration Required

```ini
# postgresql.conf
wal_level = replica
max_wal_senders = 10
max_replication_slots = 10
hot_standby = on
archive_mode = on
archive_command = '/bin/true'  # Or real archiving

# Recommended for repmgr
shared_preload_libraries = 'repmgr'
```

### pg_hba.conf

```
# repmgr connections
host    repmgr          repmgr      192.168.1.0/24    scram-sha-256
host    replication     repmgr      192.168.1.0/24    scram-sha-256
```

## Happy Path Scenarios

### Scenario 1: Cluster Initialization

```bash
# On Primary (node1)
$ repmgr primary register
INFO: connecting to primary database...
INFO: creating repmgr extension
NOTICE: primary node "node1" successfully registered

# On Standby (node2)
$ repmgr -h node1 -U repmgr -d repmgr standby clone
NOTICE: destination directory "/var/lib/postgresql/16/main" provided
INFO: connecting to source node
INFO: checking and correcting permissions on existing directory
INFO: starting backup (using pg_basebackup)...
NOTICE: standby clone complete

$ pg_ctl start
$ repmgr standby register
NOTICE: standby node "node2" successfully registered

# Check cluster
$ repmgr cluster show
 ID | Name  | Role    | Status    | Upstream | Location | Priority
----+-------+---------+-----------+----------+----------+----------
 1  | node1 | primary | * running |          | default  | 100
 2  | node2 | standby |   running | node1    | default  | 100
```

### Scenario 2: Planned Switchover

```
Timeline: Graceful switchover from node1 to node2
─────────────────────────────────────────────────────────────────

$ repmgr standby switchover -f /etc/repmgr.conf --siblings-follow

t=0s    Switchover initiated
        - Check node2 is healthy standby
        - Check node2 replication lag acceptable

t=1s    Prepare primary (node1)
        - Checkpoint
        - Pause replication

t=2s    Wait for node2 to catch up
        - Replay all pending WAL

t=3s    Demote node1
        - pg_ctl stop or pg_ctl promote --dry-run

t=4s    Promote node2
        - pg_promote() called
        - New timeline created

t=5s    Reconfigure node1 as standby
        - Point to node2
        - Start streaming

t=6s    Update other standbys
        - --siblings-follow triggers follow_command

Total time: ~6-10 seconds
Zero data loss (synchronous)
```

### Scenario 3: Normal Monitoring

```
Timeline: Steady state operation
─────────────────────────────────────────────────────────────────

repmgrd daemon loop (every monitor_interval_secs):

t=0s    Check local PostgreSQL
        - Connection alive?
        - Recovery status?
        - Replication lag?

t=0s    Check primary connectivity (if standby)
        - Direct connection to primary
        - pg_stat_replication check

t=0s    Update monitoring data
        - Log status
        - Trigger event notifications if changed

t=2s    Repeat...
```

## Unhappy Path Scenarios

### Scenario 1: Primary Failure - Automatic Failover

```
Timeline: Unexpected primary crash
─────────────────────────────────────────────────────────────────

t=0s    Primary (node1) crashes
        - PostgreSQL process terminates
        - repmgrd on node1 also fails (or loses DB)

t=2s    Standby repmgrd detects connection failure
        - Cannot connect to primary
        - Begins reconnect_attempts

t=2s    reconnect_attempt 1 fails
t=12s   reconnect_attempt 2 fails
t=22s   reconnect_attempt 3 fails
t=32s   reconnect_attempt 4 fails
t=42s   reconnect_attempt 5 fails
t=52s   reconnect_attempt 6 fails

t=62s   All reconnect_attempts exhausted
        - Primary declared failed
        - Election begins

t=63s   Node2 evaluates election:
        - Can reach other standbys? (if any)
        - Own priority: 100
        - Own lag: 0 bytes

t=64s   Node2 wins election
        - Executes promote_command
        - PostgreSQL promoted

t=65s   Node3 (if exists) executes follow_command
        - Reconfigures to follow node2

t=70s   Event notifications sent
        - notify.sh called with event details

Total failover time: ~65-70 seconds (6 attempts × 10s interval)
Potential data loss: Uncommitted + unreplicated transactions
```

### Scenario 2: Network Partition - Split Brain Risk

```
Timeline: Network splits cluster
─────────────────────────────────────────────────────────────────

WITHOUT proper configuration (DANGEROUS):

         DC1              │           DC2
    ┌──────────┐          │      ┌──────────┐
    │  node1   │          │      │  node2   │
    │ (primary)│          │      │ (standby)│
    └──────────┘          │      └──────────┘
         │                │           │
    partition             │      partition

t=0s    Network partition occurs

t=60s   node2 cannot reach node1
        - Exhausts reconnect_attempts
        - Decides to promote itself
        - No quorum check!

t=61s   SPLIT BRAIN!
        - node1 still running as primary
        - node2 promoted as primary
        - Both accepting writes

─────────────────────────────────────────────────────────────────

WITH proper configuration (location + witness):

         DC1                    │           DC2
    ┌──────────┐                │      ┌──────────┐
    │  node1   │                │      │  node2   │
    │ (primary)│                │      │ (standby)│
    │ location=dc1              │      │ location=dc2
    └──────────┘                │      └──────────┘
         │                      │           │
    ┌──────────┐                │           │
    │ witness  │                │           │
    │ location=dc1              │           │
    └──────────┘                │           │
                                │
t=0s    Network partition

t=60s   node2 cannot reach node1
        - Cannot reach witness either
        - Only node in its location
        - location_check fails
        - Does NOT promote

Result: No split brain, but DC2 unavailable
        Manual intervention needed
```

### Scenario 3: Cascading Failure

```
Timeline: Multiple failures
─────────────────────────────────────────────────────────────────

Cluster: node1 (primary) → node2 (standby) → node3 (cascaded)

t=0s    node1 fails
        - node2 promotes automatically
        - node3 tries to follow new primary

t=65s   node2 now primary
        - node3 must reconfigure upstream

t=66s   node3 follow_command runs
        - Repoints to node2

But if node3 was far behind:
        - May need pg_rewind
        - Or full reclone

t=120s  node3 rejoins as standby
        - Cluster operational with 2 nodes
```

### Scenario 4: repmgrd Daemon Failure

```
Timeline: Monitoring daemon crashes
─────────────────────────────────────────────────────────────────

t=0s    repmgrd on node2 crashes
        - Node2 PostgreSQL still running
        - Still streaming from primary

t=60s   Primary fails
        - node2 doesn't detect (no repmgrd)
        - node3 repmgrd detects, promotes

t=65s   node3 is new primary
        - node2 still replicating from dead node1
        - Eventually connection fails

Manual intervention:
        $ repmgr standby follow --upstream-node-id=3

Impact: Unmonitored node misses failover
        Data divergence possible
```

### Scenario 5: All Standbys Unavailable

```
Timeline: Primary alone
─────────────────────────────────────────────────────────────────

t=0s    Cluster: node1 (primary), node2 (standby), node3 (standby)

t=10s   node2 fails
t=20s   node3 fails

t=30s   Primary (node1) running alone
        - No failover capability
        - Single point of failure
        - Accepts writes (no quorum required)

t=60s   node1 fails
        - Complete cluster failure
        - No automatic recovery possible

Impact: No availability once primary fails
        Manual recovery from backups required
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Simplicity** | No external DCS required |
| **Lightweight** | Minimal resource overhead |
| **PostgreSQL Native** | Works with standard streaming replication |
| **Mature** | Long history, well-documented |
| **EDB Support** | Commercial support available |
| **Flexible** | Works on VMs, bare metal, containers |
| **Event System** | Rich notification and scripting hooks |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Split Brain Risk** | No true consensus without witness nodes |
| **Slower Failover** | 60+ seconds with default settings |
| **Manual Integration** | Load balancer not included |
| **Witness Complexity** | Witness nodes needed for partition safety |
| **Election Algorithm** | Simpler than Raft, more edge cases |
| **No Read Routing** | Must use external tools |

## Key Configuration Parameters

### Timing Parameters

```ini
# Detection speed
monitor_interval_secs=2         # How often to check (2-5s typical)
connection_check_type=ping      # ping, connection, or query

# Failover timing
reconnect_attempts=6            # Attempts before failover
reconnect_interval=10           # Seconds between attempts
# Total detection time = reconnect_attempts × reconnect_interval
# Default: 6 × 10 = 60 seconds

# Aggressive settings (faster failover, more false positives)
reconnect_attempts=3
reconnect_interval=5
# Detection time: 15 seconds
```

### Failover Safety Parameters

```ini
# Prevent promotion if lagging
standby_disconnect_on_failover=true  # Disconnect from failed primary first

# Location-based voting
location='dc1'                  # Must match across DC

# Priority
priority=100                    # Higher = preferred for promotion
priority=0                      # Never promote this node

# Degraded monitoring
degraded_monitoring_timeout=60  # Seconds before entering degraded mode
```

## Witness Nodes

### Purpose

```
Witness Node:
─────────────────────────────────────────────────────────────────

- PostgreSQL instance with only repmgr metadata
- No actual data replication
- Provides vote in partition scenarios
- Allows determination of "majority"

Deployment:
- Place witness in location with primary
- Or in third location if available
```

### Configuration

```bash
# Create witness
$ createdb -h witness_host -p 5432 repmgr

# Register witness
$ repmgr witness register -h primary_host
```

## Comparison with Alternatives

| Feature | repmgr | Patroni | pg_auto_failover |
|---------|--------|---------|------------------|
| External DCS | No | Yes | No (monitor) |
| Failover Time | 60s+ | 30s | 20s |
| Split Brain Safety | Witness needed | DCS consensus | Monitor consensus |
| Complexity | Low | Medium | Low |
| Connection Pooling | External | External | Built-in |
| Read Routing | No | HAProxy | No |
| Kubernetes Native | No | Yes | Limited |

## Limitations

1. **No External Consensus**: Relies on PostgreSQL connectivity for decisions
2. **Witness Requirement**: Need witness nodes for partition safety
3. **Manual Load Balancing**: No built-in connection routing
4. **Slow Detection**: Default 60-second failover time
5. **Aggressive Settings Risky**: Faster detection = more false positives
6. **No Built-in Fencing**: Must implement via scripts
7. **Complex Multi-DC**: Requires careful location configuration
8. **No Quorum Concept**: Majority based on visible nodes only

## Best Practices

1. **Always Use Witness Nodes**: In 2+ datacenter deployments
2. **Configure Locations**: Properly label nodes by datacenter
3. **Implement Fencing**: Use event notifications to fence old primary
4. **Monitor repmgrd**: Alert if daemon fails
5. **Test Failover**: Regular failover drills
6. **Use Priority**: Set clear promotion preferences
7. **Log Everything**: Enable detailed logging for debugging
8. **Backup Regularly**: repmgr is not a backup solution

## Conclusion

repmgr is a simple, lightweight PostgreSQL HA solution ideal for teams wanting to avoid external dependencies like etcd. Its main advantage is simplicity—no separate consensus system to manage. However, this simplicity comes with tradeoffs: proper split-brain prevention requires witness nodes, and the election algorithm is less robust than true consensus protocols.

**Recommended for:**
- Simpler deployments without Kubernetes
- Teams wanting to avoid external DCS
- Traditional VM/bare metal infrastructure
- Organizations with EDB support contracts

**Not recommended for:**
- Kubernetes-native deployments
- Sub-30-second failover requirements
- Complex multi-region setups
- Teams without HA expertise
