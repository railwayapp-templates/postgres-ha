# RFC-014: PlanetScale (Vitess) - MySQL-Compatible Distributed Database

## Overview

PlanetScale is a serverless MySQL-compatible database platform built on Vitess, the open-source database clustering system originally developed at YouTube to scale MySQL. While **not PostgreSQL-compatible**, PlanetScale is included in this document for architectural comparison as it represents a leading approach to horizontally scalable relational databases with innovative features like non-blocking schema changes and database branching.

> **Note**: PlanetScale uses MySQL protocol, not PostgreSQL. This RFC is for comparative purposes.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
│                     (MySQL Protocol)                            │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                        PlanetScale Edge                         │
│              (Global edge network, connection pooling)          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                          VTGate                                 │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │  - Query routing (knows shard locations)                   ││
│  │  - Query rewriting (scatter-gather for cross-shard)        ││
│  │  - Connection pooling to VTTablets                         ││
│  │  - Transaction coordination                                 ││
│  └─────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│   VTTablet    │     │   VTTablet    │     │   VTTablet    │
│   (Shard 1)   │     │   (Shard 2)   │     │   (Shard 3)   │
│  ┌─────────┐  │     │  ┌─────────┐  │     │  ┌─────────┐  │
│  │ Primary │  │     │  │ Primary │  │     │  │ Primary │  │
│  │ MySQL   │  │     │  │ MySQL   │  │     │  │ MySQL   │  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │Replica 1│  │     │  │Replica 1│  │     │  │Replica 1│  │
│  │ MySQL   │  │     │  │ MySQL   │  │     │  │ MySQL   │  │
│  └────┬────┘  │     │  └────┬────┘  │     │  └────┬────┘  │
│       │       │     │       │       │     │       │       │
│  ┌────▼────┐  │     │  ┌────▼────┐  │     │  ┌────▼────┐  │
│  │Replica 2│  │     │  │Replica 2│  │     │  │Replica 2│  │
│  │ MySQL   │  │     │  │ MySQL   │  │     │  │ MySQL   │  │
│  └─────────┘  │     │  └─────────┘  │     │  └─────────┘  │
└───────────────┘     └───────────────┘     └───────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                          VTCtld                                 │
│  - Cluster management (topology, resharding)                    │
│  - Schema management (Online DDL)                               │
│  - Orchestrates VTTablets                                       │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                      Topology Service                           │
│              (etcd / Consul / ZooKeeper)                       │
│  - Shard map, tablet locations, cluster state                  │
└─────────────────────────────────────────────────────────────────┘
```

## Core Concepts

### 1. Vitess Components

```
Component Hierarchy:
─────────────────────────────────────────────────────────────────

Keyspace (≈ Database)
    │
    ├── Shard -80 (range: MIN to 0x80)
    │   ├── Primary VTTablet → MySQL
    │   ├── Replica VTTablet → MySQL
    │   └── Replica VTTablet → MySQL
    │
    ├── Shard 80-c0 (range: 0x80 to 0xC0)
    │   ├── Primary VTTablet → MySQL
    │   ├── Replica VTTablet → MySQL
    │   └── Replica VTTablet → MySQL
    │
    └── Shard c0- (range: 0xC0 to MAX)
        ├── Primary VTTablet → MySQL
        ├── Replica VTTablet → MySQL
        └── Replica VTTablet → MySQL

VTGate: Stateless query router (many instances)
VTTablet: MySQL manager per instance
VTCtld: Cluster orchestrator (few instances)
```

### 2. Sharding Model

```
Sharding in Vitess:
─────────────────────────────────────────────────────────────────

Sharding Key (vindexes):
    - Hash-based (default): Even distribution
    - Range-based: Sequential data (time-series)
    - Custom: Application-specific logic

Example:
    Table: orders
    Sharding key: customer_id

    customer_id=123 → hash(123) = 0x4F... → Shard -80
    customer_id=456 → hash(456) = 0xB2... → Shard 80-c0
    customer_id=789 → hash(789) = 0xE1... → Shard c0-

Vindexes (Virtual Indexes):
    - Primary: Determines shard location
    - Secondary: Cross-shard lookups
    - Functional: Computed values
```

### 3. Non-Blocking Schema Changes (Online DDL)

```
PlanetScale Schema Changes:
─────────────────────────────────────────────────────────────────

Traditional DDL:
    ALTER TABLE large_table ADD COLUMN new_col INT;
    → Locks table
    → Blocks writes
    → Minutes to hours of downtime

PlanetScale Online DDL (gh-ost/pt-online-schema-change):
    1. Create shadow table with new schema
    2. Copy data in background
    3. Stream binlog changes to shadow
    4. Atomic table swap when caught up
    → Zero downtime
    → No blocking
