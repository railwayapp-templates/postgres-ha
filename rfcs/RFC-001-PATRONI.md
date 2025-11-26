# RFC-001: Patroni - PostgreSQL High Availability

## Overview

Patroni is a template for building high-availability PostgreSQL clusters using Python. Developed by Zalando, it manages automatic failover and provides a REST API for cluster management. Patroni uses a Distributed Configuration Store (DCS) for leader election and cluster state management.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Load Balancer (HAProxy)                      │
│              Port 5000 (RW) / Port 5001 (RO)                   │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│    Node 1     │     │    Node 2     │     │    Node 3     │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │ Patroni │  │     │  │ Patroni │  │     │  │ Patroni │  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │PostgreSQL│ │     │  │PostgreSQL│ │     │  │PostgreSQL│ │
│  │ (Leader) │ │     │  │(Replica) │ │     │  │(Replica) │ │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
└───────────────┘     └───────────────┘     └───────────────┘
        │                     │                     │
        └─────────────────────┼─────────────────────┘
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│              Distributed Configuration Store (DCS)              │
│                  (etcd / Consul / ZooKeeper)                   │
└─────────────────────────────────────────────────────────────────┘
```

## Core Components

### 1. Patroni Agent
- Runs alongside each PostgreSQL instance
- Monitors PostgreSQL health via pg_isready and custom checks
- Participates in leader election via DCS
- Manages PostgreSQL configuration and replication
- Exposes REST API (default port 8008)

### 2. Distributed Configuration Store (DCS)
Supported backends:
- **etcd** (recommended) - Strong consistency, widely used
- **Consul** - Service discovery integration
- **ZooKeeper** - Mature, complex
- **Kubernetes** - Native K8s integration
- **Raft** (experimental) - No external dependency

### 3. PostgreSQL Streaming Replication
- Asynchronous replication by default
- Synchronous replication configurable
- WAL-based replication for durability

## How It Works

### Leader Election Process

```
1. Patroni starts → Connects to DCS
2. Attempts to create leader key with TTL
3. If successful → Becomes leader, configures PostgreSQL as primary
4. If fails → Becomes replica, streams from leader
5. Leader continuously renews TTL (heartbeat)
6. If leader fails to renew → Key expires → New election
```

### State Machine

```
                    ┌─────────────┐
                    │   STOPPED   │
                    └──────┬──────┘
                           │ start
                           ▼
                    ┌─────────────┐
            ┌───────│  STARTING   │───────┐
            │       └─────────────┘       │
            │ win election     lose election
            ▼                             ▼
     ┌─────────────┐              ┌─────────────┐
     │   MASTER    │◄────────────►│   REPLICA   │
     └─────────────┘  failover    └─────────────┘
            │                             │
            │ demote                      │ promote
            ▼                             ▼
     ┌─────────────┐              ┌─────────────┐
     │   DEMOTING  │              │  PROMOTING  │
     └─────────────┘              └─────────────┘
```

## Configuration

### Minimal Configuration (patroni.yml)

```yaml
scope: postgres-cluster
name: node1

restapi:
  listen: 0.0.0.0:8008
  connect_address: node1:8008

etcd3:
  hosts:
    - etcd1:2379
    - etcd2:2379
    - etcd3:2379

bootstrap:
  dcs:
    ttl: 30
    loop_wait: 10
    retry_timeout: 10
    maximum_lag_on_failover: 1048576  # 1MB
    postgresql:
      use_pg_rewind: true
      parameters:
        max_connections: 100
        max_wal_senders: 10
        wal_level: replica
        hot_standby: "on"

  initdb:
    - encoding: UTF8
    - data-checksums

postgresql:
  listen: 0.0.0.0:5432
  connect_address: node1:5432
  data_dir: /var/lib/postgresql/data
  authentication:
    superuser:
      username: postgres
      password: secretpassword
    replication:
      username: replicator
      password: replpassword
```

## Happy Path Scenarios

### Scenario 1: Normal Cluster Operation

```
Timeline: Normal steady-state operation
─────────────────────────────────────────────────────────────────

t=0s    Leader (Node1) holds DCS lock
        - Heartbeat every 10s (loop_wait)
        - PostgreSQL accepting writes
        - Replicas streaming WAL

