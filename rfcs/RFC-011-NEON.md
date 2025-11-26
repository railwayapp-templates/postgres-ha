# RFC-011: Neon - Serverless PostgreSQL with Branching

## Overview

Neon is a serverless PostgreSQL platform that separates storage and compute, enabling instant branching, autoscaling, and scale-to-zero capabilities. Built on a custom storage engine that extends PostgreSQL, Neon introduces innovative features like copy-on-write branching (similar to Git) and bottomless storage. It represents a new generation of cloud-native PostgreSQL.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ PostgreSQL Protocol
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                       Neon Proxy                                │
│        (Connection pooling, routing, authentication)            │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┴─────────────────────┐
        ▼                                           ▼
┌───────────────────┐                    ┌───────────────────┐
│  Compute (main)   │                    │ Compute (branch)  │
│  ┌─────────────┐  │                    │  ┌─────────────┐  │
│  │ PostgreSQL  │  │                    │  │ PostgreSQL  │  │
│  │  (modified) │  │                    │  │  (modified) │  │
│  │             │  │                    │  │             │  │
│  │ No local    │  │                    │  │ No local    │  │
│  │ storage!    │  │                    │  │ storage!    │  │
│  └─────────────┘  │                    │  └─────────────┘  │
└─────────┬─────────┘                    └─────────┬─────────┘
          │                                        │
          └─────────────────┬──────────────────────┘
                            │
                    Neon Storage API
                            │
┌─────────────────────────────────────────────────────────────────┐
│                       Pageserver                                 │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │              Page Cache + WAL Processing                    ││
│  │   - Serves pages to compute on demand                      ││
│  │   - Maintains page versions (MVCC for storage)             ││
│  │   - Copy-on-write for branches                             ││
│  └─────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘
                            │
┌─────────────────────────────────────────────────────────────────┐
│                       Safekeepers                                │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │
│  │ Safekeeper  │  │ Safekeeper  │  │ Safekeeper  │             │
│  │     1       │  │     2       │  │     3       │             │
│  │             │  │             │  │             │             │
│  │  WAL        │  │  WAL        │  │  WAL        │             │
│  │  Consensus  │  │  Consensus  │  │  Consensus  │             │
│  └─────────────┘  └─────────────┘  └─────────────┘             │
│           Paxos-based WAL durability (2/3 quorum)              │
└─────────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Object Storage (S3)                          │
│              Long-term storage, bottomless                      │
└─────────────────────────────────────────────────────────────────┘
```

## Core Concepts

### 1. Storage-Compute Separation

```
Traditional PostgreSQL:
─────────────────────────────────────────────────────────────────
    Compute ←→ Local Disk (coupled)
    - Data lives on local disk
    - Compute and storage scale together
    - Instance == data

Neon:
─────────────────────────────────────────────────────────────────
    Compute ←→ Network ←→ Pageserver ←→ S3
    - Compute is stateless (ephemeral)
    - Storage is independent service
    - Multiple computes can attach to same storage
    - Storage scales independently

Benefits:
    - Scale to zero (no compute cost when idle)
    - Instant branching (copy-on-write)
    - Bottomless storage (S3-backed)
    - Fast compute restart (no recovery)
```

### 2. Branching

```
Neon Branching:
─────────────────────────────────────────────────────────────────

                    main branch (production)
                          │
    ─────────────────────────────────────────────→ time
                          │
                          │ branch point (LSN)
                          │
                          └──── dev branch
                                    │
                                    └──── feature branch

Properties:
    - Instant creation (copy-on-write, no data copy)
    - Isolated computes (each branch has own compute)
    - Time travel (branch from any point in history)
    - Space efficient (only stores diff)
    - Read-write (branches can diverge)

Use Cases:
    - Development environments (clone production)
    - Testing (branch before test, delete after)
    - Data recovery (branch from before incident)
    - Feature development (isolated data)
```

### 3. Point-in-Time Recovery (PITR)

```
Neon PITR:
─────────────────────────────────────────────────────────────────

Timeline:
    ──[T1]──[T2]──[T3]──[T4]──[T5]──[T6]──[NOW]──→

Every point accessible:
    - Create branch from T3
    - Query data as of T4
    - Restore to T5

Storage retains:
    - All page versions
    - Complete WAL history
    - Up to retention limit (7 days free, configurable)

No separate backups needed for PITR range
```

## How Neon HA Works

### Write Path and Durability

```
Write Path:
─────────────────────────────────────────────────────────────────

