# RFC-002: Stolon - Cloud Native PostgreSQL High Availability

## Overview

Stolon is a cloud-native PostgreSQL HA manager developed by Sorint.lab. It provides automatic failover, synchronous replication support, and is designed specifically for containerized environments like Kubernetes. Stolon uses a three-component architecture: Keeper, Sentinel, and Proxy.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                      Stolon Proxies                             │
│              (Multiple instances for HA)                        │
│         Discovers master from DCS, routes connections           │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│   Keeper 1    │     │   Keeper 2    │     │   Keeper 3    │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │ Keeper  │  │     │  │ Keeper  │  │     │  │ Keeper  │  │
│  │ Process │  │     │  │ Process │  │     │  │ Process │  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │PostgreSQL│ │     │  │PostgreSQL│ │     │  │PostgreSQL│ │
│  │ (Master) │ │     │  │(Standby) │ │     │  │(Standby) │ │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
└───────────────┘     └───────────────┘     └───────────────┘
        │                     │                     │
        └─────────────────────┼─────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                         Sentinels                               │
│           (Multiple instances, discover keepers)                │
│        Perform leader election, manage cluster state            │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│              Distributed Configuration Store                    │
│                     (etcd / Consul)                            │
└─────────────────────────────────────────────────────────────────┘
```

## Core Components

### 1. Keeper
- Manages a PostgreSQL instance
- Reports status to the DCS
- Receives commands from Sentinels via DCS
- Handles PostgreSQL lifecycle (start, stop, promote, demote)
- Can reinitialize from another keeper using pg_basebackup

### 2. Sentinel
- Monitors keepers and cluster health
- Performs leader election among sentinels
- Elected sentinel leader makes cluster decisions
- Updates cluster view in DCS
- Multiple sentinels for HA (odd number recommended)

### 3. Proxy
- Stateless connection router
- Reads current master from DCS
- Routes all connections to current master
- Zero-configuration (discovers cluster from DCS)
- Can run multiple instances with load balancer

### 4. stolonctl
- CLI tool for cluster management
- Initialize, update configuration, failover
- Reads/writes to DCS

## How It Works

### Cluster State Management

```
                    DCS (etcd/Consul)
                          │
           ┌──────────────┼──────────────┐
           │              │              │
           ▼              ▼              ▼
    ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
    │ Cluster     │ │ Cluster     │ │ Proxy       │
    │ Data        │ │ View        │ │ View        │
    │ (config)    │ │ (state)     │ │ (master)    │
    └─────────────┘ └─────────────┘ └─────────────┘
          ▲               ▲               │
          │               │               │
    stolonctl       Sentinels         Proxies
    (write)         (write)           (read)

    Keepers
    (read cluster data, write keeper state)
```

### Data Flow

```
1. Keepers report their PostgreSQL state to DCS
2. Sentinels read keeper states, compute cluster view
3. Elected sentinel writes cluster view to DCS
4. Keepers read cluster view, execute required actions
5. Proxies read proxy view, route to current master
```

### Leader Election (Sentinel)

```
Sentinel Election Process:
─────────────────────────────────────────────────────────────────

t=0s    Multiple sentinels start
        - Each tries to acquire leadership lock in DCS

t=1s    One sentinel becomes leader
        - Holds exclusive lock with TTL
        - Other sentinels become followers

t=2s    Leader sentinel:
        - Reads all keeper states
        - Computes desired cluster state
        - Writes cluster view to DCS

t=10s   Leader renews lock
        - Continuous operation

If leader fails:
        - Lock expires
        - New election among remaining sentinels
        - New leader takes over cluster management
```

### PostgreSQL Failover Process

```
t=0s    Master keeper fails
        - Keeper stops reporting to DCS

t=10s   Sentinel detects missing keeper
        - Waits for keeper-fail-interval

t=20s   Sentinel marks keeper as failed
        - Evaluates standby candidates
        - Considers replication lag

t=21s   Sentinel selects best standby
        - Writes new cluster view to DCS
        - New master designation

t=22s   Selected keeper reads new view
        - Promotes PostgreSQL to master
        - Begins accepting writes

t=23s   Other keepers reconfigure
        - Point replication to new master

t=24s   Proxies update
        - Read new master from DCS
        - Route connections to new master

