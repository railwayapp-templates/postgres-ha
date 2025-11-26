# RFC-008: Citus - Distributed PostgreSQL for Horizontal Scaling

## Overview

Citus is a PostgreSQL extension that transforms PostgreSQL into a distributed database. Acquired by Microsoft in 2019 and now fully open source, Citus enables horizontal scaling by sharding tables across multiple PostgreSQL nodes. Unlike traditional HA solutions, Citus provides both high availability (through replication) and horizontal scalability (through sharding).

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ Standard PostgreSQL Protocol
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                     Coordinator Node(s)                         │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │                  PostgreSQL + Citus Extension              ││
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ ││
│  │  │   Query     │  │   Shard     │  │   Distributed       │ ││
│  │  │   Planner   │  │   Map       │  │   Transaction Mgr   │ ││
│  │  └─────────────┘  └─────────────┘  └─────────────────────┘ ││
│  └─────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│  Worker Node  │     │  Worker Node  │     │  Worker Node  │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │PostgreSQL│ │     │  │PostgreSQL│ │     │  │PostgreSQL│ │
│  │+ Citus  │  │     │  │+ Citus  │  │     │  │+ Citus  │  │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │Shard 1  │  │     │  │Shard 2  │  │     │  │Shard 3  │  │
│  │Shard 4  │  │     │  │Shard 5  │  │     │  │Shard 6  │  │
│  │Shard 7  │  │     │  │Shard 8  │  │     │  │Shard 9  │  │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
└───────────────┘     └───────────────┘     └───────────────┘
        │                     │                     │
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│Worker Standby │     │Worker Standby │     │Worker Standby │
│  (Streaming   │     │  (Streaming   │     │  (Streaming   │
│  Replication) │     │  Replication) │     │  Replication) │
└───────────────┘     └───────────────┘     └───────────────┘
```

## Core Concepts

### 1. Table Types

```sql
-- Distributed Table (sharded across workers)
SELECT create_distributed_table('orders', 'customer_id');

-- Reference Table (replicated to all workers)
SELECT create_reference_table('countries');

-- Local Table (only on coordinator)
-- Regular PostgreSQL table, not distributed
```

### 2. Sharding

```
Hash-Based Sharding:
─────────────────────────────────────────────────────────────────

customer_id = 12345
     │
     ▼
hash(12345) = 0x7A3B...
     │
     ▼
shard_id = hash % shard_count
     │
     ▼
shard_id = 7 → Worker Node 2

All rows with same distribution key (customer_id)
go to same shard → enables co-located joins
```

### 3. Query Routing

```
Query Types:
─────────────────────────────────────────────────────────────────

Router Query (single shard):
    SELECT * FROM orders WHERE customer_id = 12345;
    → Routes directly to one worker

Distributed Query (all shards):
    SELECT COUNT(*) FROM orders;
    → Parallel execution on all workers
    → Results aggregated on coordinator

Co-located Join:
    SELECT * FROM orders o JOIN order_items i
    ON o.customer_id = i.customer_id AND o.id = i.order_id;
    → Executes locally on each worker (same distribution key)

Cross-Shard Join:
    SELECT * FROM orders o JOIN products p ON o.product_id = p.id;
    → Requires data shuffling between workers
    → More expensive
```

## High Availability in Citus

### Coordinator HA

```
Coordinator HA Options:
─────────────────────────────────────────────────────────────────

Option 1: Streaming Replication + Patroni
    - Coordinator is PostgreSQL
    - Standard Patroni/repmgr/pg_auto_failover
    - Standby coordinator promotes on failure

Option 2: Multi-Coordinator (Citus Enterprise / Azure)
    - Multiple active coordinators
    - Load balanced
    - Share same worker set

Option 3: Coordinator on Every Worker (schema-based sharding)
    - No dedicated coordinator
    - Any node can be entry point
```

### Worker HA

```
Worker HA Options:
─────────────────────────────────────────────────────────────────

Option 1: Streaming Replication
    - Each worker has standby
    - Patroni/repmgr manages failover
    - Coordinator tracks active workers

Option 2: Shard Replication (Citus built-in)
    - citus.shard_replication_factor = 2
    - Shards copied to multiple workers
    - Coordinator routes to healthy copy

Option 3: Combined
    - Streaming replication for each worker
    - Shard replication for additional safety
