# RFC-012: CockroachDB - Distributed SQL with PostgreSQL Wire Protocol

## Overview

CockroachDB is a distributed SQL database designed to survive datacenter failures with zero data loss. While not PostgreSQL itself, it implements the PostgreSQL wire protocol, making it compatible with many PostgreSQL applications. CockroachDB provides serializable isolation, automatic sharding, and multi-region capabilities with built-in replication using the Raft consensus protocol.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
│                 (PostgreSQL-compatible drivers)                 │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ PostgreSQL Wire Protocol
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                      Load Balancer                              │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│  CockroachDB  │     │  CockroachDB  │     │  CockroachDB  │
│    Node 1     │     │    Node 2     │     │    Node 3     │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │   SQL   │  │     │  │   SQL   │  │     │  │   SQL   │  │
│  │  Layer  │  │     │  │  Layer  │  │     │  │  Layer  │  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │  Dist   │  │     │  │  Dist   │  │     │  │  Dist   │  │
│  │  KV     │  │     │  │  KV     │  │     │  │  KV     │  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │ Storage │  │     │  │ Storage │  │     │  │ Storage │  │
│  │(Pebble) │  │     │  │(Pebble) │  │     │  │(Pebble) │  │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
└───────────────┘     └───────────────┘     └───────────────┘
        │                     │                     │
        └─────────────────────┴─────────────────────┘
              Raft consensus between range replicas
```

## Core Concepts

### 1. Ranges and Replication

```
Data Organization:
─────────────────────────────────────────────────────────────────

Database → Tables → Key-Value pairs → Ranges

Range:
    - ~512MB of contiguous key-value data
    - Unit of replication
    - Has a Raft group for consensus

Replication Factor (default 3):
    Range A: [Node1*, Node2, Node3]  (* = leader)
    Range B: [Node2*, Node3, Node1]
    Range C: [Node3*, Node1, Node2]

    - Leaders distributed across nodes
    - Writes go to leader, replicated via Raft
    - Reads can go to any replica (with lease)
```

### 2. Raft Consensus

```
Raft Write Path:
─────────────────────────────────────────────────────────────────

Client → Leader Node
         │
         ▼
    ┌─────────────────┐
    │  Propose Write  │
    └────────┬────────┘
             │
    ┌────────▼────────┐
    │  Replicate to   │
    │  Followers      │
    └────────┬────────┘
             │
    ┌────────▼────────┐
    │ Majority ACK    │
    │ (2 of 3)        │
    └────────┬────────┘
             │
    ┌────────▼────────┐
    │  Commit Entry   │
    │  Apply to State │
    └────────┬────────┘
             │
    ┌────────▼────────┐
    │  ACK to Client  │
    └─────────────────┘

Durability: Majority must persist before commit
Consistency: Serializable isolation guaranteed
```

### 3. Multi-Region Capabilities

```
Survival Goals:
─────────────────────────────────────────────────────────────────

ZONE Survival:
    - Survive single zone/rack failure
    - Replicas spread across zones
    - Low latency (same region)

REGION Survival:
    - Survive entire region failure
    - Replicas in 3+ regions
    - Higher latency (cross-region)

Table Locality:
    - GLOBAL: Low-latency reads everywhere, cross-region writes
    - REGIONAL BY ROW: Row placed in specified region
    - REGIONAL BY TABLE: Table pinned to region
```

## How HA Works

### Node Failure

```
Node Failure Recovery:
─────────────────────────────────────────────────────────────────

t=0s    Node2 crashes
        - Raft groups on Node2 lose member

t=0s    Range leadership transfer
        - Ranges where Node2 was leader
        - Election triggers on remaining nodes

t=1s    New leaders elected
        - Node1 or Node3 become leaders
        - Write availability restored

t=10s   Under-replicated ranges detected
        - System notices replica missing

t=30s   Re-replication begins
        - New replicas created on surviving nodes
        - Or wait for Node2 recovery

