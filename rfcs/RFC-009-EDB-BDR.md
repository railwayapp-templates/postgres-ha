# RFC-009: EDB Postgres Distributed (BDR) - Multi-Master Replication

## Overview

EDB Postgres Distributed (formerly BDR - Bi-Directional Replication) is a commercial multi-master replication solution from EnterpriseDB. It enables active-active deployments where writes can occur on any node, with asynchronous or synchronous replication between nodes. BDR is designed for global distribution with sub-second failover and conflict resolution.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Global Application                          │
└─────────────────────────────────────────────────────────────────┘
        │                     │                     │
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│   Region A    │     │   Region B    │     │   Region C    │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │ PGD     │  │     │  │ PGD     │  │     │  │ PGD     │  │
│  │ Proxy   │  │     │  │ Proxy   │  │     │  │ Proxy   │  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │PostgreSQL│ │     │  │PostgreSQL│ │     │  │PostgreSQL│ │
│  │  + BDR  │◄─┼─────┼─►│  + BDR  │◄─┼─────┼─►│  + BDR  │ │
│  │(Active) │  │     │  │(Active) │  │     │  │(Active) │ │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │ Shadow  │  │     │  │ Shadow  │  │     │  │ Shadow  │  │
│  │ Node    │  │     │  │ Node    │  │     │  │ Node    │  │
│  │(Standby)│  │     │  │(Standby)│  │     │  │(Standby)│  │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
└───────────────┘     └───────────────┘     └───────────────┘
        │                     │                     │
        └─────────────────────┴─────────────────────┘
                    Logical Replication
                    (All nodes connected)
```

## Core Concepts

### 1. Node Types

```
Node Types in BDR:
─────────────────────────────────────────────────────────────────

Data Node (Active)
    - Full participant in replication
    - Accepts reads and writes
    - Part of logical replication mesh

Shadow Node (Standby)
    - Physical replica of a data node
    - Streaming replication from its parent
    - Promotes if data node fails
    - Does not participate in logical replication

Witness Node
    - Voting member for consensus
    - No data, just for quorum
    - Used in 2-node deployments

Subscriber-Only Node
    - Receives data but doesn't publish
    - For read replicas or analytics
```

### 2. Replication Mechanisms

```
BDR uses Logical Replication:
─────────────────────────────────────────────────────────────────

        Node A                    Node B
           │                         │
    ┌──────┴──────┐           ┌──────┴──────┐
    │   Tables    │           │   Tables    │
    │   (data)    │           │   (data)    │
    └──────┬──────┘           └──────┬──────┘
           │                         │
    ┌──────▼──────┐           ┌──────▼──────┐
    │  WAL Decode │           │  WAL Decode │
    │  (publish)  │           │  (publish)  │
    └──────┬──────┘           └──────┬──────┘
           │                         │
           └─────────────────────────┘
                Logical Replication
                   (both ways)

Benefits:
- Only committed changes replicated
- Selective replication possible
- Cross-version compatible
- DDL can be replicated
```

### 3. Conflict Resolution

```
Conflict Types:
─────────────────────────────────────────────────────────────────

INSERT/INSERT Conflict:
    Node A: INSERT INTO t (id=1, val='A')
    Node B: INSERT INTO t (id=1, val='B')
    → Same primary key inserted on both nodes

UPDATE/UPDATE Conflict:
    Node A: UPDATE t SET val='A' WHERE id=1
    Node B: UPDATE t SET val='B' WHERE id=1
    → Same row updated on both nodes

UPDATE/DELETE Conflict:
    Node A: UPDATE t SET val='A' WHERE id=1
    Node B: DELETE FROM t WHERE id=1
    → Row updated on one, deleted on other

Resolution Strategies:
    - update_if_newer: Most recent timestamp wins
    - update_always: Last received wins
    - update_origin_change: Origin node wins
    - Custom: User-defined functions
```

## Commit Scopes

### Group Commit

```sql
-- Configure commit scope requiring confirmation from 2 nodes
SELECT bdr.create_commit_scope(
    commit_scope_name := 'dc_scope',
    origin_node_group := 'world',
    rule := 'ANY 2 (region_a, region_b, region_c)',
    wait_for_ready := true
);

