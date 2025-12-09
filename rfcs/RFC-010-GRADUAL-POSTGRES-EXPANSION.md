# RFC-010: Gradual PostgreSQL Expansion via Data UI

**Status**: Draft
**Created**: 2025-12-08
**Author**: Platform Team

## Overview

This RFC proposes a user-facing feature that allows customers to gradually expand their PostgreSQL infrastructure through the Railway Data UI. Users start with a single PostgreSQL instance and can progressively add components—connection pooling (Pgpool), read replicas, and full high availability (HA) mode—as their needs grow.

## Motivation

Currently, deploying a highly-available PostgreSQL cluster requires:
1. Deep understanding of Patroni, etcd, and Pgpool architecture
2. Manual deployment of 7+ services (3 etcd, 3 PostgreSQL, 1+ Pgpool)
3. Careful coordination of environment variables and networking
4. Significant upfront cost even for simple use cases

Most users don't need full HA from day one. They start with a single database and want to scale incrementally as their application grows. This RFC enables that journey through a simple UI-driven workflow.

## Goals

1. **Progressive Complexity**: Users pay only for what they need
2. **Zero-Downtime Upgrades**: Each expansion step maintains availability
3. **Reversibility**: Users can scale down (with appropriate warnings)
4. **Transparency**: Clear visibility into current topology and costs
5. **Automation**: One-click expansion with sensible defaults

## Non-Goals

- Multi-region PostgreSQL (separate RFC)
- Automatic scaling based on load (future enhancement)
- Migration from external PostgreSQL providers
- Support for PostgreSQL versions older than 15

---

## Expansion Stages

### Stage 0: Single PostgreSQL Instance (Baseline)

**Components**: 1 PostgreSQL service

```
┌─────────────────────────┐
│   Application           │
└───────────┬─────────────┘
            │
            ▼
┌─────────────────────────┐
│   PostgreSQL (Primary)  │
│   └─ Volume: 10GB       │
└─────────────────────────┘
```

**Characteristics**:
- Standard Railway PostgreSQL deployment
- No HA, no connection pooling
- Direct connection to database
- Suitable for development and low-traffic production

**Data UI Tab**: Shows database metrics, query logs, basic monitoring

---

### Stage 1: Add Connection Pooling (Pgpool)

**Components**: 1 PostgreSQL + 1 Pgpool

```
┌─────────────────────────┐
│   Application           │
└───────────┬─────────────┘
            │
            ▼
┌─────────────────────────┐
│   Pgpool-II (1 replica) │
│   └─ 128 connections    │
└───────────┬─────────────┘
            │
            ▼
┌─────────────────────────┐
│   PostgreSQL (Primary)  │
│   └─ Volume: 10GB       │
└─────────────────────────┘
```

**User Action**: Click "Add Connection Pooling" in Data UI

**What Happens**:
1. System deploys Pgpool service with single replica
2. Pgpool configured to connect to existing PostgreSQL
3. New connection string provided (pgpool endpoint)
4. Original PostgreSQL endpoint remains available (optional direct access)

**Benefits**:
- Connection multiplexing (32 workers × 4 pools = 128 effective connections)
- Query caching (optional)
- Prepared statement optimization
- Load balancing ready for future replicas

**Configuration Options** (shown in UI):
| Setting | Default | Description |
|---------|---------|-------------|
| Pool Size | 128 | Max pooled connections |
| Connection Timeout | 30s | Client connection timeout |
| Statement Timeout | 0 (none) | Query execution limit |

**Estimated Additional Cost**: ~$5/month (0.5 vCPU, 512MB RAM)

---

### Stage 2: Add Read Replica(s)

**Components**: 1 PostgreSQL Primary + N PostgreSQL Replicas + 1 Pgpool

```
┌─────────────────────────┐
│   Application           │
└───────────┬─────────────┘
            │
            ▼
┌─────────────────────────┐
│   Pgpool-II             │
│   ├─ Writes → Primary   │
│   └─ Reads → Replicas   │
└───────────┬─────────────┘
            │
    ┌───────┴───────┐
    ▼               ▼
┌─────────┐   ┌─────────────┐
│ Primary │   │ Replica (1) │
│ (r/w)   │──▶│ (read-only) │
└─────────┘   └─────────────┘
     streaming replication
```

