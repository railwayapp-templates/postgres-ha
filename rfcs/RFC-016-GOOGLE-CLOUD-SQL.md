# RFC-016: Google Cloud SQL for PostgreSQL

## Overview

Google Cloud SQL is Google Cloud's fully managed relational database service supporting PostgreSQL, MySQL, and SQL Server. For high availability, Cloud SQL offers regional instances with automatic failover between zones. Cloud SQL uses synchronous replication to a standby instance in a different zone, providing automatic failover with zero data loss. It integrates deeply with Google Cloud services and offers features like automatic storage increases, automated backups, and maintenance.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Client Applications                         │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Cloud SQL Connection                         │
│         project:region:instance-name (Private/Public IP)        │
│              OR Cloud SQL Auth Proxy                            │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                        Regional Instance                        │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │                        Zone A                               ││
│  │   ┌───────────────────────────────────────────────────┐    ││
│  │   │              Primary Instance                      │    ││
│  │   │   ┌─────────────┐    ┌─────────────┐             │    ││
│  │   │   │ PostgreSQL  │    │ Persistent  │             │    ││
│  │   │   │   Server    │────│    Disk     │             │    ││
│  │   │   │             │    │   (SSD)     │             │    ││
│  │   │   └─────────────┘    └─────────────┘             │    ││
│  │   └───────────────────────────────────────────────────┘    ││
│  └─────────────────────────────────────────────────────────────┘│
│                              │                                  │
│                   Synchronous Replication                       │
│                              │                                  │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │                        Zone B                               ││
│  │   ┌───────────────────────────────────────────────────┐    ││
│  │   │              Standby Instance                      │    ││
│  │   │   ┌─────────────┐    ┌─────────────┐             │    ││
│  │   │   │ PostgreSQL  │    │ Persistent  │             │    ││
│  │   │   │   Server    │────│    Disk     │             │    ││
│  │   │   │  (Standby)  │    │  (Replica)  │             │    ││
│  │   │   └─────────────┘    └─────────────┘             │    ││
│  │   └───────────────────────────────────────────────────┘    ││
│  └─────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────┘

                    Read Replicas (Optional)
┌───────────────┐     ┌───────────────┐     ┌───────────────┐
│ Read Replica  │     │ Read Replica  │     │ Cross-Region  │
│   (Zone C)    │     │   (Zone A)    │     │   Replica     │
│    Async      │     │    Async      │     │    Async      │
└───────────────┘     └───────────────┘     └───────────────┘
```

## Instance Types

### 1. Zonal Instance (No HA)

```
Zonal Instance:
─────────────────────────────────────────────────────────────────

Single zone deployment:
    - One PostgreSQL instance
    - One persistent disk
    - No automatic failover
    - Lower cost
    - Suitable for dev/test

Zone A only:
    ┌─────────────┐
    │  Instance   │
    │ PostgreSQL  │
    │     +       │
    │   Disk      │
    └─────────────┘
```

### 2. Regional Instance (HA)

```
Regional Instance (High Availability):
─────────────────────────────────────────────────────────────────

Two-zone deployment:
    - Primary in Zone A
    - Standby in Zone B
    - Synchronous replication
    - Automatic failover
    - Production recommended

Zone A (Primary)        Zone B (Standby)
┌─────────────┐         ┌─────────────┐
│  Primary    │         │  Standby    │
│ PostgreSQL  │ ──────► │ PostgreSQL  │
│     +       │  Sync   │     +       │
│   Disk      │   Rep   │   Disk      │
└─────────────┘         └─────────────┘
```

## How HA Works

### Synchronous Replication

```
Write Path:
─────────────────────────────────────────────────────────────────

t=0ms   Client: INSERT INTO orders VALUES (...)

t=1ms   Primary receives write
        - Writes to PostgreSQL WAL

t=5ms   WAL persisted to primary disk
        - Persistent Disk (PD-SSD)

t=6ms   WAL streamed to standby
        - Synchronous streaming replication

t=15ms  Standby receives WAL
        - Applies to standby disk

t=20ms  Standby acknowledges
        - Confirms durability

t=21ms  Primary commits
        - Transaction visible

t=22ms  Client receives success

Guarantee: Data in 2 zones before commit
           RPO = 0 (zero data loss)
```

### Failover Process

```
Automatic Failover:
─────────────────────────────────────────────────────────────────

t=0s    Primary instance fails
        - Zone A outage, or
        - Instance crash, or
        - Disk failure

t=5s    Cloud SQL detects failure
        - Health checks fail
        - Replication stops

t=10s   Failover decision
        - Cloud SQL orchestrator
        - Initiates promotion

t=30s   Standby promoted
        - Becomes new primary
        - Starts accepting writes