1. Compute receives write
2. Compute generates WAL record
3. WAL sent to Safekeepers (consensus group)
4. Safekeepers replicate via Paxos
5. Once 2/3 Safekeepers acknowledge:
   - WAL is durable
   - Compute can acknowledge commit
6. Safekeepers stream WAL to Pageserver
7. Pageserver:
   - Applies WAL to pages
   - Stores in page cache
   - Eventually offloads to S3

Durability guarantees:
    - WAL durable after Safekeeper quorum
    - Data durable after S3 offload
    - Compute failure: no data loss (WAL safe)
    - Pageserver failure: rebuild from WAL + S3

Commit latency:
    - Must wait for 2/3 Safekeeper quorum (Paxos)
    - Similar to Aurora's approach (quorum ack from memory)
    - Typical: 3-10ms depending on network and load
    - RPO = 0 for all committed transactions
```

### Why Neon Achieves Zero Data Loss

Like Aurora, Neon uses **quorum-based durability** rather than waiting for individual node fsyncs:

```
Traditional Sync Replication:
    Write → Primary fsync → Replica fsync → Ack
    Latency: 5-15ms (two fsync operations in series)

Neon:
    Write → 2/3 Safekeepers ack (Paxos consensus) → Ack
    Latency: 3-10ms (parallel acks, memory-first)
```

The key insight: **replication count provides durability**, not individual disk fsyncs. If 2 of 3 Safekeepers have the WAL in memory, losing 1 node doesn't lose data. This allows acknowledging commits faster while maintaining RPO=0.

### Compute Failover

```
Compute Failure Scenario:
─────────────────────────────────────────────────────────────────

t=0s    Compute crashes
        - Process terminates
        - Connections lost

t=0s    WAL already safe on Safekeepers
        - No committed data at risk

t=1s    Neon control plane detects
        - Health check fails

t=2s    New compute started
        - Fresh PostgreSQL instance
        - Connects to Pageserver

t=3s    Compute attaches to storage
        - Receives latest LSN from Safekeepers
        - No traditional recovery (storage has pages)

t=5s    Compute ready
        - Accepting connections
        - All data available (from Pageserver)

Total failover: ~5-10 seconds
Data loss: Zero (WAL on Safekeepers)

Key insight: Compute is stateless
             No recovery replay needed
             Just reconnect to storage
```

### Pageserver Failure

```
Pageserver Failure:
─────────────────────────────────────────────────────────────────

t=0s    Pageserver crashes

t=1s    Compute requests page
        - Request fails

t=2s    New Pageserver started
        - Or traffic routed to replica

t=5s    Pageserver rebuilds state
        - Reads base images from S3
        - Applies recent WAL from Safekeepers

t=30s   Pageserver ready
        - Serving pages again

Impact: Compute requests stall during rebuild
        Eventual consistency maintained
```

## Configuration

### Creating a Neon Project

```bash
# Using Neon CLI
neon projects create --name my-project

# Output:
# Project created:
#   id: spring-flower-123456
#   region: aws-us-east-1
#   pg_version: 16

# Create main branch compute
neon branches create --name main --compute-size 0.25

# Connection string provided:
# postgresql://user:pass@ep-spring-flower-123456.us-east-1.aws.neon.tech/neondb
```

### Branching Operations

```bash
# Create branch from main
neon branches create \
    --name feature-xyz \
    --parent main

# Create branch from specific point in time
neon branches create \
    --name recovery-branch \
    --parent main \
    --timestamp '2024-01-15T10:30:00Z'

# Create branch from LSN
neon branches create \
    --name debug-branch \
    --parent main \
    --lsn 0/1234ABCD

# List branches
neon branches list

# Delete branch
neon branches delete feature-xyz
```

### Compute Configuration

```bash
# Scale compute
neon compute set main --compute-size 2

# Enable autoscaling
neon compute set main \
    --autoscaling-min 0.25 \
    --autoscaling-max 4

# Enable scale to zero
neon compute set main --suspend-timeout 300  # 5 minutes
```

### Connection Pooling

```python
# Neon provides built-in connection pooling
# Use pooled connection string:

# Direct (no pooling):
# postgresql://user:pass@ep-xxx.us-east-1.aws.neon.tech/neondb

# Pooled (PgBouncer):
# postgresql://user:pass@ep-xxx.us-east-1.aws.neon.tech/neondb?pgbouncer=true

# Transaction mode pooling (default):
import psycopg2
conn = psycopg2.connect(
    "postgresql://user:pass@ep-xxx.us-east-1.aws.neon.tech/neondb?pgbouncer=true"
)
```

## Happy Path Scenarios

### Scenario 1: Development Branch Workflow

```
Timeline: Feature development with data
─────────────────────────────────────────────────────────────────