Total failover time: ~25-30 seconds
```

## Configuration

### Cluster Initialization

```bash
# Initialize cluster in DCS
stolonctl init \
  --cluster-name=production \
  --store-backend=etcdv3 \
  --store-endpoints=http://etcd1:2379,http://etcd2:2379 \
  --yes
```

### Cluster Specification

```json
{
  "initMode": "new",
  "mergePgParameters": true,
  "role": "master",
  "sleepInterval": "5s",
  "failInterval": "20s",
  "deadKeeperRemovalInterval": "48h",
  "maxStandbys": 5,
  "maxStandbysPerSender": 3,
  "synchronousReplication": false,
  "minSynchronousStandbys": 1,
  "maxSynchronousStandbys": 1,
  "pgParameters": {
    "max_connections": "100",
    "shared_buffers": "256MB",
    "wal_level": "replica",
    "max_wal_senders": "10",
    "max_replication_slots": "10"
  },
  "pgHBA": [
    "host replication replicator 0.0.0.0/0 md5",
    "host all all 0.0.0.0/0 md5"
  ]
}
```

### Keeper Configuration

```bash
stolon-keeper \
  --cluster-name=production \
  --store-backend=etcdv3 \
  --store-endpoints=http://etcd1:2379 \
  --data-dir=/var/lib/postgresql/data \
  --pg-listen-address=0.0.0.0 \
  --pg-port=5432 \
  --pg-repl-username=replicator \
  --uid=keeper1
```

### Sentinel Configuration

```bash
stolon-sentinel \
  --cluster-name=production \
  --store-backend=etcdv3 \
  --store-endpoints=http://etcd1:2379
```

### Proxy Configuration

```bash
stolon-proxy \
  --cluster-name=production \
  --store-backend=etcdv3 \
  --store-endpoints=http://etcd1:2379 \
  --listen-address=0.0.0.0 \
  --port=5432
```

## Happy Path Scenarios

### Scenario 1: Normal Cluster Operation

```
Timeline: Steady-state operation
─────────────────────────────────────────────────────────────────

t=0s    Cluster running normally
        - 1 master keeper, 2 standby keepers
        - 3 sentinels (1 leader, 2 followers)
        - 2 proxies (stateless, load-balanced)

t=5s    Keeper heartbeats
        - Each keeper reports state to DCS
        - PostgreSQL status, replication lag

t=5s    Sentinel leader checks
        - Reads keeper states
        - Validates cluster health
        - No action needed

t=5s    Proxies serve traffic
        - Read master info from DCS
        - Route all connections to master

Status: All components healthy
```

### Scenario 2: Planned Failover

```
Timeline: Manual switchover
─────────────────────────────────────────────────────────────────

t=0s    Admin initiates failover
        $ stolonctl failkeeper keeper1

t=1s    Sentinel leader receives command
        - Marks keeper1 as to-be-removed
        - Selects best standby

t=2s    New cluster view published
        - keeper2 designated as new master

t=3s    keeper1 reads view
        - Demotes to standby
        - Or shuts down if requested

t=4s    keeper2 reads view
        - Promotes to master
        - Timeline increments

t=5s    keeper3 reconfigures
        - Points to keeper2

t=6s    Proxies detect change
        - Route to keeper2

Total time: ~6 seconds
```

### Scenario 3: Scaling Out Standbys

```
Timeline: Adding new standby
─────────────────────────────────────────────────────────────────

t=0s    New keeper starts
        - No local data directory

t=1s    Keeper registers with DCS
        - Reports as "uninitialized"

t=2s    Sentinel detects new keeper
        - Assigns as standby
        - Designates master as sync source

t=3s    New keeper initializes
        - Runs pg_basebackup from master
        - Configures streaming replication

t=5min  Base backup completes
        - Keeper reports as "healthy standby"
        - Begins streaming WAL

Status: New standby added to cluster
```

## Unhappy Path Scenarios

### Scenario 1: Master Failure

```
Timeline: Unexpected master crash
─────────────────────────────────────────────────────────────────

t=0s    Master keeper process crashes
        - PostgreSQL stops
        - No graceful shutdown

t=5s    Next keeper heartbeat interval
        - No update from master keeper

t=10s   Sentinel detects stale keeper state
        - Last update > sleepInterval * 2

t=20s   failInterval expires (20s)
        - Sentinel marks master keeper failed