t=10s   Leader renews lock (TTL reset to 30s)
        - Replicas report healthy via REST API
        - HAProxy health checks pass

t=20s   Leader renews lock again
        - Application writes committed
        - WAL shipped to replicas

Status: All nodes healthy, replication lag < 1MB
```

### Scenario 2: Planned Switchover

```
Timeline: Manual switchover via patronictl
─────────────────────────────────────────────────────────────────

t=0s    Admin runs: patronictl switchover
        - Selects target replica (Node2)

t=1s    Leader (Node1) receives switchover signal
        - Completes pending transactions
        - Checkpoints WAL

t=2s    Leader demotes to replica
        - Releases DCS lock
        - Enters read-only mode

t=3s    Target (Node2) acquires DCS lock
        - Promotes to primary
        - Accepts new writes

t=5s    Other replicas repoint to new leader
        - HAProxy detects change via health checks

Total downtime: ~3-5 seconds
```

### Scenario 3: New Replica Joins

```
Timeline: Adding a new replica to cluster
─────────────────────────────────────────────────────────────────

t=0s    New Patroni node starts
        - Connects to DCS, discovers cluster

t=1s    Patroni creates replica
        - Runs pg_basebackup from leader
        - Configures recovery.conf/standby.signal

t=5min  Base backup completes (depends on data size)
        - PostgreSQL starts in recovery mode
        - Begins streaming WAL from leader

t=6min  Replica catches up
        - Reports healthy via REST API
        - Added to HAProxy pool

Status: New replica serving read traffic
```

## Unhappy Path Scenarios

### Scenario 1: Leader Crashes

```
Timeline: Primary node sudden failure
─────────────────────────────────────────────────────────────────

t=0s    Leader (Node1) crashes
        - Process killed, no graceful shutdown
        - DCS lock still held (TTL=30s remaining)

t=10s   Loop wait expires, no heartbeat
        - DCS lock TTL counting down
        - Replicas detect leader unhealthy via REST

t=30s   DCS lock expires (TTL timeout)
        - Leader key removed
        - Election triggered

t=31s   Remaining nodes compete for lock
        - Node2 wins (lowest lag, highest priority)
        - Creates new leader key

t=32s   Node2 promotes PostgreSQL
        - Timeline increments
        - pg_promote() called
        - Accepts new connections

t=33s   Node3 reconfigures
        - Points to new leader (Node2)
        - Restarts streaming replication

t=35s   HAProxy detects new primary
        - Health check on :8008/primary succeeds
        - Traffic routed to Node2

Total failover time: ~35 seconds
Data loss: Potentially up to maximum_lag_on_failover (1MB WAL)

⚠️  DATA LOSS EXPLAINED:
    With async replication (default), transactions committed on the leader
    may NOT be on replicas yet. These are LOST when the leader crashes.

    Example: Client receives "COMMIT successful" but data is gone after failover.

    This is the fundamental tradeoff of async replication:
    - Lower latency (~1ms commits)
    - But potential data loss on failover

    To prevent: Enable synchronous_mode (accepts higher latency ~5-15ms)
```

### Scenario 2: Network Partition (Split Brain Prevention)

```
Timeline: Network splits cluster
─────────────────────────────────────────────────────────────────

         Partition
            │
   DC1      │      DC2
┌───────┐   │   ┌───────┐
│ Node1 │   │   │ Node2 │
│(Leader)│   │   │(Replica)│
│ etcd1 │   │   │ etcd2 │
└───────┘   │   │ etcd3 │
            │   └───────┘

t=0s    Network partition occurs
        - Node1 isolated with etcd1 (minority)
        - Node2 has etcd2 + etcd3 (majority)

t=10s   Node1 cannot reach etcd quorum
        - Fails to renew leader lock
        - etcd rejects writes (no quorum)

t=15s   Node1 demotes to read-only
        - Detects loss of DCS connectivity
        - Calls on_restart.sh or fences itself

t=30s   Leader lock expires in DCS
        - etcd2/etcd3 still have quorum
        - Node2 can acquire lock

t=31s   Node2 promotes to primary
        - New leader in DC2
        - Accepts writes

