# RFC-010: Amazon Aurora PostgreSQL

## Overview

Amazon Aurora PostgreSQL is a fully managed, cloud-native database compatible with PostgreSQL. Aurora separates compute from storage, using a distributed, fault-tolerant storage layer that automatically replicates data 6 ways across 3 Availability Zones. This architecture enables sub-minute failover, up to 15 read replicas, and automatic scaling.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┴─────────────────────┐
        │                                           │
        ▼                                           ▼
┌───────────────────┐                    ┌───────────────────┐
│   Writer Endpoint │                    │  Reader Endpoint  │
│  (single writer)  │                    │ (load balanced)   │
└─────────┬─────────┘                    └─────────┬─────────┘
          │                                        │
          ▼                                        ▼
┌─────────────────────────────────────────────────────────────────┐
│                       Compute Layer                              │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │
│  │   Primary    │  │   Replica    │  │   Replica    │  ...     │
│  │   Instance   │  │   Instance   │  │   Instance   │  (up to  │
│  │              │  │              │  │              │   15)    │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘          │
└─────────┼─────────────────┼─────────────────┼───────────────────┘
          │                 │                 │
          └─────────────────┴─────────────────┘
                            │
                    Shared Storage API
                            │
┌─────────────────────────────────────────────────────────────────┐
│              Aurora Distributed Storage Layer                    │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │                  Storage Volume                             ││
│  │      (10GB - 128TB, auto-scaling, encrypted)               ││
│  └─────────────────────────────────────────────────────────────┘│
│                            │                                    │
│    ┌───────────────────────┼───────────────────────┐           │
│    ▼                       ▼                       ▼           │
│  ┌─────┐  ┌─────┐  ┌─────┐  ┌─────┐  ┌─────┐  ┌─────┐        │
│  │ AZ1 │  │ AZ1 │  │ AZ2 │  │ AZ2 │  │ AZ3 │  │ AZ3 │        │
│  │Copy1│  │Copy2│  │Copy1│  │Copy2│  │Copy1│  │Copy2│        │
│  └─────┘  └─────┘  └─────┘  └─────┘  └─────┘  └─────┘        │
│         6 copies across 3 Availability Zones                   │
└─────────────────────────────────────────────────────────────────┘
```

## Core Concepts

### 1. Storage Architecture

```
Aurora Storage Model:
─────────────────────────────────────────────────────────────────

Traditional PostgreSQL:
    Compute → writes pages → Local Storage
    Replication: Send WAL → Apply WAL → Write pages

Aurora:
    Compute → writes redo log → Distributed Storage
    Storage: Applies redo, creates pages
    Replication: Storage layer handles it

Key Difference:
    - Only redo log crosses network (not full pages)
    - 6x replication at storage layer
    - Compute is stateless (cache only)

Write Path:
    1. Primary generates redo log
    2. Redo sent to storage nodes
    3. Storage writes to 4/6 copies (quorum)
    4. Acknowledge to Primary
    5. Storage asynchronously materializes pages

Why Aurora Achieves Low Latency with Synchronous Writes:
    - Only sends log records (~100 bytes), not full pages (8KB)
    - 4-of-6 quorum takes fastest 4 responses, ignores slow 2
    - Storage nodes acknowledge from memory, persist in background
    - SSD write latency ~1ms per storage node

Read Replica Path:
    1. Primary sends redo to replicas (async)
    2. Replicas apply redo to cache
    3. Replicas read from shared storage
    4. Typical lag: 10-20ms
```

### 2. Endpoints

```
Aurora Endpoints:
─────────────────────────────────────────────────────────────────

Writer Endpoint:
    - Points to current primary
    - Automatically updates on failover
    - Use for writes and read-after-write

Reader Endpoint:
    - Load balances across all replicas
    - Round-robin distribution
    - Use for read scaling

Instance Endpoints:
    - Direct connection to specific instance
    - Use for debugging or specific routing

Custom Endpoints:
    - Define groups of instances
    - Custom load balancing rules
```

## How Failover Works

### Automatic Failover

```
Aurora Failover Process:
─────────────────────────────────────────────────────────────────

t=0s    Primary instance failure detected
        - Heartbeat lost
        - OR manual failover triggered

t=0s    Aurora evaluates replicas
        - Priority tiers considered
        - Replica lag evaluated

t=1s    Best replica selected
        - Lowest tier wins (0-15)
        - Within tier, smallest lag wins

t=5s    Storage pointer updated
        - New primary claims write role
        - No data movement needed!
        - Storage already has all committed data

t=10s   DNS updated
        - Writer endpoint → new primary
        - TTL: 5 seconds

t=15s   Replica caches invalidated
        - Other replicas update cache
        - Point to new primary for writes