t=21s   Sentinel elects new master
        - Evaluates standby keepers:
          - Replication lag
          - Priority settings
          - Available WAL

t=22s   Best standby selected (keeper2)
        - New cluster view written to DCS

t=23s   keeper2 promotes
        - pg_ctl promote
        - New timeline

t=25s   Remaining keepers reconfigure
        - Point to new master

t=26s   Proxies failover
        - Detect new master in DCS
        - New connections go to keeper2

Total downtime: ~26 seconds
Potential data loss: Uncommitted transactions + unreplicated WAL
```

### Scenario 2: Split Brain Prevention

```
Timeline: Network partition
─────────────────────────────────────────────────────────────────

         DC1 (minority)    │    DC2 (majority)
                          │
    ┌─────────────────┐   │   ┌─────────────────┐
    │ Master Keeper   │   │   │ Standby Keeper  │
    │ Sentinel        │   │   │ Sentinel x2     │
    │ etcd (1 node)   │   │   │ etcd (2 nodes)  │
    └─────────────────┘   │   └─────────────────┘

t=0s    Network partition occurs

t=5s    DC1 sentinel loses contact with DCS quorum
        - Cannot write to etcd
        - Cannot be elected leader

t=10s   DC1 master keeper loses DCS access
        - Cannot report state
        - Enters "lost DCS" state

t=15s   DC1 keeper reads last known state
        - Sees itself as master
        - But cannot verify

t=20s   DC2 sentinels maintain quorum
        - Detect master keeper missing
        - Elect new master from DC2 standbys

t=25s   DC2 standby promotes
        - New master in DC2

t=30s   DC1 master demotes
        - Cannot reach DCS for too long
        - Shuts down PostgreSQL (fencing)

Result: No split brain
        DC1 master cannot accept writes without DCS
        DC2 takes over cleanly
```

### Scenario 3: All Sentinels Fail

```
Timeline: Sentinel layer failure
─────────────────────────────────────────────────────────────────

t=0s    All sentinel processes crash

t=5s    Cluster continues operating
        - Keepers still running
        - PostgreSQL instances healthy
        - Proxies still routing to last known master

t=60s   No cluster state updates
        - Keepers reporting but no sentinel reading
        - Cluster view becomes stale

Problem: No failover capability
        - If master fails now, no automatic recovery
        - Manual intervention required

t=120s  Sentinels restarted
        - Read current keeper states
        - Resume normal operation

Impact: No automatic failover during sentinel outage
        Data serving continues normally
```

### Scenario 4: DCS Failure

```
Timeline: etcd cluster becomes unavailable
─────────────────────────────────────────────────────────────────

t=0s    etcd cluster fails

t=5s    All stolon components lose DCS
        - Keepers cannot report state
        - Sentinels cannot read/write
        - Proxies cannot read master

t=10s   Proxies enter fallback mode
        - Use last known master
        - Continue routing (dangerous)
        - OR refuse connections (safe)

t=10s   Keepers enter fallback mode
        - Continue running PostgreSQL
        - Cannot verify role

t=10s   Sentinels blocked
        - Cannot perform any operations

t=5min  etcd cluster recovers
        - All components reconnect
        - State synchronizes
        - Normal operation resumes

Impact: Undefined behavior during DCS outage
        Configuration determines safety vs availability
```

### Scenario 5: Replication Lag Blocks Failover

```
Timeline: Standby too far behind
─────────────────────────────────────────────────────────────────

Configuration: maxStandbyLag not set (dangerous)

t=0s    Master fails
        - Standby has 10GB replication lag

t=20s   Sentinel elects standby as new master
        - No lag check (misconfiguration)
        - Standby promotes

t=21s   10GB of transactions lost
        - Data inconsistency possible

BETTER Configuration:

{
  "maxStandbyLag": 16777216  // 16MB max lag for failover
}

t=0s    Master fails
        - Standby has 10GB lag

t=20s   Sentinel evaluates standbys
        - All exceed maxStandbyLag
        - No eligible candidates

t=21s   Cluster remains leaderless
        - Prevents data loss
        - Manual intervention needed