t=60s   Network heals
        - Node1 rejoins cluster as replica
        - May need pg_rewind if diverged

Result: No split brain - DCS quorum prevents dual masters
```

### Scenario 3: DCS Cluster Failure

```
Timeline: All DCS nodes become unavailable
─────────────────────────────────────────────────────────────────

t=0s    etcd cluster fails (all nodes down)
        - Patroni loses DCS connectivity

t=10s   Leader cannot renew lock
        - retry_timeout countdown begins

t=20s   retry_timeout expires (10s default)
        - Leader demotes PostgreSQL to read-only
        - Prevents writes without consensus

t=30s   All Patroni nodes in "DCS unavailable" state
        - PostgreSQL instances running but read-only
        - No failover possible

t=5min  DCS cluster recovers
        - Patroni reconnects
        - Previous leader re-acquires lock (if still valid)
        - Or election occurs

Result: Complete unavailability of writes during DCS outage
        Reads may continue if configured (dangerous)
```

### Scenario 4: Replica Too Far Behind

```
Timeline: Failover blocked due to replication lag
─────────────────────────────────────────────────────────────────

t=0s    Leader crashes
        - Replica lag: 500MB (exceeds maximum_lag_on_failover)

t=30s   DCS lock expires
        - Election triggered

t=31s   Replica checks eligibility
        - Lag > maximum_lag_on_failover (1MB)
        - Refuses to participate in election

t=32s   No eligible candidates
        - Cluster remains leaderless
        - All nodes read-only

Manual intervention required:
- Increase maximum_lag_on_failover (accept data loss)
- Or restore from backup

Result: Cluster unavailable until manual intervention
```

### Scenario 5: pg_rewind Failure

```
Timeline: Old leader cannot rejoin after failover
─────────────────────────────────────────────────────────────────

t=0s    Failover completed
        - Node2 is new leader (timeline 2)
        - Node1 crashed, now recovering

t=60s   Node1 restarts
        - Discovers it's on old timeline
        - Attempts pg_rewind to rejoin

t=61s   pg_rewind fails
        - Required WAL no longer available
        - Or checksums don't match

t=62s   Patroni removes old leader
        - Clears data directory
        - Reinitializes from pg_basebackup

t=10min  Node1 rejoins as fresh replica
        - Full base backup from Node2
        - Longer recovery time

Impact: Extended recovery time for failed node
Mitigation: Enable wal_log_hints, retain more WAL
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Maturity** | Battle-tested at Zalando (4000+ clusters) |
| **Flexibility** | Multiple DCS backends, extensive configuration |
| **REST API** | Easy integration with load balancers, monitoring |
| **pg_rewind** | Fast node recovery without full backup |
| **Kubernetes** | Native support via K8s API as DCS |
| **Synchronous mode** | Optional zero data loss configuration |
| **Cascading replication** | Reduces load on primary |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **External dependency** | Requires DCS cluster (etcd/Consul/ZK) |
| **Failover time** | 30-60s typical (TTL + detection + promotion) |
| **Complexity** | Multiple components to manage |
| **DCS availability** | Cluster unusable if DCS fails |
| **Single write node** | No multi-master support |
| **Resource overhead** | DCS cluster needs 3+ nodes |

## Configuration Parameters Deep Dive

### Critical Timing Parameters

```yaml
ttl: 30
# Time-to-live for leader lock in DCS
# Lower = faster failover, higher = more stability
# Recommendation: 30s for most deployments

loop_wait: 10
# Interval between Patroni main loop iterations
# Lower = faster detection, more DCS load
# Recommendation: 10s (must be < ttl)

retry_timeout: 10
# Time to retry DCS operations before giving up
# Affects behavior during DCS instability
# Recommendation: 10s

maximum_lag_on_failover: 1048576
# Max bytes of WAL lag for failover eligibility
# Lower = less data loss, higher = more availability
# Recommendation: 1MB-10MB depending on RPO
```

### Failover Time Calculation

```
Minimum failover time = ttl - loop_wait + promotion_time
                      = 30 - 10 + 5
                      = 25 seconds

Maximum failover time = ttl + promotion_time + haproxy_check_interval
                      = 30 + 5 + 5
                      = 40 seconds
```

## Synchronous Replication Mode

### Configuration