```

### Shard Replication

```sql
-- Set replication factor (before creating distributed tables)
SET citus.shard_replication_factor = 2;

-- Create distributed table (shards replicated)
SELECT create_distributed_table('orders', 'customer_id');

-- Check shard placements
SELECT * FROM pg_dist_shard_placement;

-- Result shows each shard on 2 workers:
-- shard_id | shard_state | worker_node
-- 102008   | 1           | worker1
-- 102008   | 1           | worker2  (replica)
-- 102009   | 1           | worker2
-- 102009   | 1           | worker3  (replica)
```

## Configuration

### Coordinator Setup

```sql
-- Install extension
CREATE EXTENSION citus;

-- Add worker nodes
SELECT citus_add_node('worker1.example.com', 5432);
SELECT citus_add_node('worker2.example.com', 5432);
SELECT citus_add_node('worker3.example.com', 5432);

-- Verify cluster
SELECT * FROM citus_get_active_worker_nodes();

-- Configure distributed tables
SET citus.shard_count = 32;
SET citus.shard_replication_factor = 2;

-- Create distributed table
SELECT create_distributed_table('events', 'tenant_id');
```

### postgresql.conf (All Nodes)

```ini
# Citus settings
shared_preload_libraries = 'citus'

# Worker connection pooling
citus.max_adaptive_executor_pool_size = 16

# Query execution
citus.multi_shard_modify_mode = 'parallel'
citus.enable_repartition_joins = on

# HA settings
citus.node_conninfo = 'sslmode=require'
citus.use_secondary_nodes = 'always'  # Route reads to standbys

# Standard PostgreSQL HA
wal_level = replica
max_wal_senders = 10
max_replication_slots = 10
```

### pg_hba.conf

```
# Allow coordinator to connect to workers
host    all         citus_user    coordinator_ip/32    scram-sha-256

# Allow worker-to-worker connections (for repartition joins)
host    all         citus_user    worker_network/24    scram-sha-256

# Allow replication
host    replication replicator    standby_network/24   scram-sha-256
```

## Happy Path Scenarios

### Scenario 1: Distributed Query Execution

```
Timeline: Multi-tenant analytics query
─────────────────────────────────────────────────────────────────

Query: SELECT tenant_id, COUNT(*), SUM(amount)
       FROM orders
       GROUP BY tenant_id;

t=0ms   Coordinator receives query
        - Parses and plans
        - Identifies as distributed query

t=1ms   Coordinator generates fragment queries
        - One per shard group
        - Parallel execution plan

t=2ms   Fragments sent to workers
        Worker1: SELECT tenant_id, COUNT(*), SUM(amount)
                 FROM orders_102008 GROUP BY tenant_id;
        Worker2: SELECT tenant_id, COUNT(*), SUM(amount)
                 FROM orders_102009 GROUP BY tenant_id;
        Worker3: SELECT tenant_id, COUNT(*), SUM(amount)
                 FROM orders_102010 GROUP BY tenant_id;

t=50ms  Workers return partial results
        - Parallel execution complete

t=55ms  Coordinator aggregates results
        - Combines tenant_id groups
        - Final SUM and COUNT

t=60ms  Result returned to client

Benefit: 3x parallelism, scales with workers
```

### Scenario 2: Router Query (Single Tenant)

```
Timeline: Single-tenant lookup
─────────────────────────────────────────────────────────────────

Query: SELECT * FROM orders WHERE tenant_id = 'acme-corp';

t=0ms   Coordinator receives query
        - Identifies tenant_id filter
        - Calculates shard: hash('acme-corp') % 32 = 7

t=1ms   Route directly to worker
        - Shard 7 lives on Worker2
        - Direct query execution

t=10ms  Worker2 returns results
        - No aggregation needed

t=12ms  Result returned to client

Benefit: Single network hop, minimal coordinator work
```

### Scenario 3: Online Shard Rebalancing

```
Timeline: Adding new worker node
─────────────────────────────────────────────────────────────────

Initial: 3 workers, 32 shards each (96 placements)
Goal: Add worker4, rebalance to 24 shards each

t=0s    Add new worker
        SELECT citus_add_node('worker4.example.com', 5432);

t=5s    Start rebalancing
        SELECT citus_rebalance_start();

t=5s    Background rebalancer activates
        - Identifies shard movements needed
        - Plans minimal data movement