t=45s   Network configuration updated
        - Same IP address maintained
        - Internal DNS updated

t=60s   Failover complete
        - Clients can reconnect

t=2-5min New standby provisioned
        - In different zone (if available)
        - Synchronous replication resumed

Total failover time: ~60 seconds
        (Google SLA: minutes)
```

### Health Monitoring

```
Cloud SQL Health Checks:
─────────────────────────────────────────────────────────────────

Continuous monitoring:
    - Instance responsiveness
    - Disk health
    - Replication status
    - Resource utilization

Failure detection:
    - Connection failures
    - Query timeouts
    - Replication lag spikes
    - Zone-level issues

Actions:
    - Automatic failover (if HA enabled)
    - Alert notifications
    - Logging to Cloud Logging
```

## Configuration

### Creating HA Instance (gcloud)

```bash
# Create regional (HA) instance
gcloud sql instances create mydb-production \
    --database-version=POSTGRES_16 \
    --tier=db-custom-4-16384 \
    --region=us-central1 \
    --availability-type=REGIONAL \  # HA enabled
    --storage-type=SSD \
    --storage-size=100GB \
    --storage-auto-increase \
    --backup-start-time=02:00 \
    --maintenance-window-day=SUN \
    --maintenance-window-hour=03 \
    --network=projects/myproject/global/networks/default \
    --no-assign-ip \  # Private IP only
    --enable-point-in-time-recovery

# Create database
gcloud sql databases create myapp --instance=mydb-production

# Create user
gcloud sql users create appuser \
    --instance=mydb-production \
    --password=secretpassword
```

### Creating HA Instance (Terraform)

```hcl
resource "google_sql_database_instance" "postgres" {
  name             = "mydb-production"
  database_version = "POSTGRES_16"
  region           = "us-central1"

  settings {
    tier              = "db-custom-4-16384"
    availability_type = "REGIONAL"  # HA enabled

    disk_type       = "PD_SSD"
    disk_size       = 100
    disk_autoresize = true

    backup_configuration {
      enabled                        = true
      start_time                     = "02:00"
      point_in_time_recovery_enabled = true
      transaction_log_retention_days = 7
      backup_retention_settings {
        retained_backups = 7
      }
    }

    maintenance_window {
      day          = 7  # Sunday
      hour         = 3
      update_track = "stable"
    }

    ip_configuration {
      ipv4_enabled    = false
      private_network = google_compute_network.vpc.id
    }

    database_flags {
      name  = "log_min_duration_statement"
      value = "1000"
    }

    insights_config {
      query_insights_enabled = true
    }
  }

  deletion_protection = true
}

# Read replica (optional)
resource "google_sql_database_instance" "replica" {
  name                 = "mydb-replica-1"
  master_instance_name = google_sql_database_instance.postgres.name
  region               = "us-central1"
  database_version     = "POSTGRES_16"

  replica_configuration {
    failover_target = false
  }

  settings {
    tier              = "db-custom-2-8192"
    availability_type = "ZONAL"  # Replica doesn't need HA
  }
}
```

### Connection Methods

```python
# Method 1: Direct connection (private IP)
import psycopg2

conn = psycopg2.connect(
    host="10.0.0.5",  # Private IP
    port=5432,
    database="myapp",
    user="appuser",
    password="secretpassword",
    sslmode="require"
)

# Method 2: Cloud SQL Auth Proxy (recommended)
# Start proxy: cloud-sql-proxy project:region:instance
conn = psycopg2.connect(
    host="127.0.0.1",
    port=5432,
    database="myapp",
    user="appuser",
    password="secretpassword"
)

# Method 3: Cloud SQL Connector (Python)
from google.cloud.sql.connector import Connector

connector = Connector()

def get_conn():
    return connector.connect(
        "project:us-central1:mydb-production",
        "pg8000",
        user="appuser",
        password="secretpassword",
        db="myapp"
    )

# Method 4: IAM authentication
conn = psycopg2.connect(
    host="10.0.0.5",
    port=5432,
    database="myapp",
    user="myuser@myproject.iam",  # IAM user
    password=access_token,  # OAuth token
    sslmode="require"
)
```

## Happy Path Scenarios

### Scenario 1: Normal Operations

```
Timeline: Steady-state with HA
─────────────────────────────────────────────────────────────────

Regional Instance:
    - Primary in us-central1-a
    - Standby in us-central1-b
    - Synchronous replication active

All traffic:
    └─► Private IP (10.0.0.5)
        └─► Primary Instance
            └─► Sync to Standby

Every write:
    1. Primary receives query
    2. WAL written locally
    3. WAL sent to standby
    4. Standby acknowledges
    5. Commit confirmed