```yaml
bootstrap:
  dcs:
    synchronous_mode: true
    synchronous_mode_strict: false  # Allow async if no sync available
    synchronous_node_count: 1       # Number of sync replicas
```

### Behavior

```
Synchronous Mode ON:
─────────────────────────────────────────────────────────────────
- Primary waits for replica acknowledgment before commit
- Zero data loss guarantee (RPO = 0)
- Write latency increases (network round-trip)
- If sync replica fails:
  - synchronous_mode_strict=false → Falls back to async
  - synchronous_mode_strict=true → Writes blocked

Trade-off: Durability vs Availability vs Latency
```

### Actual Latency Cost of Synchronous Replication

Based on [EDB benchmarks](https://www.enterprisedb.com/blog/the-varying-cost-synchronous-replication), the real-world cost of synchronous replication depends heavily on network latency between nodes:

| Network Latency | Sync vs Async Performance |
|-----------------|---------------------------|
| 0.1ms (same rack) | ~95% of async throughput |
| 1ms (same DC) | ~90% of async throughput |
| 3ms (cross-AZ) | ~92% at 40 clients, parity at 80 clients |
| 10ms (cross-region) | ~60% at 40 clients, ~84% at 80 clients |

**Key finding**: At high concurrency, CPU saturation masks the replication overhead. The latency becomes "background noise."

### Comparison with Managed Services

Managed services like Aurora achieve lower latency with synchronous durability through architectural innovations:

| Solution | Sync Method | Min Commit Latency | Data Loss (RPO) |
|----------|-------------|-------------------|-----------------|
| **Patroni (async)** | None | ~0.5-1ms | Up to `maximum_lag_on_failover` |
| **Patroni (sync)** | Streaming + fsync | ~2-5ms* | Zero |
| **RDS Multi-AZ Instance** | Streaming + EBS fsync | ~4ms | Zero |
| **RDS Multi-AZ Cluster** | Semi-sync + NVMe | ~2ms | Zero |
| **Aurora** | 4-of-6 quorum, memory ack | ~2ms | Zero |

*With NVMe storage and <1ms network latency between nodes. On slower storage or higher-latency networks, expect 5-15ms.

**Important**: Patroni sync and RDS Multi-AZ Cluster use the **same underlying mechanism** (PostgreSQL streaming replication). With equivalent hardware (NVMe SSDs, low-latency network), they achieve similar performance. The difference is:

- **RDS Cluster**: AWS guarantees fast hardware (R6gd/M6gd with local NVMe) and optimized inter-AZ networking
- **Patroni**: Performance depends on your infrastructure choices

**Aurora is architecturally different:**

```
Patroni/RDS Cluster (PostgreSQL streaming replication):
    Primary WAL → Network → Replica WAL → Replica fsync → Ack
    Latency depends on: storage speed + network latency

Aurora (custom distributed storage):
    Primary → Send small redo log → 4-of-6 storage nodes ack from MEMORY
    Durability via replication count, not individual fsyncs
```

Aurora achieves consistently low latency (~2ms) because storage nodes acknowledge from memory (relying on 6-way replication for durability). PostgreSQL streaming replication (Patroni or RDS) depends on actual fsync performance.

### Recommendation

- **Low-latency requirement + zero data loss**: Consider Aurora or RDS Multi-AZ Cluster
- **Cost-sensitive + can tolerate async**: Patroni with async replication + monitoring
- **Self-managed + zero data loss**: Patroni sync mode, accept ~5-15ms commit latency
- **Hybrid**: Use async for most writes, sync for critical transactions via `synchronous_commit` per-transaction

## Monitoring & Health Checks

### REST API Endpoints

```
GET /                 → Node status (JSON)
GET /health           → Returns 200 if PostgreSQL is running
GET /primary          → Returns 200 only if node is primary
GET /replica          → Returns 200 only if node is replica
GET /read-only        → Returns 200 if accepting reads
GET /read-write       → Returns 200 if accepting writes
GET /leader           → Returns 200 if holds leader lock
GET /liveness         → Kubernetes liveness probe
GET /readiness        → Kubernetes readiness probe
```

### HAProxy Health Check Configuration

```haproxy
backend postgres_primary
    option httpchk GET /primary
    http-check expect status 200
    default-server inter 3s fall 3 rise 2
    server node1 node1:5432 check port 8008
    server node2 node2:5432 check port 8008
    server node3 node3:5432 check port 8008

backend postgres_replica
    option httpchk GET /replica
    http-check expect status 200
    balance roundrobin
    default-server inter 3s fall 3 rise 2
    server node1 node1:5432 check port 8008
    server node2 node2:5432 check port 8008
    server node3 node3:5432 check port 8008
```

### Replication Lag Monitoring (Critical)

Monitoring replication lag is essential to prevent data loss during failover. With async replication, any WAL not yet shipped to replicas is lost if the leader crashes.

**Why it matters:**
- `maximum_lag_on_failover` (default 1MB) determines failover eligibility
- If all replicas exceed this threshold, failover is blocked entirely
- Even within threshold, all unreplicated data is lost on failover

**What to monitor:**

```sql
-- On the primary: check replication lag per replica
SELECT
    client_addr,
    state,
    sent_lsn,
    write_lsn,
    flush_lsn,
    replay_lsn,
    pg_wal_lsn_diff(sent_lsn, replay_lsn) AS replay_lag_bytes,
    pg_wal_lsn_diff(pg_current_wal_lsn(), sent_lsn) AS send_lag_bytes
FROM pg_stat_replication;
```

**Via Patroni REST API:**

```bash
# Returns cluster state including lag info
curl -s http://node1:8008/cluster | jq '.members[] | {name, role, lag}'
```

**Recommended alerts:**

| Metric | Warning | Critical |
|--------|---------|----------|
| Replication lag (bytes) | > 100KB | > 500KB |
| Replication lag (seconds) | > 5s | > 30s |
| Replicas with lag > threshold | any | all |

**Key insight:** If replication lag consistently approaches `maximum_lag_on_failover`, you risk either data loss (if failover occurs) or unavailability (if all replicas become ineligible). Consider enabling synchronous replication for critical workloads.

## Limitations

1. **No Multi-Master**: Only single-writer architecture
2. **DCS Dependency**: External system required and must be highly available
3. **Failover Duration**: 30-60 seconds typical, cannot achieve sub-second
4. **Geographic Distribution**: High latency to DCS impacts stability
5. **Large Clusters**: Not designed for >10 nodes typically
6. **WAL Retention**: Must be carefully managed for pg_rewind
7. **Connection Handling**: Existing connections terminated on failover
8. **Read-Your-Writes**: No built-in read-your-writes consistency for replicas

## Best Practices

1. **DCS Quorum**: Always deploy odd number of DCS nodes (3 or 5)
2. **Separate Failure Domains**: Place nodes in different racks/zones
3. **Enable pg_rewind**: Faster node recovery
4. **Use Synchronous for Critical Data**: Accept latency penalty
5. **Monitor Replication Lag**: Alert before it exceeds failover threshold
6. **Test Failover Regularly**: Chaos engineering in staging
7. **Tune TTL Carefully**: Balance failover speed vs stability
8. **Implement Connection Retry Logic**: Applications must handle reconnects

## Comparison with Alternatives

| Feature | Patroni | pg_auto_failover | Stolon |
|---------|---------|------------------|--------|
| DCS Required | Yes | No (monitor node) | Yes |
| Failover Time | 30-60s | 15-30s | 30-60s |
| REST API | Yes | No | No |
| Kubernetes Native | Yes | Limited | Yes |
| Sync Replication | Yes | Yes | Yes |
| pg_rewind | Yes | Yes | No |
| Community Size | Large | Medium | Medium |

## Conclusion

Patroni is the most widely-adopted open-source PostgreSQL HA solution. Its flexibility, extensive configuration options, and battle-tested reliability make it suitable for most production deployments. The main trade-offs are the operational complexity of managing a DCS cluster and the 30-60 second failover window.

**Recommended for:**
- Production deployments requiring proven reliability
- Teams with operational expertise in distributed systems
- Kubernetes environments (using K8s DCS)
- Organizations needing extensive customization

**Not recommended for:**
- Sub-second failover requirements
- Teams without distributed systems experience
- Small deployments where complexity isn't justified
- Multi-master write requirements