```

### 4. Database Branching

```
PlanetScale Branching (similar to Neon):
─────────────────────────────────────────────────────────────────

    main (production)
        │
        │──────────────────────────────────→ time
        │
        └── feature-branch
            │
            └── Deploy Request (PR for schema)

Branch Features:
    - Isolated copy of database schema
    - Test schema changes before production
    - Deploy Requests for review workflow
    - Safe rollback capability
```

## How HA Works

### Per-Shard Replication

```
Shard HA (MySQL Replication):
─────────────────────────────────────────────────────────────────

Each shard:
    Primary ──binlog──► Replica 1
         │
         └──binlog──► Replica 2

Replication: MySQL async or semi-sync
Failover: VTOrc (Vitess Orchestrator) or external

VTOrc:
    - Monitors MySQL replication topology
    - Detects primary failure
    - Promotes replica to primary
    - Reconfigures other replicas
    - Updates topology service
```

### Failover Process

```
Shard Primary Failure:
─────────────────────────────────────────────────────────────────

t=0s    Primary for Shard 1 crashes

t=1s    VTOrc detects failure
        - MySQL not responding
        - Replication lag increasing

t=5s    VTOrc initiates failover
        - Identifies best replica candidate
        - Lowest replication lag
        - Check errant GTIDs

t=10s   Replica promoted
        - SET GLOBAL read_only=OFF
        - New primary accepting writes

t=12s   Topology updated
        - Topology service updated
        - VTGate learns new primary

t=15s   Other replicas repointed
        - CHANGE MASTER TO new_primary

t=20s   Failover complete
        - VTGate routing to new primary
        - Service restored

Total time: ~15-20 seconds
```

### VTGate Health Routing

```
VTGate Query Routing:
─────────────────────────────────────────────────────────────────

Read Query:
    SELECT * FROM users WHERE id = 123;

    1. VTGate receives query
    2. Compute shard from id (hash vindex)
    3. Route to shard's replica (load balanced)
    4. Return results

Write Query:
    INSERT INTO users (id, name) VALUES (123, 'Alice');

    1. VTGate receives query
    2. Compute shard from id
    3. Route to shard's primary only
    4. Return result

Health-based Routing:
    - VTGate monitors tablet health
    - Unhealthy tablets removed from routing
    - Automatic failover handling
```

## Configuration

### PlanetScale CLI Setup

```bash
# Install CLI
brew install planetscale/tap/pscale

# Login
pscale auth login

# Create database
pscale database create my-app --region us-east

# Create branch
pscale branch create my-app feature-xyz

# Connect (opens local proxy)
pscale connect my-app main --port 3306

# Then use any MySQL client
mysql -h 127.0.0.1 -P 3306 -u root
```

### Schema Management

```sql
-- On a branch, make schema changes
ALTER TABLE users ADD COLUMN phone VARCHAR(20);

-- Create deploy request (via CLI or UI)
-- pscale deploy-request create my-app feature-xyz

-- After review and approval, deploy merges changes
-- Zero-downtime schema migration to production
```

### VSchema (Vitess Schema)

```json
{
  "sharded": true,
  "vindexes": {
    "hash": {
      "type": "hash"
    },
    "user_id_lookup": {
      "type": "lookup_hash",
      "params": {
        "table": "user_id_lookup",
        "from": "user_id",
        "to": "keyspace_id"
      }
    }
  },
  "tables": {
    "users": {
      "column_vindexes": [
        {
          "column": "id",
          "name": "hash"
        }
      ]
    },
    "orders": {
      "column_vindexes": [
        {
          "column": "user_id",
          "name": "hash"
        }
      ]
    }
  }
}
```

### Connection String

```python
# PlanetScale connection
import mysql.connector

conn = mysql.connector.connect(
    host="aws.connect.psdb.cloud",
    user="username",
    password="pscale_pw_xxx",
    database="my-app",
    ssl_ca="/etc/ssl/certs/ca-certificates.crt",
    ssl_verify_cert=True
)

# Or via pscale connect proxy
conn = mysql.connector.connect(
    host="127.0.0.1",
    port=3306,
    user="root",
    database="my-app"
)
```

## Happy Path Scenarios

### Scenario 1: Non-Blocking Schema Change

```
Timeline: Adding column to 100GB table
─────────────────────────────────────────────────────────────────

Traditional MySQL:
    t=0     ALTER TABLE orders ADD COLUMN status VARCHAR(20);
    t=0     Table locked, no writes
    t=30min Migration complete
    → 30 minutes downtime