t=30s   Failover complete
        - New primary accepting writes
        - Client retry succeeds

Total time: 15-30 seconds typical
            Sub-minute SLA

Key: No data replication needed during failover
     Storage layer already has all data
```

### Failover Priority

```sql
-- Set failover priority (0 = highest, 15 = lowest)
-- AWS CLI:
aws rds modify-db-instance \
    --db-instance-identifier my-replica-1 \
    --promotion-tier 0

-- Replica with tier 0 will be promoted first
-- If tie, Aurora picks replica with least lag
```

## Configuration

### Creating Aurora Cluster (Terraform)

```hcl
resource "aws_rds_cluster" "aurora_pg" {
  cluster_identifier      = "aurora-postgres-cluster"
  engine                  = "aurora-postgresql"
  engine_version          = "15.4"
  database_name           = "mydb"
  master_username         = "admin"
  master_password         = var.db_password

  # High Availability
  availability_zones      = ["us-east-1a", "us-east-1b", "us-east-1c"]

  # Storage
  storage_encrypted       = true
  kms_key_id             = aws_kms_key.rds.arn

  # Backup
  backup_retention_period = 7
  preferred_backup_window = "02:00-03:00"

  # Networking
  db_subnet_group_name    = aws_db_subnet_group.aurora.name
  vpc_security_group_ids  = [aws_security_group.aurora.id]

  # Performance Insights
  performance_insights_enabled = true

  # Deletion protection
  deletion_protection     = true
  skip_final_snapshot     = false
}

# Primary instance
resource "aws_rds_cluster_instance" "primary" {
  identifier           = "aurora-instance-1"
  cluster_identifier   = aws_rds_cluster.aurora_pg.id
  instance_class       = "db.r6g.2xlarge"
  engine               = "aurora-postgresql"
  promotion_tier       = 0
}

# Read replicas
resource "aws_rds_cluster_instance" "replicas" {
  count                = 2
  identifier           = "aurora-instance-${count.index + 2}"
  cluster_identifier   = aws_rds_cluster.aurora_pg.id
  instance_class       = "db.r6g.xlarge"
  engine               = "aurora-postgresql"
  promotion_tier       = 1
}
```

### Connection Strings

```python
# Writer endpoint (for writes and read-after-write)
writer_dsn = "postgresql://admin:password@aurora-cluster.cluster-xxxx.us-east-1.rds.amazonaws.com:5432/mydb"

# Reader endpoint (for read scaling)
reader_dsn = "postgresql://admin:password@aurora-cluster.cluster-ro-xxxx.us-east-1.rds.amazonaws.com:5432/mydb"

# Application pattern
def get_connection(for_write=False):
    if for_write:
        return psycopg2.connect(writer_dsn)
    else:
        return psycopg2.connect(reader_dsn)
```

## Happy Path Scenarios

### Scenario 1: Normal Operations with Read Scaling

```
Timeline: High-traffic application
─────────────────────────────────────────────────────────────────

t=0s    Application handling 10,000 req/s
        - 80% reads, 20% writes

t=0s    Writes → Writer Endpoint → Primary
        - Primary handles 2,000 writes/s

t=0s    Reads → Reader Endpoint → Load balanced
        - 3 replicas each handle ~2,667 reads/s

t=0s    Storage handles all I/O
        - Single storage volume
        - 6x replicated automatically

Result: Linear read scaling by adding replicas
        Write scaling limited to single primary
```

### Scenario 2: Adding Read Replica

```
Timeline: Scaling out reads
─────────────────────────────────────────────────────────────────

$ aws rds create-db-instance \
    --db-instance-identifier aurora-instance-4 \
    --db-cluster-identifier aurora-cluster \
    --db-instance-class db.r6g.xlarge \
    --engine aurora-postgresql

t=0s    Create replica request

t=5s    Instance provisioning
        - EC2 instance launched
        - Aurora agent started

t=60s   Instance connects to storage
        - NO data copy needed!
        - Reads from shared storage immediately

t=90s   Cache warming (optional)
        - Hot data pulled into buffer cache

t=120s  Replica available
        - Added to reader endpoint
        - Accepting connections

Time to add replica: ~2 minutes
        (vs hours for traditional replication)
```

### Scenario 3: Planned Failover

```
Timeline: Maintenance failover
─────────────────────────────────────────────────────────────────

$ aws rds failover-db-cluster \
    --db-cluster-identifier aurora-cluster \
    --target-db-instance-identifier aurora-instance-2

t=0s    Failover initiated

t=1s    Primary completes in-flight transactions

t=2s    Primary flushes redo to storage

t=3s    Storage confirms durability

t=5s    aurora-instance-2 becomes primary
        - Acquires write capability
        - No data movement

t=10s   DNS updates
        - Writer endpoint → instance-2

