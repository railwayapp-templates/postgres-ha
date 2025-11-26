# RFC-013: YugabyteDB - Distributed PostgreSQL-Compatible Database

## Overview

YugabyteDB is an open-source, distributed SQL database designed for cloud-native applications. It offers a PostgreSQL-compatible API (YSQL) with higher compatibility than CockroachDB, plus a Cassandra-compatible API (YCQL). Built on a DocDB storage layer inspired by Google Spanner, YugabyteDB provides tunable consistency, automatic sharding, and multi-region capabilities.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
        │                                    │
        │ PostgreSQL Protocol (YSQL)         │ Cassandra Protocol (YCQL)
        ▼                                    ▼
┌───────────────────────────────────────────────────────────────────┐
│                         YB-TServer Layer                          │
│  ┌───────────────────┐              ┌───────────────────┐        │
│  │   YSQL Engine     │              │   YCQL Engine     │        │
│  │  (PostgreSQL      │              │  (Cassandra       │        │
│  │   query layer)    │              │   query layer)    │        │
│  └─────────┬─────────┘              └─────────┬─────────┘        │
│            │                                  │                  │
│            └──────────────┬───────────────────┘                  │
│                           │                                      │
│                    ┌──────▼──────┐                               │
│                    │   DocDB     │                               │
│                    │  (Storage   │                               │
│                    │   Engine)   │                               │
│                    └──────┬──────┘                               │
└───────────────────────────┼──────────────────────────────────────┘
                            │
┌───────────────────────────┼──────────────────────────────────────┐
│                     Tablet Layer                                  │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐             │
│  │ Tablet  │  │ Tablet  │  │ Tablet  │  │ Tablet  │             │
│  │   1     │  │   2     │  │   3     │  │   4     │             │
│  │ (Raft   │  │ (Raft   │  │ (Raft   │  │ (Raft   │             │
│  │  Group) │  │  Group) │  │  Group) │  │  Group) │             │
│  └─────────┘  └─────────┘  └─────────┘  └─────────┘             │
│       │            │            │            │                   │
│       └────────────┴────────────┴────────────┘                   │
│                         │                                        │
│              ┌──────────▼──────────┐                             │
│              │   RocksDB Storage   │                             │
│              │   (per TServer)     │                             │
│              └─────────────────────┘                             │
└──────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────┐
│                       YB-Master Layer                             │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐                          │
│  │ Master  │  │ Master  │  │ Master  │                          │
│  │    1    │  │    2    │  │    3    │                          │
│  │ (Leader)│  │(Follower)│ │(Follower)│                          │
│  └─────────┘  └─────────┘  └─────────┘                          │
│      Metadata management, tablet placement, load balancing       │
└──────────────────────────────────────────────────────────────────┘
```

## Core Concepts

### 1. Two API Layers

```
YSQL (PostgreSQL-compatible):
─────────────────────────────────────────────────────────────────
- Full SQL support
- PostgreSQL wire protocol
- Many PostgreSQL features
- Extensions support (limited)
- Best for existing PostgreSQL apps

YCQL (Cassandra-compatible):
─────────────────────────────────────────────────────────────────
- CQL syntax
- Cassandra drivers
- Wide-column model
- Best for high-throughput workloads
- Eventually consistent option
```

### 2. DocDB Storage Engine

```
DocDB Architecture:
─────────────────────────────────────────────────────────────────

Tablet (unit of sharding):
    - Range of key space
    - Raft group for replication
    - Default: 3 replicas

Key Structure:
    DocKey = [hash | range] + [subkeys...]

    Example: (user_id, post_id) → (hash(user_id), post_id)

Hash Sharding:
    - Default for most tables
    - Even distribution
    - Good for point queries

Range Sharding:
    - For range scans
    - Ordered data
    - Good for time-series
```

### 3. Consistency Levels

```
Consistency Options:
─────────────────────────────────────────────────────────────────

YSQL (default = strong):
    - Serializable isolation
    - Single-row linearizable
    - Distributed transactions via 2PC

YCQL (configurable):
    - Strong consistency (default)
    - Timeline consistency (relaxed)
    - Tunable per operation

Read Replicas (async):
    - Follower reads for lower latency
    - Eventual consistency
    - Good for geo-distributed reads
