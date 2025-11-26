# RFC-015: Amazon RDS Multi-AZ for PostgreSQL

## Overview

Amazon RDS (Relational Database Service) Multi-AZ is AWS's managed high availability solution for PostgreSQL. It provides automatic failover to a synchronous standby replica in a different Availability Zone. Unlike Aurora (which has a custom storage layer), RDS Multi-AZ uses standard PostgreSQL streaming replication with managed infrastructure, offering a simpler upgrade path from self-managed PostgreSQL.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                      RDS DNS Endpoint                           │
│            mydb.xxxxx.us-east-1.rds.amazonaws.com              │
│            (Automatically points to current primary)            │
└─────────────────────────────────────────────────────────────────┘
                              │
        ┌─────────────────────┴─────────────────────┐
        │                                           │
        ▼                                           ▼
┌─────────────────────┐                ┌─────────────────────┐
│  Availability Zone A │                │  Availability Zone B │
│  ┌───────────────┐   │                │  ┌───────────────┐   │
│  │   Primary     │   │                │  │   Standby     │   │
│  │   Instance    │───┼── Synchronous ─┼──│   Instance    │   │
│  │               │   │   Replication  │  │   (Hidden)    │   │
│  │  PostgreSQL   │   │                │  │  PostgreSQL   │   │
│  └───────┬───────┘   │                │  └───────┬───────┘   │
│          │           │                │          │           │
│  ┌───────▼───────┐   │                │  ┌───────▼───────┐   │
│  │   EBS Volume  │   │                │  │   EBS Volume  │   │
│  │  (gp3/io1)    │   │                │  │  (gp3/io1)    │   │
│  │               │   │                │  │   (Replica)   │   │
│  └───────────────┘   │                │  └───────────────┘   │
└─────────────────────┘                └─────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                    AWS Infrastructure                           │
│  - Automated backups to S3                                      │
│  - Health monitoring                                            │
│  - Automatic failover orchestration                             │
│  - DNS endpoint management                                       │
└─────────────────────────────────────────────────────────────────┘
```

## Multi-AZ Deployment Options

### Option 1: Multi-AZ DB Instance (Classic)

```
Multi-AZ DB Instance:
─────────────────────────────────────────────────────────────────

Primary (AZ-A)        Standby (AZ-B)
     │                     │
     │   Synchronous       │
     │   Replication       │
     └─────────────────────┘

- Single standby in different AZ
- Synchronous replication
- Standby not readable
- Failover: 60-120 seconds
- DNS TTL: 5 seconds
```

### Option 2: Multi-AZ DB Cluster (New)

```
Multi-AZ DB Cluster (2 readable standbys):
─────────────────────────────────────────────────────────────────

        Writer Instance (AZ-A)
        [Local NVMe SSD]
              │
              │  Semi-Synchronous
              │  Replication (2-of-3 quorum)
        ┌─────┴─────┐
        │           │
        ▼           ▼
Reader Instance  Reader Instance
    (AZ-B)          (AZ-C)

- One writer + two readers
- All instances in different AZs
- Readers serve read traffic
- Failover: ~35 seconds
- Separate reader endpoint
- Uses local NVMe for faster commits
```

**Important**: "Semi-synchronous" means the writer waits for **at least 1 of 2 standbys** to acknowledge before committing. This is still **RPO = 0** (no data loss) because committed data exists on 2+ nodes before the client receives success.

## How It Works

### Synchronous Replication

```
Write Path (Multi-AZ Instance):
─────────────────────────────────────────────────────────────────

t=0ms   Client: INSERT INTO orders VALUES (...)

t=1ms   Primary receives write
        - Writes to WAL
        - Writes to EBS volume

t=5ms   WAL streamed to standby
        - Synchronous streaming replication
        - Same as PostgreSQL synchronous_commit

t=10ms  Standby acknowledges
        - WAL received and flushed
        - Written to standby EBS

t=11ms  Primary commits transaction
        - Confirms to client

t=12ms  Client receives success

Guarantee: Data written to 2 AZs before commit
           Zero data loss on failover (RPO = 0)
```

### Failover Process

```
Automatic Failover (Multi-AZ Instance):
─────────────────────────────────────────────────────────────────

t=0s    Primary instance failure detected
        - Health check fails
        - Or underlying hardware issue
        - Or AZ outage