-- Use for specific transaction
SET bdr.commit_scope = 'dc_scope';
BEGIN;
INSERT INTO important_data VALUES (...);
COMMIT;  -- Waits for 2 nodes to confirm

-- Or set as default for a table
SELECT bdr.alter_table_commit_scope('important_data', 'dc_scope');
```

### CAMO (Commit At Most Once)

```sql
-- CAMO prevents duplicate commits during failover
SELECT bdr.create_commit_scope(
    commit_scope_name := 'camo_scope',
    origin_node_group := 'world',
    rule := 'CAMO DEGRADE ON (region_a_partner)',
    wait_for_ready := true
);

-- Transaction with CAMO protection
SET bdr.commit_scope = 'camo_scope';
BEGIN;
-- Critical financial transaction
UPDATE accounts SET balance = balance - 100 WHERE id = 1;
UPDATE accounts SET balance = balance + 100 WHERE id = 2;
COMMIT;  -- CAMO ensures exactly-once semantics
```

## Configuration

### Node Setup

```sql
-- Create BDR extension
CREATE EXTENSION bdr;

-- Initialize BDR group (first node)
SELECT bdr.create_node(
    node_name := 'node_a',
    local_dsn := 'host=node-a.example.com dbname=mydb'
);

SELECT bdr.create_node_group(
    node_group_name := 'mygroup'
);

-- Join additional nodes
-- On node_b:
SELECT bdr.create_node(
    node_name := 'node_b',
    local_dsn := 'host=node-b.example.com dbname=mydb'
);

SELECT bdr.join_node_group(
    join_target_dsn := 'host=node-a.example.com dbname=mydb',
    node_group_name := 'mygroup'
);
```

### postgresql.conf

```ini
# BDR settings
shared_preload_libraries = 'bdr'

# Replication settings
wal_level = logical
max_replication_slots = 20
max_wal_senders = 20
max_worker_processes = 20

# BDR specific
bdr.default_commit_scope = 'local'
bdr.global_lock_timeout = '10s'
bdr.ddl_replication = on
```

### PGD Proxy Configuration

```yaml
# pgd-proxy.yml
name: proxy_a
listen:
  host: 0.0.0.0
  port: 6432

endpoints:
  - name: mygroup
    dsn: host=node-a.example.com,node-b.example.com dbname=mydb
    default_pool_size: 20
    routing: leader

read_routing: round-robin
write_routing: leader
```

## Happy Path Scenarios

### Scenario 1: Normal Multi-Master Operations

```
Timeline: Writes to multiple regions simultaneously
─────────────────────────────────────────────────────────────────

t=0ms   User in US writes to Node A
        INSERT INTO orders (customer='US-123', amount=100);

t=0ms   User in EU writes to Node B (same time)
        INSERT INTO orders (customer='EU-456', amount=200);

t=1ms   Both commits succeed locally
        - No conflict (different customers)
        - WAL generated on both nodes

t=10ms  Logical replication propagates
        - Node A's change → Node B
        - Node B's change → Node A

t=20ms  Both nodes fully consistent
        - All orders visible everywhere
        - No conflicts to resolve

Benefit: True active-active, local latency for all users
```

### Scenario 2: Commit Scope with Synchronous Confirmation

```
Timeline: Critical transaction with durability guarantee
─────────────────────────────────────────────────────────────────

SET bdr.commit_scope = 'any_2_regions';
BEGIN;
UPDATE accounts SET balance = balance - 1000000 WHERE id = 1;
COMMIT;

t=0ms   Transaction executed on Node A (origin)
        - Data modified locally

t=1ms   COMMIT issued
        - Requires ANY 2 nodes to confirm
        - Node A prepares

t=50ms  Node B receives replication stream
        - Confirms receipt

t=100ms Node C receives replication stream
        - Confirms receipt

t=101ms Node A receives confirmations
        - 2 nodes confirmed (A + B, or A + C)
        - COMMIT completes