t=???   Full replication restored
        - Either Node2 rejoins
        - Or new replicas on other nodes

Write availability impact: ~1 second (leader election)
Data loss: Zero (committed to majority)
```

### Network Partition

```
Partition Scenario:
─────────────────────────────────────────────────────────────────

3-node cluster, replication factor 3

    Node1          │         Node2, Node3
      │            │              │
  (minority)       │          (majority)

Range with leader on Node1:
    - Node1 cannot reach majority
    - Cannot commit writes
    - Elections timeout
    - Node2 or Node3 becomes leader
    - Writes continue on majority side

Result: No split brain possible
        Minority partition read-only
        Majority continues normally
```

## Configuration

### Cluster Initialization

```bash
# Start first node (initializes cluster)
cockroach start \
    --insecure \
    --advertise-addr=node1.example.com \
    --join=node1.example.com,node2.example.com,node3.example.com \
    --store=path=/data/cockroach \
    --locality=region=us-east,zone=us-east-1a

# Initialize cluster (once)
cockroach init --insecure --host=node1.example.com

# Start additional nodes
cockroach start \
    --insecure \
    --advertise-addr=node2.example.com \
    --join=node1.example.com,node2.example.com,node3.example.com \
    --store=path=/data/cockroach \
    --locality=region=us-east,zone=us-east-1b
```

### Multi-Region Setup

```sql
-- Set up regions
ALTER DATABASE mydb PRIMARY REGION "us-east1";
ALTER DATABASE mydb ADD REGION "us-west1";
ALTER DATABASE mydb ADD REGION "eu-west1";

-- Configure survival goal
ALTER DATABASE mydb SURVIVE REGION FAILURE;

-- Global table (low-latency reads everywhere)
CREATE TABLE countries (
    code STRING PRIMARY KEY,
    name STRING
) LOCALITY GLOBAL;

-- Regional by row (data locality)
CREATE TABLE users (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    region STRING NOT NULL,
    email STRING,
    CONSTRAINT region_check CHECK (region IN ('us-east1', 'us-west1', 'eu-west1'))
) LOCALITY REGIONAL BY ROW AS region;

-- Regional by table
CREATE TABLE us_orders (
    id UUID PRIMARY KEY
) LOCALITY REGIONAL BY TABLE IN PRIMARY REGION;
```

### Connection String

```python
# CockroachDB uses PostgreSQL protocol
import psycopg2

# Single node
conn = psycopg2.connect(
    "postgresql://root@node1.example.com:26257/mydb?sslmode=require"
)

# Multi-node (client-side failover)
conn = psycopg2.connect(
    "postgresql://root@node1:26257,node2:26257,node3:26257/mydb?sslmode=require"
)

# CockroachDB Cloud
conn = psycopg2.connect(
    "postgresql://user:password@free-tier.gcp-us-central1.cockroachlabs.cloud:26257/mydb?sslmode=verify-full"
)
```

## Happy Path Scenarios

### Scenario 1: Automatic Rebalancing

```
Timeline: Adding nodes to cluster
─────────────────────────────────────────────────────────────────

Initial: 3 nodes, 300 ranges, 100 ranges/node

t=0s    Add Node4 to cluster
        $ cockroach start --join=... --locality=...

t=10s   Node4 joins cluster
        - Gossip protocol spreads info
        - Node4 has 0 ranges

t=30s   Rebalancing begins
        - CockroachDB detects imbalance
        - Plans range movements

t=5min  Gradual rebalancing
        - Ranges transferred to Node4
        - Background process, doesn't impact queries

t=30min Rebalancing complete
        - 75 ranges per node
        - Cluster balanced

Automatic, zero-downtime capacity expansion
```

### Scenario 2: Cross-Region Transaction

```
Timeline: Globally consistent write
─────────────────────────────────────────────────────────────────

User in EU writes, table replicated globally

t=0ms   Client in EU: INSERT INTO orders VALUES (...)
        - Connects to EU node