**User Action**: Click "Add Read Replica" in Data UI

**What Happens**:
1. System provisions new PostgreSQL service
2. Configures streaming replication from primary
3. Updates Pgpool backend configuration
4. Enables read/write splitting in Pgpool

**Technical Implementation**:
```yaml
# New replica configuration
postgresql:
  parameters:
    hot_standby: on
    primary_conninfo: "host=postgres-primary port=5432 user=replicator"
    primary_slot_name: "replica_1_slot"
```

**Pgpool Updates**:
```conf
backend_hostname1 = 'postgres-replica-1.railway.internal'
backend_port1 = 5432
backend_weight1 = 1
backend_flag1 = 'ALLOW_TO_FAILOVER'
```

**Configuration Options**:
| Setting | Default | Options |
|---------|---------|---------|
| Number of Replicas | 1 | 1-5 |
| Replica Region | Same as primary | Multi-region (future) |
| Replication Mode | Async | Async, Sync |
| Read Load Balancing | Round-robin | Round-robin, Least-connections |

**Benefits**:
- Offload read queries to replicas
- Improved read performance
- Disaster recovery standby
- Near-zero replication lag (async mode)

**Estimated Additional Cost**: ~$15/month per replica (1 vCPU, 1GB RAM, 10GB volume)

---

### Stage 3: Enable High Availability Mode

**Components**: 3 etcd + 3 PostgreSQL (Patroni) + 3 Pgpool

```
┌─────────────────────────────────────────────────────┐
│                    Application                       │
└───────────────────────┬─────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────┐
│              Pgpool-II Cluster (3 replicas)          │
│   ┌─────────┐  ┌─────────┐  ┌─────────┐             │
│   │ pgpool-1│  │ pgpool-2│  │ pgpool-3│             │
│   └────┬────┘  └────┬────┘  └────┬────┘             │
│        └────────────┼───────────┘                   │
└─────────────────────┼───────────────────────────────┘
                      │
     ┌────────────────┼────────────────┐
     ▼                ▼                ▼
┌─────────┐     ┌─────────┐     ┌─────────┐
│postgres-1│    │postgres-2│    │postgres-3│
│ (Leader) │◀──▶│(Standby) │◀──▶│(Standby) │
│ Patroni  │    │ Patroni  │    │ Patroni  │
└────┬─────┘    └────┬─────┘    └────┬─────┘
     │               │               │
     └───────────────┼───────────────┘
                     │ Leader Election
     ┌───────────────┼───────────────┐
     ▼               ▼               ▼
┌─────────┐   ┌─────────┐   ┌─────────┐
│ etcd-1  │◀─▶│ etcd-2  │◀─▶│ etcd-3  │
│         │   │         │   │         │
└─────────┘   └─────────┘   └─────────┘
          Distributed Consensus
```

**User Action**: Click "Enable High Availability" in Data UI

**What Happens** (Orchestrated Migration):

#### Phase 1: Deploy etcd Cluster (5-10 min)
1. Provision 3 etcd services
2. Wait for quorum establishment
3. Health check: `etcdctl endpoint health`

#### Phase 2: Convert Primary to Patroni (10-15 min)
1. Create maintenance window notification
2. Take snapshot of existing PostgreSQL
3. Deploy new Patroni-enabled PostgreSQL (postgres-1)
4. Restore data from snapshot
5. Validate data integrity
6. Update Pgpool to point to new primary
7. Deprecate old PostgreSQL service

#### Phase 3: Bootstrap Standby Nodes (10-15 min per node)
1. Deploy postgres-2, postgres-3 as Patroni standbys
2. Configure streaming replication
3. Wait for initial sync completion
4. Register with etcd cluster

#### Phase 4: Scale Pgpool Cluster (5 min)
1. Scale Pgpool to 3 replicas
2. Enable patroni-watcher on all replicas
3. Verify backend detection