t=15s   Old primary → replica
        - Continues as read replica

Total time: ~15 seconds
Zero data loss
```

## Unhappy Path Scenarios

### Scenario 1: Primary Instance Failure

```
Timeline: Unexpected primary crash
─────────────────────────────────────────────────────────────────

t=0s    Primary instance fails
        - Hardware failure
        - Process crash

t=1s    Aurora detects failure
        - Heartbeat missing
        - Health checks fail

t=2s    Committed data safe
        - Already written to storage (4/6 quorum)
        - In-flight transactions lost

t=5s    Best replica selected
        - Tier 0 replica: aurora-instance-2
        - Lag: 15ms

t=10s   aurora-instance-2 promoted
        - Acquires write lock
        - Applies any missing redo from storage

t=15s   DNS updated
        - Writer endpoint resolves to instance-2

t=20s   Other replicas updated
        - Recognize new primary

t=30s   Service restored
        - Applications retry and succeed

Total failover: ~30 seconds
Data loss: Only uncommitted transactions
```

### Scenario 2: Availability Zone Failure

```
Timeline: Entire AZ becomes unavailable
─────────────────────────────────────────────────────────────────

Cluster: Primary (AZ1), Replica-2 (AZ2), Replica-3 (AZ3)

t=0s    AZ1 fails completely
        - Network partition
        - Primary unreachable

t=5s    Storage continues operating
        - 4/6 copies available (AZ2 + AZ3)
        - Quorum maintained
        - No data at risk

t=10s   Replica in AZ2 promoted
        - Becomes new primary
        - Writes continue

t=30s   Full service restored
        - Reader endpoint excludes AZ1 replica
        - Writer in AZ2

When AZ1 recovers:
        - Instance rejoins as replica
        - Catches up from storage
        - No rebuild needed

Impact: Brief failover, no data loss
```

### Scenario 3: Storage Node Failure

```
Timeline: Storage subsystem issue
─────────────────────────────────────────────────────────────────

t=0s    One storage node fails
        - 1/6 copies unavailable

t=0s    Aurora continues normally
        - Write quorum: 4/6 (still met)
        - Read quorum: 3/6 (still met)

t=1s    Storage auto-repair begins
        - AWS infrastructure replaces node
        - Data re-replicated automatically

t=???   Repair completes
        - 6/6 copies restored
        - Fully redundant again

Impact: Zero - transparent to application
        Aurora handles storage HA automatically
```

### Scenario 4: Network Partition

```
Timeline: Compute-Storage network issue
─────────────────────────────────────────────────────────────────

t=0s    Primary loses storage connectivity
        - Cannot write redo
        - Cannot confirm commits

t=1s    Transactions start timing out
        - Writes fail
        - Reads may work (cached data)

t=5s    Aurora detects issue
        - Primary unhealthy

t=10s   Failover to replica in different AZ
        - Different network path
        - Can reach storage

t=30s   Service restored
        - New primary operational

Impact: Brief outage, failover resolves
        Assumes partition doesn't affect all AZs
```

### Scenario 5: All Replicas Unavailable

```
Timeline: Only primary remains
─────────────────────────────────────────────────────────────────

t=0s    Cluster: Primary only (replicas terminated)

t=5s    Primary fails

t=10s   No failover target!
        - No replicas to promote

t=15s   Aurora recovery options:
        - Wait for primary to recover
        - Create new instance from storage
        - Restore from snapshot

t=2min  New instance created
        - Reads from existing storage
        - No data loss (storage intact)

t=3min  Service restored
        - New primary operational

Impact: Extended outage (~3 minutes)
        No data loss (storage durable)

Lesson: Always maintain at least 1 replica
```

## Actual Commit Latency (Benchmarks)

Real-world measurements show Aurora achieves remarkably low commit latency despite synchronous 4-of-6 quorum writes:

| Metric | Aurora v3 | RDS Multi-AZ (2-AZ) | Self-Managed Async |
|--------|-----------|---------------------|-------------------|
| **Minimum** | ~2ms | ~4ms | ~0.5-1ms |
| **P99.9** | 20-30ms | 50-100ms | 5-15ms |
| **Maximum** | ~226ms | ~922ms | varies |

**Source**: [HackMySQL benchmarks](https://hackmysql.com/commit-latency-aurora-vs-rds-mysql-8.0/)

### Why Aurora Is Faster Than Traditional Sync Replication

```
Traditional Synchronous Replication:
    Write → Local WAL → Send full WAL to standby → Standby fsyncs → Ack
    Latency: 4-15ms (network + remote fsync is the killer)

Aurora:
    Write → Send small redo log → 4-of-6 storage nodes ack from memory
    Latency: ~2ms (takes fastest 4, storage persists in background)