t=10s   Shard moves begin (non-blocking)
        - Logical replication of shard data
        - Writes continue to old location

t=30min Shard moves complete
        - 24 shards moved to worker4
        - Atomic cutover

t=30min+1s New shard map active
        - Queries route to new locations
        - Old shard copies cleaned up

Benefit: Zero downtime rebalancing
```

## Unhappy Path Scenarios

### Scenario 1: Worker Node Failure

```
Timeline: Worker2 crashes
─────────────────────────────────────────────────────────────────

Configuration: shard_replication_factor = 2

t=0s    Worker2 crashes
        - Shards 102009, 102012, 102015 on worker2
        - Each has replica on another worker

t=5s    Coordinator detects failure
        - Health check to worker2 fails

t=10s   Coordinator updates shard map
        - Routes to replica placements
        - Shard 102009 → worker3 (replica)
        - Shard 102012 → worker1 (replica)
        - Shard 102015 → worker3 (replica)

t=15s   Queries continue
        - Using replica placements
        - Slight performance impact (fewer workers)

t=???   Worker2 recovers
        - Coordinator detects healthy
        - Shards re-synchronized if needed
        - Full capacity restored

Impact with replication: Brief query failures during detection
Impact without replication: Affected shards unavailable
```

### Scenario 2: Worker Failure WITHOUT Replication

```
Timeline: Worker2 crashes (replication_factor = 1)
─────────────────────────────────────────────────────────────────

t=0s    Worker2 crashes

t=5s    Coordinator detects failure

t=6s    Queries to worker2's shards fail
        ERROR: could not connect to worker node

t=???   Recovery options:

Option A: Worker2 recovers
        - Service resumes
        - No data loss

Option B: Worker2 cannot recover
        - Restore from backup
        - Or accept data loss for those shards

Impact: Partial availability
        Shards on worker2 inaccessible
        Other shards continue working
```

### Scenario 3: Coordinator Failure

```
Timeline: Coordinator crashes
─────────────────────────────────────────────────────────────────

Without Coordinator HA:
─────────────────────────────────────────────────────────────────

t=0s    Coordinator crashes

t=1s    All client connections lost
        - No query routing possible
        - Workers still running but unreachable

t=???   Manual recovery required
        - Restore coordinator from backup
        - Or rebuild from metadata

Impact: Complete outage until recovery

With Coordinator HA (Patroni):
─────────────────────────────────────────────────────────────────

t=0s    Coordinator primary crashes

t=30s   Patroni promotes standby coordinator
        - Has replicated shard metadata
        - Has connection info to workers

t=35s   Service resumes
        - Clients reconnect
        - Query routing continues

Impact: 30-60 second outage
```

### Scenario 4: Cross-Shard Transaction Failure

```
Timeline: Distributed transaction partial failure
─────────────────────────────────────────────────────────────────

Query: BEGIN;
       UPDATE accounts SET balance = balance - 100
       WHERE tenant_id = 'a' AND user_id = 1;

       UPDATE accounts SET balance = balance + 100
       WHERE tenant_id = 'b' AND user_id = 2;
       COMMIT;

Shards: tenant_id='a' → Worker1
        tenant_id='b' → Worker2

t=0ms   BEGIN on coordinator
        - Starts 2PC coordinator

t=5ms   UPDATE on Worker1
        - Shard locked
        - Prepare succeeds

t=10ms  UPDATE on Worker2 fails
        - Disk full error
        - Prepare fails

t=11ms  Coordinator detects failure
        - Cannot commit distributed transaction

t=12ms  ROLLBACK sent to all workers
        - Worker1: Rollback prepared transaction
        - Worker2: Already failed

t=15ms  Transaction aborted
        - Atomicity preserved
        - No partial commit

Impact: Transaction fails but data consistent
```

### Scenario 5: Shard Map Inconsistency

```
Timeline: Metadata corruption/inconsistency
─────────────────────────────────────────────────────────────────

Cause: Network partition during shard move

t=0s    Shard move in progress
        - Shard 102008 moving from worker1 to worker3

t=5s    Network partition
        - Coordinator loses worker1
        - worker3 completes shard copy

t=10s   Partition heals
        - Coordinator uncertain of shard state

t=15s   Queries may fail
        - Routing to wrong worker
        - Or both workers have shard