Status: Both zones have all committed data
```

### Scenario 2: Adding Read Replica

```
Timeline: Scaling reads
─────────────────────────────────────────────────────────────────

t=0     Create read replica
        gcloud sql instances create mydb-replica \
            --master-instance-name=mydb-production

t=1min  Replica provisioning starts
        - GCP creates new instance
        - Snapshot from primary

t=10min Replica ready
        - Async streaming replication
        - Separate IP address

Usage:
        Primary IP: 10.0.0.5 (writes + reads)
        Replica IP: 10.0.0.6 (reads only)

Application routing:
        - Route writes to primary
        - Route reads to replica
        - Application handles routing

Note: Replica separate from HA standby
      Standby: Sync, not readable, same region
      Replica: Async, readable, any region
```

### Scenario 3: Cross-Region Replica

```
Timeline: Disaster recovery setup
─────────────────────────────────────────────────────────────────

Primary: us-central1 (HA enabled)
DR Replica: us-east1

t=0     Create cross-region replica
        gcloud sql instances create mydb-dr \
            --master-instance-name=mydb-production \
            --region=us-east1

t=30min Replica ready in us-east1
        - Async replication
        - Lag: seconds to minutes

DR Scenario:
        t=0     us-central1 region fails
        t=1min  Promote replica to standalone
                gcloud sql instances promote-replica mydb-dr
        t=5min  Replica is now primary
                - Update application to use us-east1
                - Some data loss possible (async)

RTO: ~5-10 minutes
RPO: Replication lag (seconds to minutes)
```

## Unhappy Path Scenarios

### Scenario 1: Primary Zone Failure

```
Timeline: Zone A outage
─────────────────────────────────────────────────────────────────

t=0s    Zone us-central1-a fails
        - Primary instance unreachable
        - Standby in us-central1-b healthy

t=5s    Cloud SQL detects failure
        - Health checks fail
        - Standby still connected to storage

t=10s   Failover initiated
        - Cloud SQL orchestrator decides

t=30s   Standby promoted
        - PostgreSQL comes out of recovery
        - Starts accepting writes

t=45s   Network updated
        - Same private IP (10.0.0.5)
        - Routes to new primary in Zone B

t=60s   Applications reconnect
        - Same connection string works
        - New connections succeed

t=5min  New standby created
        - In us-central1-c (different zone)
        - HA restored

Impact: ~60 seconds downtime
        Zero data loss (synchronous replication)
```

### Scenario 2: Maintenance Window

```
Timeline: Scheduled maintenance
─────────────────────────────────────────────────────────────────

Maintenance window: Sunday 03:00-04:00

t=03:00 Maintenance begins
        - Cloud SQL needs to update primary

t=03:01 Failover to standby
        - Same as automatic failover
        - ~60 seconds impact

t=03:02 Applications reconnected
        - Now using former standby

t=03:05 Original primary updated
        - Patches applied
        - Rejoins as standby

t=03:10 Maintenance complete
        - No second failover unless needed
        - HA fully restored

With maintenance denial period:
        gcloud sql instances patch mydb-production \
            --deny-maintenance-period-start-date=2024-12-01 \
            --deny-maintenance-period-end-date=2024-12-31
```

### Scenario 3: Storage Full

```
Timeline: Disk space exhaustion
─────────────────────────────────────────────────────────────────

Without auto-increase:
    t=0     Disk at 99%
    t=1min  PostgreSQL stops accepting writes
    t=2min  Connections fail
    Manual: Increase disk size
            gcloud sql instances patch mydb-production \
                --storage-size=200GB

With auto-increase (recommended):
    t=0     Disk at 90%
    t=0s    Cloud SQL detects
    t=1min  Automatic disk increase triggered
    t=5min  Additional storage added
    t=5min  Continues operating normally

Recommendation: Always enable storage-auto-increase
```

### Scenario 4: Connection Exhaustion

```
Timeline: Too many connections
─────────────────────────────────────────────────────────────────

t=0     Connection count approaching limit
        - db-custom-4-16384 default: ~800 connections

t=5min  Max connections reached
        - New connections rejected
        - "too many connections" error

t=5min  Options:
        1. Kill idle connections
           SELECT pg_terminate_backend(pid)
           FROM pg_stat_activity
           WHERE state = 'idle';

        2. Scale up instance
           gcloud sql instances patch mydb-production \
               --tier=db-custom-8-32768

        3. Use connection pooling
           - Cloud SQL Auth Proxy doesn't pool
           - Use PgBouncer or similar

Prevention:
        - Monitor connection count
        - Implement connection pooling
        - Set appropriate timeouts
        - Use serverless connections (where supported)