t=5s    RDS initiates failover
        - Marks primary as failed

t=10s   Standby promotion begins
        - Standby comes out of recovery
        - Becomes new primary

t=30s   Instance ready
        - PostgreSQL accepting connections
        - EBS attached and active

t=60s   DNS propagation
        - Endpoint updated to new primary
        - TTL: 5 seconds

t=60-120s Failover complete
        - Clients reconnect via same endpoint
        - New standby provisioned (background)

Client Impact:
        - Connection errors during failover
        - Applications must retry
        - Same endpoint works after failover
```

### Failover (Multi-AZ DB Cluster)

```
Failover (Multi-AZ Cluster - Faster):
─────────────────────────────────────────────────────────────────

t=0s    Writer instance failure

t=1s    RDS detects failure
        - Faster detection than classic

t=5s    Reader promotion
        - Synchronous reader already has data
        - No recovery replay needed

t=15s   DNS update
        - Writer endpoint → new writer
        - Reader endpoint updated

t=35s   Failover complete
        - Much faster than classic Multi-AZ
        - Reader was already handling queries

Improvement: ~35s vs ~60-120s
```

## Actual Commit Latency (Benchmarks)

Real-world measurements show the latency cost of synchronous replication:

| Deployment | Min Latency | P99.9 Latency | Max Latency |
|------------|-------------|---------------|-------------|
| **Single-AZ** | ~0.5-1ms | 5-15ms | varies |
| **Multi-AZ Instance** | ~4ms | 50-100ms | ~922ms |
| **Multi-AZ DB Cluster** | ~2ms | 20-40ms | ~345ms |
| **Aurora** | ~2ms | 20-30ms | ~226ms |

**Source**: [HackMySQL benchmarks](https://hackmysql.com/commit-latency-aurora-vs-rds-mysql-8.0/)

### Why Multi-AZ Instance Is Slower

```
Multi-AZ Instance (Classic):
    Write → Local EBS → Send WAL over network → Standby EBS fsync → Ack

    The killer: Standby must fsync to EBS before acknowledging
    EBS fsync across AZ = 4-10ms minimum
```

### Why Multi-AZ DB Cluster Is Faster

```
Multi-AZ DB Cluster:
    Write → Local NVMe (fast!) → Send WAL → Standby acks from memory

    Key optimizations:
    - Local NVMe SSD instead of network EBS for primary
    - Semi-sync: only needs 1-of-2 standbys to ack
    - Standbys can ack before full fsync (like Aurora)
```

### The Latency vs Durability Tradeoff

| Option | Commit Latency | Data Loss (RPO) | Standby Readable |
|--------|----------------|-----------------|------------------|
| Single-AZ | Lowest (~1ms) | Up to last backup | N/A |
| Multi-AZ Instance | Higher (~4ms+) | Zero | No |
| Multi-AZ DB Cluster | Medium (~2ms) | Zero | Yes |

All Multi-AZ options guarantee **RPO = 0** for committed transactions. The "semi-synchronous" naming in DB Cluster is misleading - it still waits for acknowledgment before returning success to the client.

## Configuration

### Creating Multi-AZ Instance (Terraform)

```hcl
resource "aws_db_instance" "postgres" {
  identifier     = "mydb-production"
  engine         = "postgres"
  engine_version = "16.1"
  instance_class = "db.r6g.xlarge"

  # Storage
  allocated_storage     = 100
  max_allocated_storage = 1000
  storage_type          = "gp3"
  storage_encrypted     = true
  kms_key_id           = aws_kms_key.rds.arn

  # High Availability
  multi_az = true  # Enable Multi-AZ

  # Database
  db_name  = "myapp"
  username = "admin"
  password = var.db_password
  port     = 5432

  # Networking
  db_subnet_group_name   = aws_db_subnet_group.main.name
  vpc_security_group_ids = [aws_security_group.rds.id]
  publicly_accessible    = false

  # Backup
  backup_retention_period = 7
  backup_window          = "03:00-04:00"
  maintenance_window     = "Mon:04:00-Mon:05:00"

  # Performance
  performance_insights_enabled = true
  monitoring_interval         = 60
  enabled_cloudwatch_logs_exports = ["postgresql", "upgrade"]

  # Protection
  deletion_protection = true
  skip_final_snapshot = false

  tags = {
    Environment = "production"
  }
}
```

### Creating Multi-AZ DB Cluster (Terraform)

```hcl
resource "aws_rds_cluster" "postgres" {
  cluster_identifier = "mydb-cluster"
  engine            = "postgres"
  engine_version    = "16.1"
  database_name     = "myapp"
  master_username   = "admin"
  master_password   = var.db_password

  # Multi-AZ Cluster specific
  db_cluster_instance_class = "db.r6gd.xlarge"
  storage_type              = "io1"
  iops                      = 3000
  allocated_storage         = 100

  # Networking
  db_subnet_group_name   = aws_db_subnet_group.main.name
  vpc_security_group_ids = [aws_security_group.rds.id]

  # Backup
  backup_retention_period = 7

  # Encryption
  storage_encrypted = true
  kms_key_id       = aws_kms_key.rds.arn
}