```

## How HA Works

### Tablet Replication

```
Tablet Raft Group:
─────────────────────────────────────────────────────────────────

    Tablet T1: Data for keys [A-F]

    TServer1 (Leader)     TServer2 (Follower)    TServer3 (Follower)
         │                      │                      │
         │◄─────────────────────┼──────────────────────│
         │     Raft Protocol    │                      │
         │                      │                      │
    ┌────▼────┐            ┌────▼────┐            ┌────▼────┐
    │ T1      │            │ T1      │            │ T1      │
    │ (Leader)│───────────►│(Follower)───────────►│(Follower)│
    │         │   Replicate│         │   Replicate│         │
    └─────────┘            └─────────┘            └─────────┘

Write Path:
    1. Client → Leader
    2. Leader → Log entry
    3. Replicate to followers
    4. Majority ACK (2/3)
    5. Commit + respond
```

### Master HA

```
YB-Master Raft Group:
─────────────────────────────────────────────────────────────────

Masters manage:
    - Cluster metadata
    - Table schemas
    - Tablet locations
    - Load balancing decisions

Master1 (Leader)    Master2 (Follower)   Master3 (Follower)
      │                   │                    │
      │◄──────────────────┼────────────────────│
      │    Raft consensus │                    │
      │                   │                    │
  ┌───▼───┐           ┌───▼───┐           ┌───▼───┐
  │Metadata│           │Metadata│           │Metadata│
  │(active)│           │(replica)│          │(replica)│
  └────────┘           └────────┘           └────────┘

If Master1 fails:
    - Master2 or Master3 elected leader
    - Metadata operations continue
    - Tablet operations unaffected (TServers independent)
```

## Configuration

### Cluster Setup

```bash
# Start YB-Master nodes
yb-master \
    --master_addresses=master1:7100,master2:7100,master3:7100 \
    --rpc_bind_addresses=master1:7100 \
    --fs_data_dirs=/data/yb-master

# Start YB-TServer nodes
yb-tserver \
    --tserver_master_addrs=master1:7100,master2:7100,master3:7100 \
    --rpc_bind_addresses=tserver1:9100 \
    --pgsql_proxy_bind_address=0.0.0.0:5433 \
    --fs_data_dirs=/data/yb-tserver \
    --placement_cloud=aws \
    --placement_region=us-east-1 \
    --placement_zone=us-east-1a
```

### PostgreSQL Connection

```python
import psycopg2

# Connect to YSQL (PostgreSQL-compatible)
conn = psycopg2.connect(
    host="tserver1",
    port=5433,
    dbname="yugabyte",
    user="yugabyte",
    password="yugabyte",
    # Load balance across TServers
    load_balance="true",
    topology_keys="aws.us-east-1.*"
)

# Multi-host connection
conn = psycopg2.connect(
    "postgresql://yugabyte:yugabyte@ts1:5433,ts2:5433,ts3:5433/yugabyte"
)
```

### Table Configuration

```sql
-- Hash-sharded table (default)
CREATE TABLE users (
    id UUID PRIMARY KEY,
    email VARCHAR(255),
    region VARCHAR(50)
) SPLIT INTO 16 TABLETS;

-- Range-sharded table (for scans)
CREATE TABLE events (
    event_time TIMESTAMP,
    event_id UUID,
    data JSONB,
    PRIMARY KEY (event_time, event_id ASC)
) SPLIT AT VALUES (
    ('2024-01-01'),
    ('2024-04-01'),
    ('2024-07-01'),
    ('2024-10-01')
);

-- Colocated tables (single tablet, lower overhead)
CREATE DATABASE mydb WITH COLOCATION = true;

-- Tablespace for geo-placement
CREATE TABLESPACE us_east WITH (
    replica_placement = '{"num_replicas": 3, "placement_blocks": [
        {"cloud": "aws", "region": "us-east-1", "zone": "us-east-1a", "min_num_replicas": 1},
        {"cloud": "aws", "region": "us-east-1", "zone": "us-east-1b", "min_num_replicas": 1},
        {"cloud": "aws", "region": "us-east-1", "zone": "us-east-1c", "min_num_replicas": 1}
    ]}'
);
```

## Happy Path Scenarios

### Scenario 1: Horizontal Scaling

```
Timeline: Adding capacity
─────────────────────────────────────────────────────────────────

Initial: 3 TServers, 48 tablets, 16 tablets/TServer