#### Phase 5: Finalization (2 min)
1. Run full health check
2. Test failover (optional, user-initiated)
3. Update connection strings
4. Send completion notification

**Total Migration Time**: ~45-60 minutes (mostly automated)

**Configuration Options**:
| Setting | Default | Options |
|---------|---------|---------|
| Failover Time | <30s | 10s, 30s, 60s |
| Replication Mode | Async | Async, Sync (1 node), Sync (all) |
| Auto-Failback | Disabled | Enabled, Disabled |
| Watchdog Mode | Enabled | Enabled, Disabled |

**Benefits**:
- Automatic failover (<10 seconds with tuning)
- Zero-downtime leader election
- Split-brain prevention
- Self-healing cluster
- Production-grade reliability

**Estimated Additional Cost**: ~$50/month total
- 3 etcd nodes: ~$15 (3 × 0.5 vCPU, 512MB)
- 3 PostgreSQL nodes: ~$45 (3 × 1 vCPU, 1GB, 10GB volume)
- 3 Pgpool replicas: ~$15 (3 × 0.5 vCPU, 512MB)
- Less existing single-node cost

---

## Data UI Design

### Database Service Page - New "Scale" Tab

```
┌──────────────────────────────────────────────────────────────────┐
│  PostgreSQL: my-app-db                                           │
├──────────────────────────────────────────────────────────────────┤
│  [Overview] [Metrics] [Query Logs] [Backups] [Scale] [Settings]  │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Current Topology                                                │
│  ─────────────────                                               │
│                                                                  │
│  ┌─────────────┐                                                 │
│  │  Primary    │  ● postgres-primary                             │
│  │  10GB SSD   │    us-east-1                                    │
│  └─────────────┘                                                 │
│                                                                  │
│  ─────────────────────────────────────────────────────────────── │
│                                                                  │
│  Expansion Options                                               │
│  ─────────────────                                               │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ ◉ Connection Pooling                          [Add Pgpool]│   │
│  │   Recommended for: 50+ concurrent connections             │   │
│  │   Adds: 1 Pgpool service (~$5/mo)                        │   │
│  └──────────────────────────────────────────────────────────┘   │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ ◎ Read Replica                            [Add Replica]   │   │
│  │   Recommended for: Read-heavy workloads                   │   │
│  │   Requires: Connection Pooling                            │   │
│  │   Adds: 1 PostgreSQL replica (~$15/mo)                   │   │
│  └──────────────────────────────────────────────────────────┘   │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ ◎ High Availability                       [Enable HA]     │   │
│  │   Recommended for: Production workloads                   │   │
│  │   Requires: Connection Pooling                            │   │
│  │   Adds: 3 etcd, 2 PostgreSQL replicas (~$50/mo total)    │   │
│  │   Features: Auto-failover, split-brain prevention         │   │
│  └──────────────────────────────────────────────────────────┘   │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
```

### Stage 3 (HA Mode) - Expanded View

```
┌──────────────────────────────────────────────────────────────────┐
│  PostgreSQL: my-app-db (High Availability)                       │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Cluster Status: ● Healthy                    [Manage Cluster]   │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │  CONSENSUS LAYER (etcd)                                     ││
│  │  ┌─────────┐ ┌─────────┐ ┌─────────┐                       ││
│  │  │ etcd-1  │ │ etcd-2  │ │ etcd-3  │                       ││
│  │  │ ● Leader│ │ ○ Follow│ │ ○ Follow│                       ││
│  │  └─────────┘ └─────────┘ └─────────┘                       ││
│  └─────────────────────────────────────────────────────────────┘│
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │  DATABASE LAYER (PostgreSQL + Patroni)                      ││
│  │  ┌─────────────┐ ┌─────────────┐ ┌─────────────┐           ││
│  │  │ postgres-1  │ │ postgres-2  │ │ postgres-3  │           ││
│  │  │ ★ Primary   │ │ ○ Standby   │ │ ○ Standby   │           ││
│  │  │ Lag: 0      │ │ Lag: 24KB   │ │ Lag: 24KB   │           ││
│  │  └─────────────┘ └─────────────┘ └─────────────┘           ││
│  │                                                             ││
│  │  [Switchover to postgres-2 ▾]  [Reinitialize Node ▾]       ││
│  └─────────────────────────────────────────────────────────────┘│
│                                                                  │
│  ┌─────────────────────────────────────────────────────────────┐│
│  │  CONNECTION LAYER (Pgpool)                                  ││
│  │  Replicas: 3    Active Connections: 47/384                  ││
│  │  [Scale Pool Size ▾]                                        ││
│  └─────────────────────────────────────────────────────────────┘│
│                                                                  │
│  Recent Events                                                   │
│  ─────────────                                                   │
│  • 2 hours ago: Automatic failover postgres-2 → postgres-1      │
│  • 3 hours ago: postgres-1 marked unhealthy                     │
│  • 1 day ago: postgres-3 added to cluster                       │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
```