# Writer instance (implicit in cluster)
# Reader instances (implicit in cluster)
```

### Connection String

```python
import psycopg2

# Single endpoint (handles failover automatically)
conn = psycopg2.connect(
    host="mydb-production.xxxxx.us-east-1.rds.amazonaws.com",
    port=5432,
    database="myapp",
    user="admin",
    password="password",
    sslmode="require",
    connect_timeout=5,
    # Important for failover handling
    target_session_attrs="read-write"
)

# Multi-AZ Cluster - Separate endpoints
writer_endpoint = "mydb-cluster.cluster-xxxxx.us-east-1.rds.amazonaws.com"
reader_endpoint = "mydb-cluster.cluster-ro-xxxxx.us-east-1.rds.amazonaws.com"
```

### Read Replicas (Separate from Multi-AZ)

```hcl
# Read replica (in addition to Multi-AZ)
resource "aws_db_instance" "replica" {
  identifier          = "mydb-replica-1"
  replicate_source_db = aws_db_instance.postgres.identifier
  instance_class      = "db.r6g.large"

  # Replica can be in same or different region
  availability_zone = "us-east-1c"

  # No Multi-AZ for replica (optional)
  multi_az = false

  # Read replica specific
  backup_retention_period = 0  # Backups on source only
}
```

## Happy Path Scenarios

### Scenario 1: Normal Operations

```
Timeline: Steady-state operation
─────────────────────────────────────────────────────────────────

Application
     │
     │ mydb.xxxxx.rds.amazonaws.com
     ▼
┌─────────────┐
│   Primary   │ ←─ All reads and writes
│   (AZ-A)    │
└──────┬──────┘
       │
       │ Synchronous Streaming Replication
       │
       ▼
┌─────────────┐
│   Standby   │ ←─ Not accessible (hidden)
│   (AZ-B)    │    Receives WAL continuously
└─────────────┘

Every write:
1. Written to primary WAL
2. Replicated to standby
3. Standby acknowledges
4. Primary commits
5. Client receives confirmation

Data always in 2 AZs before commit confirmed
```

### Scenario 2: Planned Maintenance

```
Timeline: AWS maintenance window
─────────────────────────────────────────────────────────────────

t=0s    Maintenance window starts
        - AWS needs to patch primary

t=0s    Automatic failover initiated
        - Same as unplanned failover
        - Standby promoted

t=60s   Primary now standby
        - Old primary being maintained
        - Service continues on new primary

t=30min Maintenance complete
        - Old primary rejoins as standby
        - No second failover (unless requested)

Impact: ~60-120 seconds during failover
        Can be avoided with blue/green deployments
```

### Scenario 3: Adding Read Replicas

```
Timeline: Scaling reads
─────────────────────────────────────────────────────────────────

t=0     Create read replica
        aws rds create-db-instance-read-replica \
            --db-instance-identifier mydb-replica \
            --source-db-instance-identifier mydb-production

t=1min  Snapshot created
        - From current primary
        - Automatic, no downtime

t=5min  Replica restoring
        - From snapshot

t=30min Replica available
        - Streaming replication active
        - Own endpoint for reads

Application change:
        - Writes → mydb-production.xxxxx.rds.amazonaws.com
        - Reads  → mydb-replica.xxxxx.rds.amazonaws.com