t=102ms Response to client
        - Transaction durable on 2+ nodes

Guarantee: If Node A fails immediately after, data safe
```

### Scenario 3: Planned Switchover

```
Timeline: Maintenance switchover using PGD Proxy
─────────────────────────────────────────────────────────────────

t=0s    Notify proxy of planned maintenance
        $ pgd-cli switchover --group mygroup --to node_b

t=1s    Proxy begins draining Node A
        - No new connections to Node A
        - Existing connections complete

t=5s    Wait for replication catch-up
        - Ensure Node B has all data

t=6s    Proxy routes to Node B
        - New write leader

t=7s    Node A maintenance begins
        - Upgrade, patch, etc.

t=30min Node A returns
        - Rejoins group
        - Catches up via replication

t=31min Optional: switchover back
        - Or continue with Node B as leader

Zero data loss, minimal downtime
```

## Unhappy Path Scenarios

### Scenario 1: Data Node Failure

```
Timeline: Active node crashes
─────────────────────────────────────────────────────────────────

Group: Node A (active), Node B (active), Node A-shadow (standby)

t=0s    Node A crashes

t=1s    PGD Proxy detects failure
        - Health checks fail

t=2s    Proxy routes traffic away from Node A
        - Writes go to Node B

t=5s    Shadow node promotion considered
        - If Node A won't recover quickly
        - Shadow promotes to take Node A's place

t=10s   Shadow promoted (if needed)
        - Becomes new data node
        - Joins logical replication mesh
        - Proxy routes to shadow

t=15s   Full service restored
        - Node B active
        - Shadow (now Node A-new) active

Failover time: ~5-15 seconds
Data loss: Depends on commit scope (potentially zero)
```

### Scenario 2: Conflict Resolution

```
Timeline: Same row updated on two nodes simultaneously
─────────────────────────────────────────────────────────────────

t=0ms   Node A: UPDATE users SET email='a@example.com' WHERE id=1;
t=0ms   Node B: UPDATE users SET email='b@example.com' WHERE id=1;

t=1ms   Both commits succeed locally (different nodes)

t=50ms  Replication propagates

t=51ms  Node A receives Node B's change
        - Conflict detected!
        - Same row modified

t=52ms  Conflict resolver runs
        - Strategy: update_if_newer
        - Compares commit timestamps

t=53ms  Resolution:
        - If Node B's timestamp newer → email='b@example.com'
        - If Node A's timestamp newer → email='a@example.com'

t=54ms  Resolution applied on Node A
        - Conflict logged for audit

t=60ms  Both nodes consistent
        - Same winner applied everywhere

Note: Conflict resolution is automatic but should be rare
      Application design should minimize conflicts
```

### Scenario 3: Network Partition

```
Timeline: Network splits BDR cluster
─────────────────────────────────────────────────────────────────

        Region A          │          Region B + C
        Node A            │          Node B, Node C
           │              │              │
           │   partition  │              │
           │              │              │

t=0s    Partition occurs

t=5s    Nodes detect partition
        - Node A cannot reach B or C
        - Node B and C can reach each other

t=10s   Raft consensus evaluation
        - Node A: minority (1 of 3)
        - Node B+C: majority (2 of 3)

t=15s   Node A behavior (minority):
        - Can continue writes (if configured)
        - But commit scopes requiring multiple nodes fail
        - Local writes succeed, will sync later

t=15s   Node B+C behavior (majority):
        - Continue normal operation
        - Commit scopes work within majority

t=60s   Partition heals

t=61s   Reconciliation begins
        - Node A's changes replicate to B, C
        - B, C's changes replicate to A
        - Conflicts resolved if any

Impact: No automatic fencing in default mode
        Commit scopes prevent unsafe commits
```

### Scenario 4: Replication Lag Under Load

```
Timeline: One node falls behind
─────────────────────────────────────────────────────────────────

t=0s    Heavy write load on Node A
        - 10,000 transactions/second

t=60s   Node B replication lag grows
        - Network bottleneck
        - Lag: 5 seconds