t=0s    Add TServer4
        $ yb-tserver --tserver_master_addrs=... --placement_zone=us-east-1d

t=10s   TServer4 joins cluster
        - Reports to masters
        - Ready for tablets

t=30s   Load balancer activates
        - Masters detect imbalance
        - Plan tablet movements

t=5min  Gradual rebalancing
        - Tablets move to TServer4
        - Leader transfers for minimal impact

t=30min Balanced: 12 tablets/TServer
        - Zero downtime
        - Automatic process

Performance scales linearly with TServer count
```

### Scenario 2: Read Replicas for Geo-Distribution

```
Timeline: Low-latency global reads
─────────────────────────────────────────────────────────────────

Primary cluster: us-east-1 (3 TServers)
Read replica: eu-west-1 (2 TServers)

Setup:
    $ yb-admin add_read_replica_placement_info \
        aws.eu-west-1.eu-west-1a:1,aws.eu-west-1.eu-west-1b:1 2

t=0s    Read replica configured
        - Async replication from primary

t=30s   Data flowing to EU replicas
        - Lag: ~100ms typical

EU User Query:
    t=0ms   SELECT * FROM products WHERE id = 123;
    t=5ms   Routed to local EU replica
    t=10ms  Response (local latency)

Without read replica:
    t=0ms   Query
    t=80ms  Cross-Atlantic round trip
    t=90ms  Response

Benefit: 80ms → 10ms read latency
Trade-off: Eventual consistency (~100ms lag)
```

### Scenario 3: Point-in-Time Recovery

```
Timeline: Recovering from user error
─────────────────────────────────────────────────────────────────

t=10:00  Data looks good
t=10:30  User accidentally: DROP TABLE important_data;
t=10:35  Realize mistake

Recovery with PITR:
    $ yb-admin restore_snapshot_schedule \
        schedule_id \
        '2024-01-15 10:25:00'

t=10:40  Snapshot restored
        - Database state from 10:25
        - 5 minutes of data re-applied from WAL

t=10:45  important_data table restored
        - Minimal data loss
        - Quick recovery

Note: Requires snapshot schedule configured
```

## Unhappy Path Scenarios

### Scenario 1: TServer Failure

```
Timeline: TServer crashes
─────────────────────────────────────────────────────────────────

t=0s    TServer2 crashes
        - 16 tablet replicas unavailable
        - Leaders on TServer2 need election

t=0s    Raft elections trigger
        - Each tablet with leader on TServer2
        - Remaining followers elect new leader

t=2s    New leaders elected
        - TServer1 and TServer3 take over
        - Write availability restored

t=3s    Queries continue
        - Slightly slower (fewer TServers)
        - All data accessible

t=15min Under-replication detected
        - Tablets have 2/3 replicas

t=20min Re-replication begins
        - If TServer2 doesn't return
        - New replicas created on remaining TServers

Impact: 2-3 second leader election
        No data loss (committed data on majority)
```

### Scenario 2: Network Partition

```
Timeline: Network splits cluster
─────────────────────────────────────────────────────────────────

    TServer1, Master1     │     TServer2, TServer3, Master2, Master3
         │                │              │
     (minority)           │          (majority)

t=0s    Partition occurs

t=1s    Master leader election
        - If Master1 was leader
        - Master2 or Master3 becomes leader

t=2s    Tablet leader elections
        - Tablets with leaders on TServer1
        - Move to TServer2/TServer3 (majority)

t=3s    Majority partition operational
        - Full write capability
        - All committed data

t=3s    Minority partition (TServer1)
        - Cannot achieve Raft majority
        - Tablets read-only (stale)
        - Writes rejected

t=???   Partition heals
        - TServer1 rejoins
        - Catches up from peers
        - Full cluster restored

No split brain: Raft prevents conflicting leaders
```

### Scenario 3: Master Quorum Loss

```
Timeline: Multiple master failure
─────────────────────────────────────────────────────────────────

t=0s    Master1 and Master2 fail
        - Only Master3 remains
        - Cannot form majority (1/3)

t=1s    Master operations halted
        - Cannot modify metadata
        - Cannot create tables
        - Cannot change tablets

t=1s    TServer operations continue!
        - Existing tablets work
        - Reads and writes continue
        - No new tablets/tables

t=???   Restore master quorum
        - Bring back Master1 or Master2
        - Or replace with new master