Note: Read replicas are separate from Multi-AZ standby
      Standby: HA, not readable
      Replica: Read scaling, async replication
```

## Unhappy Path Scenarios

### Scenario 1: Primary Instance Failure

```
Timeline: Primary crashes unexpectedly
─────────────────────────────────────────────────────────────────

t=0s    Primary instance fails
        - Hardware issue
        - Or process crash

t=5s    RDS health check fails
        - Cannot connect to primary
        - Standby still healthy

t=10s   Failover decision made
        - RDS initiates automatic failover

t=15s   Standby promotion starts
        - PostgreSQL recovery completes
        - Instance becomes read-write

t=30s   DNS update initiated
        - Endpoint points to new primary

t=60s   DNS propagated
        - TTL = 5 seconds
        - Clients can resolve new IP

t=90s   Applications reconnecting
        - Connection poolers may cache old IP
        - Retry logic kicks in

t=120s  Full service restoration
        - All clients connected to new primary

Data loss: Zero (synchronous replication)
Downtime: 60-120 seconds
Client impact: Must handle connection errors
```

### Scenario 2: Availability Zone Outage

```
Timeline: Entire AZ becomes unavailable
─────────────────────────────────────────────────────────────────

t=0s    AZ-A experiences outage
        - Primary in AZ-A
        - Network, power, or other issue

t=0s    Primary unreachable
        - Standby in AZ-B unaffected

t=5s    RDS detects AZ issue
        - Health checks fail

t=10s   Failover to AZ-B standby
        - Same process as instance failure

t=60-120s Service restored
        - Now running in AZ-B only
        - New standby created in AZ-C (or AZ-A when recovered)

Impact: Same as instance failure
        Multi-AZ design handles this automatically
```

### Scenario 3: Storage (EBS) Failure

```
Timeline: EBS volume issue
─────────────────────────────────────────────────────────────────

t=0s    Primary EBS volume degrades
        - AWS detects storage issue

t=0s    Options:
        A) AWS repairs volume in place
        B) Failover to standby (if severe)

Option A - Volume repair:
        - AWS handles transparently
        - No failover needed
        - Brief I/O pause possible

Option B - Failover:
        - Standard failover process
        - 60-120 seconds
        - New storage attached to former standby

EBS has built-in redundancy within AZ
Multi-AZ adds cross-AZ redundancy
```

### Scenario 4: Replication Lag Impact

```
Timeline: High write load
─────────────────────────────────────────────────────────────────

Synchronous replication impact:

t=0ms   Write issued to primary
t=5ms   Write to local WAL + EBS
t=15ms  Wait for standby ACK
t=25ms  Standby in different AZ, network latency
t=30ms  Standby acknowledges
t=31ms  Commit confirmed

Impact: Every write pays ~10-20ms extra latency
        (Cross-AZ network round trip)

Mitigation:
        - Batch writes where possible
        - Use async replication for read replicas
        - Accept latency for durability guarantee

Note: This is not "lag" - synchronous means no lag
      But it adds latency to every write
```

### Scenario 5: DNS Caching Issues

```
Timeline: Failover with DNS caching
─────────────────────────────────────────────────────────────────

t=0s    Failover completes
        - New primary in AZ-B
        - DNS updated

t=0s    Application's DNS cache
        - Still points to old IP
        - TTL not expired

t=10s   Connection attempts fail
        - Old IP doesn't respond
        - Timeouts

t=30s   Some clients work
        - Different DNS cache states
        - Inconsistent behavior

t=5min  Most clients recovered
        - DNS caches refreshed

Mitigation:
        - Use short DNS TTL in client resolver
        - Implement connection retry logic
        - Use RDS Proxy for connection pooling
        - Close idle connections periodically
```

### Scenario 6: Failover During Transaction

```
Timeline: Active transaction during failover
─────────────────────────────────────────────────────────────────

t=0ms   Transaction in progress
        BEGIN;
        UPDATE accounts SET balance = balance - 100 WHERE id = 1;

t=10ms  Primary fails (before COMMIT)

t=5s    Connection lost
        - Transaction uncommitted
        - Locks released

t=60s   Failover complete
        - Client reconnects
        - Transaction was rolled back

t=61s   Application retries
        - Starts new transaction
        - Same UPDATE succeeds

Impact: Uncommitted transactions lost
        Need application-level retry logic
        Idempotency important for retries