---

## API Design

### REST Endpoints

```
# Stage 1: Add Connection Pooling
POST /v1/projects/{projectId}/services/{serviceId}/postgres/pooling
{
  "enabled": true,
  "pool_size": 128,
  "connection_timeout_seconds": 30
}

# Stage 2: Add Read Replica
POST /v1/projects/{projectId}/services/{serviceId}/postgres/replicas
{
  "count": 1,
  "replication_mode": "async",
  "load_balancing": "round_robin"
}

# Stage 3: Enable HA
POST /v1/projects/{projectId}/services/{serviceId}/postgres/ha
{
  "enabled": true,
  "failover_timeout_seconds": 30,
  "sync_mode": "async",
  "auto_failback": false
}

# Get current topology
GET /v1/projects/{projectId}/services/{serviceId}/postgres/topology
Response:
{
  "stage": 3,
  "pooling": { "enabled": true, "replicas": 3 },
  "replicas": { "count": 2, "lag_bytes": [0, 24576, 24576] },
  "ha": {
    "enabled": true,
    "etcd_nodes": 3,
    "patroni_nodes": 3,
    "current_leader": "postgres-1"
  }
}

# Manual switchover
POST /v1/projects/{projectId}/services/{serviceId}/postgres/switchover
{
  "target_node": "postgres-2"
}
```

### GraphQL Mutations

```graphql
mutation EnablePostgresPooling($serviceId: ID!, $config: PoolingConfig!) {
  postgresEnablePooling(serviceId: $serviceId, config: $config) {
    success
    poolingEndpoint
    estimatedMonthlyCost
  }
}

mutation AddPostgresReplica($serviceId: ID!, $count: Int!) {
  postgresAddReplica(serviceId: $serviceId, count: $count) {
    success
    replicas {
      id
      status
      lagBytes
    }
  }
}

mutation EnablePostgresHA($serviceId: ID!, $config: HAConfig!) {
  postgresEnableHA(serviceId: $serviceId, config: $config) {
    success
    migrationJobId
    estimatedDurationMinutes
  }
}
```

---

## Database Migrations & State Management

### Service Metadata Schema

```sql
-- New table to track PostgreSQL expansion state
CREATE TABLE postgres_cluster_state (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  service_id UUID NOT NULL REFERENCES services(id),
  stage INT NOT NULL DEFAULT 0,  -- 0=single, 1=pooling, 2=replicas, 3=ha

  -- Stage 1: Pooling
  pooling_enabled BOOLEAN DEFAULT FALSE,
  pooling_service_id UUID REFERENCES services(id),
  pool_size INT DEFAULT 128,

  -- Stage 2: Replicas
  replica_count INT DEFAULT 0,
  replication_mode VARCHAR(10) DEFAULT 'async',

  -- Stage 3: HA
  ha_enabled BOOLEAN DEFAULT FALSE,
  etcd_service_ids UUID[] DEFAULT '{}',
  patroni_service_ids UUID[] DEFAULT '{}',
  current_leader_id UUID,

  -- Timestamps
  created_at TIMESTAMPTZ DEFAULT NOW(),
  updated_at TIMESTAMPTZ DEFAULT NOW(),

  CONSTRAINT valid_stage CHECK (stage >= 0 AND stage <= 3)
);

-- Track expansion history
CREATE TABLE postgres_expansion_events (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  cluster_id UUID NOT NULL REFERENCES postgres_cluster_state(id),
  event_type VARCHAR(50) NOT NULL,  -- 'stage_upgrade', 'failover', 'replica_added'
  from_stage INT,
  to_stage INT,
  metadata JSONB,
  created_at TIMESTAMPTZ DEFAULT NOW()
);
```