Impact: DDL/admin operations blocked
        Data operations continue
```

### Scenario 4: Tablet Split During Load

```
Timeline: Auto-split under pressure
─────────────────────────────────────────────────────────────────

t=0s    Tablet T1 grows to 100GB
        - Threshold: 64GB default

t=10s   Auto-split triggered
        - T1 splits into T1a, T1b

t=15s   Split in progress
        - Writes to T1 continue
        - Redirected as split completes

t=30s   Split complete
        - T1a: keys [A-M]
        - T1b: keys [N-Z]
        - New Raft groups formed

Impact: Brief latency spike during split
        Automatic, no manual intervention
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **High PostgreSQL Compatibility** | Better than CockroachDB |
| **Dual API** | YSQL (PostgreSQL) + YCQL (Cassandra) |
| **Tunable Consistency** | Strong or eventual per use case |
| **Geographic Distribution** | Built-in multi-region support |
| **Open Source Core** | Full-featured free tier |
| **Automatic Sharding** | Tablets split/merge automatically |
| **Read Replicas** | Async replicas for read scaling |
| **Colocated Tables** | Reduce overhead for small tables |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Complexity** | Masters + TServers + tablets |
| **Resource Overhead** | More memory than single PostgreSQL |
| **Write Latency** | Raft consensus overhead |
| **Not Full PostgreSQL** | Some features missing |
| **Operational Knowledge** | Distributed systems expertise needed |
| **Debugging** | Complex distributed traces |
| **Cost** | Minimum 3 nodes for HA |

## PostgreSQL Compatibility (YSQL)

### Supported
- Most SQL syntax
- Transactions (serializable, snapshot)
- Stored procedures (PL/pgSQL)
- Triggers
- Foreign keys
- Partial indexes
- Expression indexes
- JSON/JSONB
- Many extensions (pg_stat_statements, pg_hint_plan)

### Not Supported or Limited
- Some system catalogs
- Savepoints (limited)
- Advisory locks (limited)
- Full-text search (limited)
- Logical replication
- Some extensions (PostGIS, etc.)

## Comparison with Alternatives

| Feature | YugabyteDB | CockroachDB | PostgreSQL | Aurora |
|---------|------------|-------------|------------|--------|
| PostgreSQL Compat | High | Medium | Full | Full |
| Distributed Txns | Yes | Yes | No | No |
| Write Scaling | Yes | Yes | No | No |
| Read Replicas | Yes | Yes | Yes | Yes |
| Multi-Region | Built-in | Built-in | Manual | Global DB |
| Open Source | Yes (full) | Limited | Yes | No |
| Cassandra API | Yes | No | No | No |

## Limitations

1. **Not 100% PostgreSQL**: Some features/extensions missing
2. **Minimum Cluster Size**: 3 nodes for HA
3. **Resource Requirements**: More than single-node PostgreSQL
4. **Network Sensitivity**: Performance depends on network
5. **Complexity**: Multiple component types
6. **Write Latency**: Consensus overhead
7. **Hot Spots**: Single tablet can bottleneck
8. **Learning Curve**: New operational patterns

## Best Practices

1. **Use Colocated Tables**: For small/related tables
2. **Choose Sharding Wisely**: Hash vs range based on queries
3. **Plan Tablet Count**: Too few = hot spots, too many = overhead
4. **Monitor Tablet Balance**: Use YugabyteDB metrics
5. **Configure Read Replicas**: For geo-distributed reads
6. **Use Connection Pooling**: Reduce connection overhead
7. **Test Failure Scenarios**: Verify HA behavior
8. **Size Appropriately**: Memory for tablet overhead

## Conclusion

YugabyteDB offers a compelling balance between PostgreSQL compatibility and distributed database capabilities. Its higher PostgreSQL compatibility (compared to CockroachDB) makes migration easier, while still providing horizontal scaling and multi-region support. The dual API (YSQL + YCQL) adds flexibility for different workload types.

**Recommended for:**
- PostgreSQL applications needing horizontal scale
- Multi-region deployments with consistency needs
- Teams wanting PostgreSQL familiarity in distributed DB
- Workloads benefiting from both SQL and Cassandra models

**Not recommended for:**
- Simple single-node deployments
- Applications requiring 100% PostgreSQL compatibility
- Latency-critical single-region workloads
- Teams without distributed systems experience