```

### Scenario 5: Failover Fails

```
Timeline: Standby unavailable during failover
─────────────────────────────────────────────────────────────────

t=0s    Primary fails
t=5s    Cloud SQL attempts failover
t=10s   Standby also unhealthy (rare)
        - Both zones affected
        - Or standby was already failing

t=15s   Failover cannot complete
        - No healthy standby

t=20s   Cloud SQL attempts recovery
        - Try to repair primary
        - Or create new standby

t=???   Manual intervention may be needed
        - GCP support engagement
        - Point-in-time recovery from backup

Impact: Extended outage
        Recovery depends on failure mode

Mitigation:
        - Monitor replication lag
        - Monitor standby health
        - Have cross-region replica as DR
```

## Tradeoffs

### Advantages

| Aspect | Benefit |
|--------|---------|
| **Fully Managed** | GCP handles infrastructure, patching |
| **Automatic Failover** | No manual intervention for HA |
| **Synchronous Replication** | Zero data loss (RPO = 0) |
| **Auto Storage Increase** | Never run out of disk space |
| **Point-in-Time Recovery** | Restore to any second |
| **IAM Integration** | Use GCP IAM for authentication |
| **Private Service Connect** | Secure private connectivity |
| **Cloud Logging/Monitoring** | Integrated observability |

### Disadvantages

| Aspect | Limitation |
|--------|------------|
| **Failover Time** | ~60 seconds (longer than some alternatives) |
| **Standby Not Readable** | Cannot serve read traffic |
| **Single Region HA** | Multi-region requires replicas |
| **Limited Control** | No OS access, limited tuning |
| **Extension Limitations** | Not all extensions available |
| **Cost** | Pay for standby (doesn't serve traffic) |
| **Write Latency** | Synchronous cross-zone replication |
| **Max Instance Size** | Limited by machine types |

## Comparison with Alternatives

| Feature | Cloud SQL | AlloyDB | RDS | Aurora |
|---------|-----------|---------|-----|--------|
| Failover Time | ~60s | <60s | 60-120s | 15-30s |
| Read Standby | No | Yes | No | Yes |
| Storage | Per instance | Shared | Per instance | Shared |
| Max Storage | 64TB | 64TB | 64TB | 128TB |
| PostgreSQL | Standard | Compatible | Standard | Compatible |
| Columnar | No | Yes | No | No |
| Price | $$ | $$$ | $$ | $$$ |

## Cloud SQL vs AlloyDB

```
When to choose Cloud SQL:
─────────────────────────────────────────────────────────────────
- Standard PostgreSQL needed
- Cost-sensitive workload
- Simpler requirements
- Smaller databases

When to choose AlloyDB:
─────────────────────────────────────────────────────────────────
- Need faster failover
- Read-heavy workloads (readable standby)
- Analytics workloads (columnar engine)
- Larger scale requirements
- Performance-critical applications
```

## Limitations

1. **Failover Duration**: ~60 seconds is longer than some alternatives
2. **Standby Access**: Cannot read from HA standby
3. **Region Bound**: Multi-region requires separate replicas
4. **Extension Support**: Some extensions unavailable
5. **Instance Sizing**: Limited to available machine types
6. **Custom Configs**: Some PostgreSQL settings restricted
7. **No Logical Replication**: As source (only as target)
8. **Maintenance Impact**: May cause brief failovers

## Best Practices

1. **Enable HA**: Always for production (`availability_type=REGIONAL`)
2. **Use Private IP**: More secure, better performance
3. **Enable Auto-increase**: Prevent storage exhaustion
4. **Use Cloud SQL Proxy**: Simplified, secure connections
5. **Configure Backups**: Enable PITR with appropriate retention
6. **Monitor Replication**: Watch for lag indicators
7. **Plan Maintenance**: Set appropriate windows
8. **Add Read Replicas**: For read scaling and DR
9. **Use IAM Auth**: For better security management
10. **Test Failover**: Verify application handles reconnects

## Conclusion

Google Cloud SQL for PostgreSQL provides a solid managed HA solution with automatic failover and synchronous replication. It's well-integrated with Google Cloud services and offers standard PostgreSQL compatibility. The main tradeoffs are the ~60 second failover time and the inability to read from the HA standby. For more demanding requirements, consider AlloyDB for faster failover and readable standbys.

**Recommended for:**
- GCP-centric organizations
- Teams wanting fully managed PostgreSQL
- Standard workloads tolerant of ~60s failover
- Applications requiring GCP integration

**Not recommended for:**
- Sub-30-second failover requirements (use AlloyDB)
- Heavy read workloads needing HA standby reads
- Multi-cloud deployments
- Workloads requiring unsupported extensions