---

## Scaling Down (Reverting Stages)

### Stage 3 → Stage 2: Disable HA

**Warning**: "Disabling HA will remove automatic failover. Your database will have reduced fault tolerance."

**Process**:
1. Ensure primary node is healthy
2. Remove standby nodes from Patroni cluster
3. Convert primary back to standalone PostgreSQL
4. Decommission etcd cluster
5. Scale Pgpool to single replica
6. Update connection endpoints

**Data Preservation**: All data retained on primary node

### Stage 2 → Stage 1: Remove Replicas

**Warning**: "Removing replicas will reduce read capacity and eliminate disaster recovery standby."

**Process**:
1. Gracefully remove replicas from Pgpool
2. Stop replication on replica nodes
3. Delete replica services and volumes
4. Update Pgpool configuration

### Stage 1 → Stage 0: Remove Connection Pooling

**Warning**: "Removing connection pooling may cause connection issues if your application opens many database connections."

**Process**:
1. Provide updated direct connection string
2. Grace period (configurable, default 1 hour) for connection migration
3. Decommission Pgpool service

---

## Error Handling & Rollback

### Expansion Failure Scenarios

| Failure Point | Automatic Recovery | Manual Recovery |
|--------------|-------------------|-----------------|
| etcd cluster won't form | Retry 3x, then rollback | Check networking, retry |
| Primary won't convert to Patroni | Restore from snapshot | Contact support |
| Replica won't sync | Retry replication setup | Check disk space, WAL retention |
| Pgpool can't detect backends | Update backend config | Check Patroni endpoints |

### Rollback Procedure

Each expansion step creates a checkpoint:
1. **Snapshot** of current state
2. **Reversible changes** tracked
3. **Timeout** (30 min default) triggers automatic rollback if unhealthy

```yaml
expansion_checkpoint:
  stage: 2
  timestamp: 2025-12-08T10:00:00Z
  snapshot_id: snap_abc123
  services_created:
    - service_id: svc_replica1
    - service_id: svc_pgpool
  rollback_available_until: 2025-12-08T10:30:00Z
```

---

## Billing & Cost Transparency

### Cost Breakdown UI Component

```
┌─────────────────────────────────────────────────────────────────┐
│  Estimated Monthly Cost                                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  Current (Stage 0):                               $15/month     │
│  └─ PostgreSQL Primary (1 vCPU, 1GB, 10GB SSD)                  │
│                                                                  │
│  After Adding Pgpool (Stage 1):                   +$5/month     │
│  └─ Pgpool (0.5 vCPU, 512MB RAM)                                │
│                                                                  │
│  After Adding 2 Replicas (Stage 2):               +$30/month    │
│  └─ 2× PostgreSQL Replica (1 vCPU, 1GB, 10GB SSD each)          │
│                                                                  │
│  After Enabling HA (Stage 3):                     +$25/month    │
│  └─ 3× etcd (0.5 vCPU, 512MB each)                              │
│  └─ 2× additional Pgpool replicas                               │
│                                                                  │
│  Total (Full HA):                                 $75/month     │
│                                                                  │
│  [View Detailed Breakdown]                                       │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## Implementation Phases

### Phase 1: Foundation (Weeks 1-3)
- [ ] Database schema for cluster state tracking
- [ ] API endpoints for topology queries
- [ ] Pgpool deployment automation (Stage 1)
- [ ] Basic Data UI "Scale" tab

### Phase 2: Read Replicas (Weeks 4-6)
- [ ] Streaming replication automation
- [ ] Pgpool multi-backend configuration
- [ ] Read replica deployment (Stage 2)
- [ ] Replication lag monitoring

### Phase 3: High Availability (Weeks 7-10)
- [ ] etcd cluster deployment automation
- [ ] Patroni migration procedure
- [ ] Failover testing automation
- [ ] Full HA deployment (Stage 3)

### Phase 4: Polish & Observability (Weeks 11-12)
- [ ] Enhanced cluster visualization
- [ ] Failover event notifications
- [ ] Cost transparency improvements
- [ ] Documentation and user guides

---

## Security Considerations

1. **Credential Rotation**: Automatic rotation of replication passwords
2. **Network Isolation**: All cluster traffic on private network
3. **Encryption**: TLS for replication streams, encryption at rest
4. **Access Control**: Only project members can modify cluster topology
5. **Audit Logging**: All topology changes logged with actor

---

## Monitoring & Alerting

### New Metrics (exposed via Prometheus/Grafana)

```
# Replication metrics
postgresql_replication_lag_bytes{node="postgres-2"}
postgresql_replication_lag_seconds{node="postgres-2"}
postgresql_streaming_replication_connected{node="postgres-2"}