t=120s  Lag continues growing
        - Lag: 30 seconds

Impact on commit scopes:
        - Synchronous scopes: commits wait for Node B
        - Async scopes: commits proceed, risk if failover

t=180s  Alert triggered
        - Monitor replication lag
        - Admin investigates

Recovery options:
        - Increase network capacity
        - Throttle writes on Node A
        - Add read replicas to offload Node B

t=300s  Lag reduces
        - Normal operation resumes
```

### Scenario 5: DDL Replication Conflict

```
Timeline: Conflicting schema changes
─────────────────────────────────────────────────────────────────

t=0s    Node A: ALTER TABLE users ADD COLUMN phone VARCHAR(20);
t=0s    Node B: ALTER TABLE users ADD COLUMN phone TEXT;

t=1s    Both DDLs succeed locally

t=10s   DDL replication propagates

t=11s   Conflict detected
        - Same column, different types

t=12s   DDL conflict handling:
        - BDR cannot auto-resolve
        - Requires manual intervention
        - Replication may pause

Recovery:
        - Admin reviews conflicting DDL
        - Decides correct schema
        - Manually resolves on affected nodes
        - Resumes replication

Best practice: Coordinate DDL changes
              Use DDL locking features
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Multi-Master** | True active-active writes |
| **Global Distribution** | Sub-second local latency |
| **Flexible Consistency** | Choose per-transaction |
| **Conflict Resolution** | Automatic with customization |
| **DDL Replication** | Schema changes propagate |
| **Sub-second Failover** | With PGD Proxy |
| **CAMO** | Exactly-once transactions |
| **PostgreSQL Compatible** | Standard PostgreSQL underneath |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Cost** | Commercial license required |
| **Complexity** | Significant operational complexity |
| **Conflict Risk** | Application must minimize conflicts |
| **Latency** | Synchronous scopes add latency |
| **Not ACID Across Nodes** | By default, eventual consistency |
| **Learning Curve** | BDR concepts take time to master |
| **Limited Ecosystem** | Fewer community resources |

## Comparison with Alternatives

| Feature | EDB BDR | CockroachDB | YugabyteDB | Citus |
|---------|---------|-------------|------------|-------|
| Multi-Master | Yes | Yes | Yes | No |
| Conflict Resolution | Yes | Serializable | Configurable | N/A |
| PostgreSQL Compat | Full | Wire protocol | Extension | Extension |
| Consistency | Configurable | Serializable | Configurable | N/A |
| Commercial | Yes | Yes | Yes | Open core |
| Global Tables | Yes | Yes | Yes | No |

## Limitations

1. **Conflict Resolution Complexity**: Requires careful application design
2. **DDL Coordination**: Schema changes need planning
3. **Large Transactions**: May cause replication lag
4. **Sequences**: Global sequences add overhead
5. **Some Features**: Not all PostgreSQL features supported
6. **Cost**: Enterprise pricing required
7. **Vendor Lock-in**: Proprietary extensions

## Best Practices

1. **Minimize Conflicts**: Design schema to reduce collision potential
2. **Use Commit Scopes**: Match durability to criticality
3. **Monitor Replication Lag**: Alert before it becomes critical
4. **Test Failover**: Regular drills with PGD Proxy
5. **Plan DDL Changes**: Coordinate across team
6. **Use CAMO**: For critical financial transactions
7. **Implement Conflict Logging**: Track and review conflicts
8. **Separate Read Traffic**: Use subscriber-only nodes

## Conclusion

EDB Postgres Distributed (BDR) is the premier solution for true multi-master PostgreSQL deployments, particularly for globally distributed applications. Its configurable consistency model allows balancing between performance and durability. The main challenges are operational complexity and the need for careful application design to minimize conflicts.

**Recommended for:**
- Global applications requiring local write latency
- Active-active disaster recovery
- Mission-critical systems needing CAMO
- Organizations with EDB support contracts

**Not recommended for:**
- Simple HA requirements (use Patroni)
- Budget-constrained projects
- Small-scale deployments
- Teams without distributed systems expertise