PlanetScale:
    t=0     Create branch: pscale branch create my-app add-status
    t=1s    Branch ready

    t=10s   ALTER TABLE orders ADD COLUMN status VARCHAR(20);
            (On branch, instant - small test data)

    t=20s   Create deploy request
            pscale deploy-request create my-app add-status

    t=1min  Review and approve in UI

    t=1min  Deploy starts (Online DDL)
            - Shadow table created
            - Background copy begins
            - Binlog streaming
            - Application continues normally

    t=30min Background migration complete
            - Atomic table swap
            - Zero downtime
            - No locks during migration

Impact: Zero application downtime
```

### Scenario 2: Resharding (Adding Shards)

```
Timeline: Scaling from 2 to 4 shards
─────────────────────────────────────────────────────────────────

Initial: 2 shards (-80, 80-)

t=0     Initiate resharding
        - Plan: -80 → -40, 40-80
        - Plan: 80- → 80-c0, c0-

t=1min  Create destination shards
        - New MySQL instances provisioned
        - Empty databases created

t=5min  VReplication starts
        - Copy existing data to new shards
        - Stream binlog changes

t=1hour Data copy complete (depends on size)
        - New shards caught up
        - Verifying data integrity

t=1hour Traffic cutover
+1min   - VTGate routing updated
        - Reads go to new shards
        - Writes go to new shards

t=1hour Cleanup
+5min   - Old shards decommissioned
        - Resharding complete

Zero downtime throughout
Application unaware of resharding
```

### Scenario 3: Multi-Region Read Replicas

```
Timeline: Adding read replicas in EU
─────────────────────────────────────────────────────────────────

Primary: us-east
Read region: eu-west

t=0     Add read-only region
        - Configure in PlanetScale UI
        - New MySQL replicas in EU

t=5min  Replication established
        - Async replication us-east → eu-west
        - Lag: ~100ms (cross-Atlantic)

EU User Query:
    t=0ms   SELECT * FROM products WHERE id = 123;
    t=5ms   Routed to EU replica
    t=10ms  Response (local latency)

Without EU replica:
    t=0ms   Query
    t=80ms  Cross-Atlantic round trip
    t=90ms  Response

Benefit: 80ms → 10ms for EU reads
Note: Writes still go to primary region
```

## Unhappy Path Scenarios

### Scenario 1: Shard Primary Failure

```
Timeline: Primary MySQL crashes
─────────────────────────────────────────────────────────────────

t=0s    Shard 1 primary crashes

t=1s    VTOrc detects failure
        - Cannot connect to primary
        - Replicas report lag

t=5s    VTOrc evaluates candidates
        - Replica A: lag 0 bytes, GTID current
        - Replica B: lag 100 bytes

t=7s    Replica A promoted
        - read_only=OFF
        - Accepting writes

t=10s   Replica B reconfigured
        - Points to new primary (Replica A)

t=12s   Topology service updated
        - Shard 1 primary = Replica A

t=15s   VTGate routing updated
        - New connections to new primary

t=20s   Service fully restored

Impact: 15-20 second write unavailability for Shard 1
        Other shards unaffected
        Queries to other shards continue
```

### Scenario 2: VTGate Failure

```
Timeline: VTGate instance crashes
─────────────────────────────────────────────────────────────────

PlanetScale architecture: Multiple VTGate instances

t=0s    VTGate-1 crashes
        - Connections through VTGate-1 lost

t=0s    Load balancer detects
        - Health check fails

t=1s    Traffic rerouted
        - VTGate-2 and VTGate-3 handle load

t=2s    Clients reconnect
        - New connections to healthy VTGates
        - Queries continue

t=30s   VTGate-1 restarts (or replaced)
        - Rejoins pool
        - Load balanced again

Impact: Brief connection errors
        Automatic recovery
        VTGate is stateless
```

### Scenario 3: Cross-Shard Query Performance

```
Timeline: Query hitting all shards
─────────────────────────────────────────────────────────────────

Query: SELECT COUNT(*) FROM orders WHERE status = 'pending';

Problem: No shard key in WHERE clause
         Must scatter to all shards

t=0ms   VTGate receives query

t=1ms   Query scattered to all shards
        - Shard 1: SELECT COUNT(*) FROM orders WHERE status = 'pending'
        - Shard 2: SELECT COUNT(*) FROM orders WHERE status = 'pending'
        - Shard 3: SELECT COUNT(*) FROM orders WHERE status = 'pending'

t=50ms  All shards respond
        - Shard 1: 15000
        - Shard 2: 12000
        - Shard 3: 18000

t=51ms  VTGate aggregates
        - Total: 45000

t=52ms  Response to client

Impact: Query touches all shards
        Higher latency than single-shard
        More resource usage

Optimization:
        - Add secondary vindex on status
        - Or denormalize with shard key
```

### Scenario 4: Replication Lag During High Load

```
Timeline: Burst write load
─────────────────────────────────────────────────────────────────