```

The key insight: Aurora's storage nodes **acknowledge from memory** before full persistence, relying on the 6-way replication for durability rather than individual node fsyncs. This is safe because losing 2 nodes still leaves 4 copies.

### Data Loss Characteristics

| Scenario | Data Loss |
|----------|-----------|
| Primary instance crash | Zero - storage has all committed data |
| Single AZ failure | Zero - 4/6 copies in other AZs |
| Storage node failure | Zero - quorum maintains durability |
| In-flight transactions | Lost (not yet committed) |

**RPO = 0** for all committed transactions. Aurora achieves this without the latency penalty of traditional synchronous replication.

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Managed Service** | No infrastructure management |
| **Fast Failover** | Sub-minute (typically 15-30s) |
| **Storage Scaling** | Auto-scales to 128TB |
| **Read Replicas** | Up to 15, share storage |
| **Durability** | 6-way replication across 3 AZs |
| **Add Replicas Fast** | Minutes, not hours |
| **Backtrack** | Point-in-time rollback (no restore) |
| **Global Database** | Cross-region replication |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Cost** | Higher than RDS PostgreSQL |
| **Vendor Lock-in** | AWS-specific |
| **Single Writer** | No multi-master |
| **Instance Limits** | Largest: db.r6g.16xlarge |
| **Some Extensions** | Not all PostgreSQL extensions |
| **Replica Lag** | 10-20ms typical |
| **Cross-Region Lag** | Higher for global database |
| **Egress Costs** | Data transfer charges |

## Key Features

### Aurora Serverless v2

```
Serverless v2:
─────────────────────────────────────────────────────────────────

- Auto-scales compute (0.5 to 128 ACUs)
- Sub-second scaling
- Pay per ACU-second
- Combines with provisioned instances

Use case: Variable workloads, dev/test, cost optimization
```

### Backtrack (Time Travel)

```sql
-- Rewind database to point in time (no restore!)
-- Must be enabled at cluster creation

CALL mysql.rds_backtrack_db(
    'aurora-cluster',
    'backtrack-to',
    '2024-01-15 10:30:00'
);

-- Returns to that moment in seconds
-- Good for: Accidental deletes, testing
-- Limit: 72 hours max
```

### Global Database

```
Aurora Global Database:
─────────────────────────────────────────────────────────────────

Primary Region (us-east-1)
    │
    │ Storage-based replication
    │ Lag: ~1 second
    │
    ▼
Secondary Region (eu-west-1)
    - Read-only replicas
    - Can be promoted to primary
    - RPO: ~1 second
    - RTO: ~1 minute
```

## Comparison with Alternatives

| Feature | Aurora | RDS PostgreSQL | Self-Managed |
|---------|--------|----------------|--------------|
| Failover Time | 15-30s | 60-120s | 30-60s |
| Storage Replication | Automatic 6x | EBS (3x) | Manual |
| Max Replicas | 15 | 5 | Unlimited |
| Add Replica Time | Minutes | Hours | Hours |
| Storage Limit | 128TB | 64TB | Unlimited |
| Managed | Full | Full | None |
| Cost | $$$ | $$ | $ + ops |

## Limitations

1. **Single Writer**: Cannot scale writes horizontally
2. **Instance Size Ceiling**: Limited by largest instance type
3. **Some Extensions Missing**: Not all PostgreSQL extensions available
4. **Vendor Lock-in**: Aurora-specific features don't translate
5. **Cross-Region Cost**: Global database incurs transfer costs
6. **Serverless Cold Start**: Paused instances take time to resume
7. **Limited Control**: Cannot tune storage layer
8. **Replica Lag**: Not zero, affects read-your-writes

## Best Practices

1. **Use Multi-AZ**: Deploy replicas in different AZs
2. **Set Failover Priority**: Configure promotion tiers
3. **Use Appropriate Endpoints**: Writer for writes, reader for reads
4. **Enable Backtrack**: For accidental delete protection
5. **Monitor Replica Lag**: Alert before it grows
6. **Use IAM Authentication**: More secure than passwords
7. **Enable Performance Insights**: For query optimization
8. **Plan for Global**: Use Global Database for DR

## Conclusion

Amazon Aurora PostgreSQL offers enterprise-grade HA with minimal operational overhead. Its storage-compute separation enables fast failover and easy read scaling. The main limitations are cost and single-writer architecture. Aurora is ideal for organizations wanting high availability without managing infrastructure.

**Recommended for:**
- AWS-centric organizations
- Applications needing high availability with minimal ops
- Read-heavy workloads requiring scaling
- Teams without dedicated DBA resources

**Not recommended for:**
- Multi-cloud or cloud-agnostic requirements
- Write-heavy workloads needing horizontal scale
- Budget-constrained projects
- Workloads requiring specific PostgreSQL extensions