t=1ms   EU node receives query
        - Parses SQL
        - Determines affected ranges

t=5ms   Transaction begins
        - Acquires locks
        - Finds range leaders

t=50ms  Cross-region coordination
        - Range leader might be in US
        - Raft replication across regions
        - EU → US → Asia (for 3-region)

t=150ms Majority replicas ACK
        - 2 of 3 regions confirmed
        - Commit can proceed

t=160ms Transaction committed
        - Globally consistent
        - Serializable isolation

Latency: ~160ms (cross-region)
Consistency: Full ACID, serializable
```

### Scenario 3: Zero-Downtime Schema Change

```
Timeline: Online ALTER TABLE
─────────────────────────────────────────────────────────────────

t=0s    ALTER TABLE users ADD COLUMN phone STRING;

t=1s    Schema change job created
        - Non-blocking
        - Tracked in system tables

t=2s    New schema version published
        - New writes include column
        - Old reads compatible

t=10s   Backfill begins (if needed)
        - Background process
        - Rate limited

t=5min  Backfill complete
        - All rows updated

t=5min  Old schema version dropped
        - Migration complete

Zero downtime, concurrent reads/writes throughout
```

## Unhappy Path Scenarios

### Scenario 1: Node Failure During Transaction

```
Timeline: Transaction in progress when node dies
─────────────────────────────────────────────────────────────────

t=0ms   Transaction started on Node1
        BEGIN; INSERT INTO t VALUES (1);

t=50ms  Node1 crashes
        - Transaction incomplete
        - Intents written but not committed

t=100ms Client connection lost
        - Transaction aborted (no commit)
        - Intents cleaned up by other nodes

t=1s    Client retries
        - Connects to Node2
        - Starts new transaction

t=1.1s  Transaction succeeds
        - Node2 handles request
        - No data loss (nothing committed)

Impact: Uncommitted transaction lost
        Client must retry
        Committed data safe
```

### Scenario 2: Majority Failure

```
Timeline: 2 of 3 nodes fail
─────────────────────────────────────────────────────────────────

t=0s    Node2 and Node3 crash simultaneously

t=0s    Node1 cannot reach majority
        - Raft requires 2/3 for consensus
        - Cannot elect leaders

t=1s    All ranges unavailable
        - No writes possible
        - No consistent reads possible

t=???   Recovery options:

Option A: Wait for nodes to recover
        - Node2 or Node3 comes back
        - Quorum restored
        - Automatic recovery

Option B: Unsafe recovery (data loss risk)
        $ cockroach debug recover
        - Force single-node operation
        - May lose uncommitted data
        - Last resort

Impact: Complete outage until quorum restored
```

### Scenario 3: Clock Skew

```
Timeline: Significant clock drift
─────────────────────────────────────────────────────────────────

CockroachDB relies on clocks for transaction ordering

t=0s    Node3 clock drifts 600ms ahead
        - Beyond max-offset (500ms default)

t=1s    Node3 starts failing health checks
        - Clock skew detected

t=2s    Node3 self-isolates
        - Refuses to serve requests
        - Prevents consistency issues

t=3s    Ranges on Node3 elect new leaders
        - Service continues on Node1, Node2

t=???   Fix Node3's clock
        - NTP sync
        - Node rejoins cluster

Impact: Node removed from service
        No data inconsistency
        Requires clock fix

Mitigation: Use NTP, monitor clock skew
```

### Scenario 4: Hot Range

```
Timeline: Single key receives excessive traffic
─────────────────────────────────────────────────────────────────

t=0s    Popular product ID = 1
        - All users reading/writing same key
        - Single range handles all traffic

t=10s   Range leader overloaded
        - CPU maxed
        - Latency increasing

t=30s   Symptoms:
        - Slow queries for product 1
        - Other queries unaffected
        - Range leader bottleneck