t=0s    Developer needs production-like data
        $ neon branches create --name feature-login --parent main

t=1s    Branch created instantly
        - 500GB production database
        - No data copy (copy-on-write)
        - Separate compute started

t=10s   Developer connects
        - Full production schema
        - Full production data
        - Isolated environment

t=1h    Development complete
        - Made schema changes
        - Added test data
        - Only changed pages stored

t=1h    Clean up
        $ neon branches delete feature-login
        - Compute terminated
        - Storage delta released

Cost: Only compute time + delta storage
      Not full 500GB copy
```

### Scenario 2: Scale to Zero

```
Timeline: Low-traffic application
─────────────────────────────────────────────────────────────────

t=0s    Application handling requests
        - Compute running (0.25 CU)
        - Serving queries

t=5min  No activity
        - suspend-timeout reached (5 min default)

t=5min  Compute scales to zero
        - PostgreSQL process stopped
        - No compute charges
        - Data safe on storage

t=30min Request arrives
        - Connection to Neon endpoint

t=30min+1s  Cold start begins
        - Compute provisioning
        - Connect to storage

t=30min+3s  Compute ready
        - Query executes
        - Response returned

Cold start time: ~2-3 seconds
Cost during idle: Storage only
```

### Scenario 3: Point-in-Time Query

```
Timeline: Investigating past data
─────────────────────────────────────────────────────────────────

t=0s    Need to check data from yesterday

t=1s    Create branch at specific time
        $ neon branches create \
            --name audit \
            --parent main \
            --timestamp '2024-01-14T15:00:00Z'

t=3s    Branch ready
        - Compute started
        - Data as of Jan 14, 3 PM

t=5s    Query historical data
        SELECT * FROM transactions
        WHERE amount > 10000;

t=10s   Compare with current
        - Run same query on main
        - Analyze differences

t=30min Delete branch
        - $ neon branches delete audit
        - No long-term cost

No restore needed, instant access to historical data
```

## Unhappy Path Scenarios

### Scenario 1: Compute Failure

```
Timeline: Compute crashes unexpectedly
─────────────────────────────────────────────────────────────────

t=0s    Compute encounters OOM
        - PostgreSQL process killed

t=0s    WAL already durable
        - Safekeepers have all committed WAL
        - No data loss possible

t=1s    Neon detects compute down
        - Health check fails

t=2s    Automatic compute restart
        - New compute instance starting

t=3s    Compute connects to Pageserver
        - Receives current LSN
        - Buffer cache empty (cold)

t=5s    Compute accepting connections
        - Clients retry and connect

t=10s   Cache warming
        - First queries slower (cache miss)
        - Performance normalizes

Total outage: ~5-10 seconds
Data loss: Zero
Impact: Cold cache, brief reconnect
```

### Scenario 2: Safekeeper Failure

```
Timeline: One Safekeeper crashes
─────────────────────────────────────────────────────────────────

Configuration: 3 Safekeepers (quorum = 2)

t=0s    Safekeeper-1 crashes

t=0s    Commit continues
        - Safekeeper-2 and Safekeeper-3 available
        - Quorum (2/3) still achievable

t=0s    No impact on writes
        - Paxos continues with 2 nodes
        - Slightly reduced fault tolerance

t=10s   Neon control plane replaces Safekeeper-1
        - New node joins consensus
        - Catches up from peers

t=60s   Full redundancy restored
        - 3/3 Safekeepers healthy

Impact: Reduced redundancy temporarily
        No service interruption
```

### Scenario 3: Cold Start Under Load

```
Timeline: Scale-to-zero + traffic spike
─────────────────────────────────────────────────────────────────

t=0s    Compute suspended (scale to zero)

t=0s    Sudden traffic spike
        - 100 concurrent connection attempts

t=1s    Cold start initiated
        - First connection triggers wake-up

t=1s    Connections queued
        - Proxy holds connections
        - Clients waiting

t=3s    Compute starting
        - PostgreSQL initializing

t=5s    Compute ready
        - Queued connections forwarded

t=6s    All connections served
        - Some experienced 5s latency

t=10s   Autoscaling kicks in
        - Scales up for load
        - Subsequent requests fast

Impact: First requests delayed 3-5s
Mitigation: Configure minimum compute size
            Or disable scale-to-zero for production
```

### Scenario 4: Pageserver Overload

```
Timeline: High page request rate
─────────────────────────────────────────────────────────────────