Recovery:
        SELECT citus_check_cluster_state();
        -- Identifies inconsistencies

        SELECT citus_fix_shard_placements();
        -- Repairs metadata

Impact: Requires manual intervention
        Potential query failures during repair
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Horizontal Scale** | Linear scale-out by adding workers |
| **PostgreSQL Compatible** | Standard SQL, extensions work |
| **Multi-Tenant** | Excellent for SaaS workloads |
| **Online Operations** | Add nodes, rebalance without downtime |
| **Query Parallelism** | Distributed execution |
| **Reference Tables** | Small tables replicated everywhere |
| **Open Source** | Full feature set now open source |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Distribution Key** | Must choose wisely, hard to change |
| **Cross-Shard Joins** | Expensive, require data shuffling |
| **Distributed Transactions** | 2PC overhead |
| **Coordinator SPOF** | Without HA, single point of failure |
| **Operational Complexity** | More components to manage |
| **Not All Queries** | Some PostgreSQL features unsupported |
| **Shard Management** | Rebalancing requires planning |

## Key Configuration Parameters

```sql
-- Shard configuration
citus.shard_count = 32                    -- Shards per distributed table
citus.shard_replication_factor = 2         -- Copies of each shard

-- Query execution
citus.max_adaptive_executor_pool_size = 16 -- Parallel connections
citus.enable_repartition_joins = on        -- Allow cross-shard joins
citus.task_executor_type = 'adaptive'      -- Execution mode

-- HA settings
citus.use_secondary_nodes = 'always'       -- Route reads to standbys
citus.cluster_name = 'production'          -- For multi-cluster
```

## When to Choose Citus

### Good Fit
- Multi-tenant SaaS applications
- Time-series data with tenant partitioning
- Analytics workloads needing scale-out
- Workloads with clear distribution key
- PostgreSQL compatibility required

### Poor Fit
- Small databases (< 100GB)
- Heavy cross-tenant queries
- Complex transactions across many tenants
- Workloads without natural distribution key
- Need for immediate consistency everywhere

## Comparison with Alternatives

| Feature | Citus | CockroachDB | YugabyteDB | Aurora |
|---------|-------|-------------|------------|--------|
| Sharding | Hash-based | Range-based | Hash/Range | None |
| PostgreSQL Compat | Extension | Wire protocol | Extension | Native |
| Distributed Txns | 2PC | Serializable | 2PC | N/A |
| Auto-Sharding | Manual | Automatic | Automatic | N/A |
| Rebalancing | Online | Automatic | Automatic | N/A |
| Multi-Region | Limited | Built-in | Built-in | Yes |

## Limitations

1. **Distribution Key Immutability**: Cannot change distribution column easily
2. **Cross-Shard Operations**: Expensive and slower
3. **Full SQL Support**: Not all PostgreSQL features work distributed
4. **Sequences**: Distributed sequences complex
5. **Foreign Keys**: Limited across distributed tables
6. **Coordinator Bottleneck**: All queries go through coordinator
7. **Complex Queries**: Some require careful optimization
8. **Backup/Restore**: Must coordinate across all nodes

## Best Practices

1. **Choose Distribution Key Wisely**: tenant_id for multi-tenant
2. **Use Reference Tables**: For small lookup tables
3. **Co-locate Related Tables**: Same distribution key
4. **Plan Shard Count**: 2-4x expected worker count
5. **Implement Coordinator HA**: Use Patroni or equivalent
6. **Monitor Shard Sizes**: Rebalance before skew
7. **Use Connection Pooling**: PgBouncer on coordinator
8. **Test Failover**: Regular HA drills

## Conclusion

Citus transforms PostgreSQL into a horizontally scalable distributed database while maintaining PostgreSQL compatibility. It excels at multi-tenant SaaS workloads where the distribution key aligns with query patterns. For pure HA without horizontal scaling, traditional solutions (Patroni, repmgr) are simpler. Citus shines when you need both HA and the ability to scale beyond a single node.

**Recommended for:**
- Multi-tenant SaaS applications
- Large-scale analytics with PostgreSQL
- Time-series data at scale
- Workloads outgrowing single-node PostgreSQL

**Not recommended for:**
- Small databases (< 100GB)
- Workloads without clear distribution key
- Heavy cross-tenant analytics
- Teams without distributed systems experience