Committed transactions: Safe (synchronous replication)
Uncommitted transactions: Lost
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Fully Managed** | AWS handles patching, backups, failover |
| **Standard PostgreSQL** | Same as self-managed, easy migration |
| **Synchronous Replication** | Zero data loss guarantee |
| **Automatic Failover** | No manual intervention needed |
| **Cross-AZ Durability** | Survives AZ failure |
| **Same Endpoint** | No application changes for failover |
| **Read Replicas** | Additional read scaling option |
| **Backup/PITR** | Automated backups, point-in-time recovery |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Failover Time** | 60-120s (longer than Aurora) |
| **Standby Not Readable** | Standby cannot serve reads |
| **Write Latency** | Cross-AZ sync replication adds latency |
| **Single Region** | Multi-AZ is within region only |
| **Cost** | Pay for standby that doesn't serve traffic |
| **Limited Control** | No access to underlying OS |
| **Extension Limits** | Not all extensions available |
| **Size Limits** | Max 64TB storage |

## Multi-AZ vs Aurora Comparison

| Feature | RDS Multi-AZ | Aurora |
|---------|--------------|--------|
| Failover Time | 60-120s | 15-30s |
| Read from Standby | No | Yes (replicas) |
| Max Replicas | Separate (5) | 15 |
| Storage | EBS per instance | Shared storage |
| Replication | Streaming | Storage-level |
| Write Latency | Cross-AZ sync | Local + storage |
| Cost | Lower | Higher |
| PostgreSQL Compat | Higher | Slightly less |
| Max Storage | 64TB | 128TB |

## RDS Proxy (Recommended)

```
RDS Proxy Benefits:
─────────────────────────────────────────────────────────────────

              ┌─────────────────┐
              │    RDS Proxy    │
              │  (Connection    │
              │   Pooling)      │
              └────────┬────────┘
                       │
        ┌──────────────┴──────────────┐
        ▼                             ▼
┌───────────────┐           ┌───────────────┐
│    Primary    │           │    Standby    │
│    (AZ-A)     │           │    (AZ-B)     │
└───────────────┘           └───────────────┘

Benefits:
    - Handles failover automatically
    - Maintains connection pool
    - Multiplexes connections
    - Reduces failover impact
    - IAM authentication

Failover with Proxy:
    - Proxy handles reconnection
    - Client sees brief pause
    - No DNS caching issues
    - ~10 second failover impact
```

## Limitations

1. **Failover Duration**: 60-120 seconds is longer than Aurora
2. **Standby Not Readable**: Cannot serve read traffic
3. **Write Latency**: Synchronous cross-AZ replication
4. **Single Region**: Multi-AZ doesn't span regions
5. **Storage per Instance**: Not shared like Aurora
6. **Replica Promotion**: Read replicas are separate from Multi-AZ
7. **Maintenance Windows**: May require failovers
8. **Limited Instance Sizes**: Bound by EC2 instance types

## Best Practices

1. **Enable Multi-AZ**: Always for production
2. **Use RDS Proxy**: Reduces failover impact
3. **Implement Retry Logic**: Applications must handle disconnects
4. **Monitor Replication**: Watch for unusual latency
5. **Test Failover**: Regular failover drills
6. **Use Connection Timeouts**: Don't wait forever
7. **Consider Aurora**: If faster failover needed
8. **Add Read Replicas**: For read scaling (separate from HA)

## Conclusion

Amazon RDS Multi-AZ provides solid, managed high availability for PostgreSQL using standard streaming replication. It's simpler than Aurora and maintains higher PostgreSQL compatibility. The main tradeoffs are longer failover time (60-120s vs Aurora's 15-30s) and the standby being unable to serve reads. For most workloads, RDS Multi-AZ provides adequate HA with minimal operational overhead.

**Recommended for:**
- Production PostgreSQL workloads on AWS
- Teams wanting fully managed HA
- Applications tolerant of ~2 minute failover
- Workloads not requiring read scaling from HA standby
- Migration from self-managed PostgreSQL

**Not recommended for:**
- Sub-minute failover requirements (use Aurora)
- Need to read from HA standby (use Aurora)
- Multi-region HA (use Aurora Global Database)
- Very high transaction volumes (Aurora more efficient)