t=0s    Write burst begins
        - 10,000 writes/second
        - Normal: 1,000 writes/second

t=10s   Replication lag increases
        - Primary processing writes
        - Replicas falling behind

t=30s   Lag: 30 seconds
        - Read queries may return stale data

t=30s   Read routing impact
        - VTGate can route reads to primary
        - Or serve stale data from replicas

Options:
        1. Route reads to primary (more load)
        2. Accept stale reads (application decision)
        3. Scale up replicas

t=60s   Write burst ends

t=90s   Replicas catch up
        - Lag returns to normal

Impact: Temporary stale reads possible
        Primary handles extra load
```

### Scenario 5: Failed Deploy Request

```
Timeline: Schema change causes issues
─────────────────────────────────────────────────────────────────

t=0     Deploy Request initiated
        - ADD COLUMN with DEFAULT value

t=5min  Online DDL in progress
        - Shadow table populating

t=10min Error detected
        - Disk space issue
        - Or incompatible change

t=10min Deploy Request fails
        - Rolled back automatically
        - Shadow table dropped
        - Original table unchanged

t=11min Production unaffected
        - No downtime occurred
        - Schema unchanged

Resolution:
        - Fix issue on branch
        - Create new Deploy Request
        - Retry deployment

Benefit: Safe schema changes
         Automatic rollback on failure
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Zero-Downtime DDL** | Schema changes without locks |
| **Database Branching** | Safe testing workflow |
| **Horizontal Scaling** | Vitess-powered sharding |
| **Serverless Option** | Scale to zero, pay per use |
| **Global Edge** | Connection pooling at edge |
| **Deploy Requests** | GitOps for schema changes |
| **Battle-Tested** | Vitess powers YouTube, Slack |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **MySQL Only** | Not PostgreSQL compatible |
| **No Foreign Keys** | Sharding limitation |
| **Cross-Shard Joins** | Expensive, limited |
| **Learning Curve** | Sharding concepts, vindexes |
| **Vendor Lock-in** | PlanetScale-specific features |
| **Limited Transactions** | Cross-shard transactions limited |
| **No Stored Procedures** | Not supported |

## Key Differences from PostgreSQL HA

| Aspect | PlanetScale/Vitess | PostgreSQL HA |
|--------|-------------------|---------------|
| Protocol | MySQL | PostgreSQL |
| Sharding | Built-in, automatic | Manual or Citus |
| Online DDL | Native, zero-downtime | pg_repack, limited |
| Branching | Built-in | Neon only |
| Foreign Keys | Not supported | Full support |
| Stored Procedures | Not supported | Full support |
| Extensions | MySQL UDFs only | Rich ecosystem |

## Comparison with PostgreSQL Solutions

| Feature | PlanetScale | Citus | CockroachDB | YugabyteDB |
|---------|-------------|-------|-------------|------------|
| Database | MySQL | PostgreSQL | CockroachDB | PostgreSQL-compat |
| Sharding | Vitess | Hash | Range | Hash/Range |
| Online DDL | Native | Limited | Yes | Limited |
| Branching | Yes | No | No | No |
| Foreign Keys | No | Yes | Yes | Yes |
| Serverless | Yes | No | Yes | No |

## Limitations

1. **Not PostgreSQL**: MySQL protocol only
2. **No Foreign Keys**: Fundamental Vitess limitation
3. **Cross-Shard Limitations**: Joins, transactions restricted
4. **No Stored Procedures**: MySQL stored procedures not supported
5. **Vindex Design**: Requires upfront sharding strategy
6. **Aggregations**: Cross-shard aggregations slower
7. **Sequences**: AUTO_INCREMENT challenges across shards
8. **Large Transactions**: Size limits for cross-shard

## When to Consider PlanetScale

### Good Fit
- MySQL applications needing horizontal scale
- Teams wanting zero-downtime schema changes
- Applications without foreign key requirements
- Workloads with clear sharding keys

### Not a Fit
- PostgreSQL applications (use Citus, YugabyteDB)
- Heavy use of foreign keys
- Complex stored procedures
- Need PostgreSQL extensions

## Conclusion

PlanetScale represents a mature approach to horizontally scalable MySQL with innovative developer experience features. Its zero-downtime schema changes and database branching are industry-leading. However, as a MySQL-based solution, it's not applicable for PostgreSQL workloads. For similar capabilities with PostgreSQL, consider:

- **Citus**: For horizontal scaling (sharding)
- **Neon**: For branching and serverless
- **YugabyteDB**: For distributed PostgreSQL

**Relevance to PostgreSQL**: Architecture patterns and features like branching and online DDL are influencing PostgreSQL ecosystem (e.g., Neon's branching).