# Patroni metrics
patroni_cluster_leader{cluster="pg-ha-cluster", node="postgres-1"}
patroni_cluster_unlocked{cluster="pg-ha-cluster"}
patroni_failover_count{cluster="pg-ha-cluster"}

# Pgpool metrics
pgpool_backend_status{node="0", status="up"}
pgpool_active_connections
pgpool_pool_utilization_percent

# etcd metrics
etcd_cluster_healthy
etcd_leader_changes_total
```

### Default Alerts

| Alert | Condition | Severity |
|-------|-----------|----------|
| ReplicationLagHigh | lag > 100MB for 5 min | Warning |
| ReplicationBroken | lag = -1 (disconnected) | Critical |
| FailoverOccurred | leader changed | Info |
| ClusterUnhealthy | <2 healthy nodes | Critical |
| etcdQuorumLost | <2 etcd nodes healthy | Critical |

---

## Open Questions

1. **Volume Sizing**: Should replicas have configurable volume sizes, or match primary?
2. **Cross-Region Replicas**: Timeline for multi-region support?
3. **Synchronous Replication**: Should we support sync mode for zero data loss?
4. **Connection String Management**: How to handle endpoint changes transparently?
5. **Backup Integration**: How does this interact with the backup RFC?

---

## Appendix A: Connection String Changes

| Stage | Connection String |
|-------|------------------|
| 0 (Single) | `postgresql://user:pass@postgres.railway.internal:5432/db` |
| 1+ (Pooling) | `postgresql://user:pass@pgpool.railway.internal:5432/db` |
| 2+ (Read Split) | Primary: same as above<br>Replicas: `postgresql://user:pass@pgpool.railway.internal:5433/db` |

## Appendix B: Patroni Configuration Template

```yaml
scope: {{cluster_name}}
name: {{node_name}}

etcd3:
  hosts: {{etcd_hosts}}

bootstrap:
  dcs:
    ttl: 30
    loop_wait: 10
    retry_timeout: 10
    maximum_lag_on_failover: 1048576
    postgresql:
      use_pg_rewind: true
      parameters:
        wal_level: replica
        hot_standby: "on"
        max_wal_senders: 10
        max_replication_slots: 10
        wal_keep_size: 128MB

postgresql:
  listen: 0.0.0.0:5432
  connect_address: {{node_address}}:5432
  data_dir: /var/lib/postgresql/data/pgdata
  authentication:
    superuser:
      username: {{postgres_user}}
      password: {{postgres_password}}
    replication:
      username: replicator
      password: {{replication_password}}
```

---

## References

- [RFC-001: Patroni Integration](./RFC-001-PATRONI.md)
- [RFC-006: Pgpool-II Configuration](./RFC-006-PGPOOL-II.md)
- [RFC-007: Split-Brain Prevention](./RFC-007-HAPROXY-STREAMING-REPLICATION.md)
- [Patroni Documentation](https://patroni.readthedocs.io/)
- [Pgpool-II Documentation](https://www.pgpool.net/docs/latest/en/html/)