t=0s    Large analytical query starts
        - Sequential scan of 100GB table
        - Many page requests to Pageserver

t=1s    Pageserver handles load
        - Page cache hit rate dropping
        - More S3 fetches

t=10s   Latency increases
        - Page requests taking longer
        - Query execution slows

t=30s   Query completes (slowly)
        - Higher than expected latency

Mitigations:
        - Larger compute (more local cache)
        - Query optimization
        - Appropriate indexes
        - Neon's smart caching improvements

Note: Neon optimized for OLTP
      Large analytics may hit storage limits
```

### Scenario 5: Branch Divergence Issues

```
Timeline: Schema drift between branches
─────────────────────────────────────────────────────────────────

t=0     Create feature branch from main
        - Same schema

t=1d    Feature branch: ALTER TABLE users ADD COLUMN phone;
t=1d    Main branch: ALTER TABLE users ADD COLUMN mobile;

t=2d    Feature merged to application
        - But database change not applied to main

t=2d    Mismatch!
        - App expects 'phone' column
        - Main has 'mobile' column

t=2d    Application errors

Resolution:
        - Branches are isolated (intentionally)
        - Schema migrations must be managed separately
        - Use migration tools (Flyway, Alembic)
        - Neon doesn't auto-merge branches

Best practice: Track schema changes in code
               Apply migrations to all branches
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Instant Branching** | Copy-on-write, no data copy |
| **Scale to Zero** | No cost when idle |
| **Autoscaling** | Automatic compute adjustment |
| **PITR Built-in** | Time travel without backups |
| **Fast Failover** | Stateless compute, quick restart |
| **Developer Experience** | Production clones in seconds |
| **Bottomless Storage** | S3-backed, virtually unlimited |
| **Modern Architecture** | Built for cloud from ground up |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Cold Start** | 2-5s latency when scaling from zero |
| **Network Latency** | Page fetches over network |
| **New Technology** | Less battle-tested than traditional |
| **Large Scans** | OLTP optimized, analytics may be slower |
| **Extension Support** | Not all extensions available |
| **Vendor Lock-in** | Neon-specific features |
| **Regional** | Limited regions currently |
| **Learning Curve** | New concepts (branching, etc.) |

## Comparison with Alternatives

| Feature | Neon | Aurora | RDS | Self-Managed |
|---------|------|--------|-----|--------------|
| Scale to Zero | Yes | Serverless v2 | No | No |
| Instant Branching | Yes | No | No | No |
| Storage-Compute Split | Yes | Yes | No | No |
| Cold Start | 2-5s | 30s+ | N/A | N/A |
| PITR | Built-in | Restore | Restore | Manual |
| Pricing Model | Usage-based | Instance | Instance | Infrastructure |
| Max Storage | Unlimited | 128TB | 64TB | Unlimited |

## Limitations

1. **Cold Start Latency**: Scale-to-zero has 2-5s wake-up time
2. **Network Dependency**: Every page fetch goes over network
3. **Extension Availability**: Some PostgreSQL extensions not supported
4. **Large Dataset Analytics**: May be slower than local storage
5. **Regional Availability**: Limited regions compared to major clouds
6. **Maturity**: Newer platform, less production track record
7. **Connection Limits**: Pooler limits vary by plan
8. **No Multi-Master**: Single writer per branch

## Best Practices

1. **Configure Autoscaling**: Set appropriate min/max for workload
2. **Use Connection Pooling**: Enable pgbouncer for connection efficiency
3. **Branch Strategically**: Use branches for dev/test, not production HA
4. **Monitor Cold Starts**: Alert if cold starts impact users
5. **Optimize Queries**: Reduce page fetches with proper indexing
6. **Set Retention**: Configure PITR retention based on needs
7. **Use Appropriate Plan**: Scale tier to workload
8. **Handle Reconnects**: Application must handle compute restarts

## Conclusion

Neon represents the next generation of PostgreSQL deployment with its serverless, branching-first approach. Its instant branching and scale-to-zero capabilities make it exceptional for development workflows and variable workloads. The main tradeoffs are cold start latency and the inherent network overhead of separated storage. Neon is ideal for modern applications that benefit from its innovative features.

**Recommended for:**
- Development and testing environments
- Applications with variable/unpredictable load
- Projects needing instant database clones
- Serverless application architectures
- Teams wanting modern developer experience

**Not recommended for:**
- Latency-critical applications (cold start)
- Heavy OLAP workloads
- Workloads requiring unsupported extensions
- Organizations requiring extensive production track record