Mitigations:
        1. Hash-sharded index:
           CREATE INDEX ON products (id) USING HASH

        2. Application caching
           - Cache reads in Redis/Memcached

        3. Read replicas (follower reads)
           SET CLUSTER SETTING kv.follower_read.target_duration = '5s';

Impact: Single key bottleneck
        CockroachDB scales horizontally
        But single key has limits
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Serializable** | Strongest consistency guarantee |
| **Automatic Sharding** | No manual shard management |
| **Multi-Region** | Built-in geo-distribution |
| **Zero Data Loss** | Raft consensus on every write |
| **Online Schema** | Non-blocking DDL |
| **PostgreSQL Compatible** | Works with PG drivers |
| **Horizontal Scale** | Add nodes, automatic rebalancing |
| **Self-Healing** | Automatic re-replication |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Write Latency** | Consensus overhead |
| **Cross-Region Latency** | Physics (speed of light) |
| **Not Full PostgreSQL** | Many features unsupported |
| **Complexity** | Distributed system complexity |
| **Hot Spots** | Single-key scaling limits |
| **Cost** | Enterprise features paid |
| **Learning Curve** | New concepts (ranges, localities) |

## PostgreSQL Compatibility

### Supported
- Basic SQL (SELECT, INSERT, UPDATE, DELETE)
- Joins, subqueries, CTEs
- Transactions (serializable)
- Many data types
- Basic functions
- Primary keys, foreign keys
- Indexes (B-tree, hash, GIN, inverted)

### Not Supported
- Stored procedures (limited)
- Triggers
- Full-text search (use inverted indexes)
- Some data types (geometric, etc.)
- Extensions (pg_stat_statements, PostGIS, etc.)
- Sequences (use UUID instead)
- Advisory locks

## Comparison with Alternatives

| Feature | CockroachDB | YugabyteDB | PostgreSQL | Aurora |
|---------|-------------|------------|------------|--------|
| Consistency | Serializable | Configurable | Serializable | Serializable |
| Sharding | Automatic | Automatic | Manual | None |
| Multi-Region | Built-in | Built-in | Manual | Global DB |
| PostgreSQL Compat | Partial | Higher | Full | Full |
| Distributed Txns | Yes | Yes | No | No |
| Write Scaling | Yes | Yes | No | No |

## Limitations

1. **PostgreSQL Incompatibilities**: Not all features supported
2. **Write Latency**: Consensus adds latency
3. **Single-Key Bottleneck**: Hot spots require careful design
4. **Clock Requirements**: Depends on synchronized clocks
5. **Learning Curve**: Complex distributed system
6. **Enterprise Features**: Some features require license
7. **Resource Requirements**: More resources than single-node
8. **Debugging Complexity**: Distributed system harder to debug

## Best Practices

1. **Use UUIDs**: Avoid sequences for distributed keys
2. **Design for Distribution**: Consider data locality
3. **Monitor Clock Skew**: Critical for correctness
4. **Plan for Hot Spots**: Hash keys that get heavy traffic
5. **Use Multi-Host Connection Strings**: Client-side failover
6. **Set Appropriate Localities**: Enable smart routing
7. **Monitor Range Distribution**: Watch for imbalances
8. **Test Failure Scenarios**: Verify HA behavior

## Conclusion

CockroachDB provides a compelling option for applications needing horizontal scale with strong consistency. Its Raft-based replication ensures zero data loss, while automatic sharding and rebalancing reduce operational burden. The main trade-off is latency (consensus overhead) and incomplete PostgreSQL compatibility. It's ideal for globally distributed applications willing to accept these trade-offs.

**Recommended for:**
- Multi-region deployments needing consistency
- Applications requiring horizontal write scaling
- Teams comfortable with distributed systems
- Workloads tolerant of consensus latency

**Not recommended for:**
- Applications requiring full PostgreSQL compatibility
- Single-region, latency-sensitive workloads
- Workloads with severe hot spots
- Teams without distributed systems experience