Result: Availability sacrificed for data safety
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Cloud Native** | Designed for Kubernetes/containers |
| **Separation of Concerns** | Clear component responsibilities |
| **Proxy Layer** | Simple client configuration (single endpoint) |
| **Stateless Proxy** | Easy horizontal scaling |
| **No pg_rewind** | Simpler recovery via pg_basebackup |
| **Written in Go** | Single binary, easy deployment |
| **Kubernetes Native** | Works well with K8s operators |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **No pg_rewind** | Slower node recovery |
| **DCS Dependency** | Requires etcd/Consul |
| **More Components** | Keeper + Sentinel + Proxy |
| **Less Features** | Fewer configuration options than Patroni |
| **Smaller Community** | Less active than Patroni |
| **Read Scaling** | No built-in read routing |

## Comparison: Stolon vs Patroni

| Feature | Stolon | Patroni |
|---------|--------|---------|
| Architecture | Keeper/Sentinel/Proxy | Single Agent |
| Components | 3 distinct services | 1 service + optional HAProxy |
| Proxy | Built-in | External (HAProxy) |
| pg_rewind | Not used | Supported |
| REST API | No | Yes |
| Configuration | stolonctl/JSON | YAML + REST API |
| Kubernetes | Native design | Adapter via DCS |
| Recovery | Always pg_basebackup | pg_rewind preferred |
| Read Replicas | External routing needed | Via HAProxy |

## Kubernetes Deployment

### Typical K8s Architecture

```yaml
# StatefulSet for Keepers
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: stolon-keeper
spec:
  serviceName: stolon-keeper
  replicas: 3
  template:
    spec:
      containers:
      - name: keeper
        image: sorintlab/stolon:latest
        command: ["stolon-keeper"]
        args:
        - --cluster-name=production
        - --store-backend=etcdv3
        - --store-endpoints=http://etcd:2379
        - --data-dir=/var/lib/postgresql/data
        volumeMounts:
        - name: data
          mountPath: /var/lib/postgresql/data
  volumeClaimTemplates:
  - metadata:
      name: data
    spec:
      accessModes: ["ReadWriteOnce"]
      resources:
        requests:
          storage: 100Gi

---
# Deployment for Sentinels
apiVersion: apps/v1
kind: Deployment
metadata:
  name: stolon-sentinel
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: sentinel
        image: sorintlab/stolon:latest
        command: ["stolon-sentinel"]
        args:
        - --cluster-name=production
        - --store-backend=etcdv3
        - --store-endpoints=http://etcd:2379

---
# Deployment for Proxies
apiVersion: apps/v1
kind: Deployment
metadata:
  name: stolon-proxy
spec:
  replicas: 2
  template:
    spec:
      containers:
      - name: proxy
        image: sorintlab/stolon:latest
        command: ["stolon-proxy"]
        args:
        - --cluster-name=production
        - --store-backend=etcdv3
        - --store-endpoints=http://etcd:2379
        - --listen-address=0.0.0.0
        - --port=5432
```

## Limitations

1. **No Multi-Master**: Single writer only
2. **DCS Required**: etcd or Consul mandatory
3. **No pg_rewind**: Always full base backup for recovery
4. **No Read Routing**: Proxy only routes to master
5. **Limited Ecosystem**: Fewer integrations than Patroni
6. **Monitoring**: No built-in metrics endpoint
7. **Large Recovery Times**: pg_basebackup for large databases
8. **Single Cluster per DCS Path**: Namespace management required

## Best Practices

1. **Run Odd Sentinels**: 3 or 5 for proper leader election
2. **Multiple Proxies**: Behind load balancer for HA
3. **Separate Storage**: Keepers on separate volumes/nodes
4. **Configure maxStandbyLag**: Prevent lagged failover
5. **Monitor etcd**: DCS health is critical
6. **Use initMode=existing**: For brownfield deployments
7. **Implement Connection Retries**: Applications must handle failover
8. **Regular Backups**: Don't rely only on replication

## Conclusion

Stolon is a well-designed cloud-native PostgreSQL HA solution ideal for Kubernetes environments. Its clear separation of concerns (Keeper, Sentinel, Proxy) provides operational clarity. The built-in proxy simplifies client connectivity. However, the lack of pg_rewind support means longer recovery times for failed nodes.

**Recommended for:**
- Kubernetes-native deployments
- Teams preferring clear component separation
- Environments where pg_basebackup recovery time is acceptable
- Simple client connectivity requirements

**Not recommended for:**
- Very large databases (slow recovery)
- Teams needing read replica routing
- Environments requiring pg_rewind
- Non-containerized deployments
