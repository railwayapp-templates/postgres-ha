# RFC-000: PostgreSQL High Availability Solutions - Summary & Comparison

## Document Index

| RFC | Solution | Category | Status |
|-----|----------|----------|--------|
| [RFC-001](./RFC-001-PATRONI.md) | Patroni | Open Source HA Manager | Complete |
| [RFC-002](./RFC-002-STOLON.md) | Stolon | Open Source HA Manager | Complete |
| [RFC-003](./RFC-003-REPMGR.md) | repmgr | Open Source HA Manager | Complete |
| [RFC-004](./RFC-004-PAF-PACEMAKER.md) | PAF (Pacemaker) | Open Source HA Manager | Complete |
| [RFC-005](./RFC-005-PG_AUTO_FAILOVER.md) | pg_auto_failover | Open Source HA Manager | Complete |
| [RFC-006](./RFC-006-PGPOOL-II.md) | Pgpool-II | Middleware/Proxy | Complete |
| [RFC-007](./RFC-007-HAPROXY-STREAMING-REPLICATION.md) | HAProxy + Streaming Replication | Pattern | Complete |
| [RFC-008](./RFC-008-CITUS.md) | Citus | Distributed PostgreSQL | Complete |
| [RFC-009](./RFC-009-EDB-BDR.md) | EDB Postgres Distributed (BDR) | Multi-Master | Complete |
| [RFC-010](./RFC-010-AMAZON-AURORA.md) | Amazon Aurora PostgreSQL | Cloud Managed | Complete |
| [RFC-011](./RFC-011-NEON.md) | Neon | Serverless PostgreSQL | Complete |
| [RFC-012](./RFC-012-COCKROACHDB.md) | CockroachDB | NewSQL | Complete |
| [RFC-013](./RFC-013-YUGABYTEDB.md) | YugabyteDB | Distributed SQL | Complete |

## Solution Categories

### 1. Traditional HA Managers (Single-Writer)
Solutions that manage PostgreSQL streaming replication and automatic failover:

| Solution | DCS Required | Failover Time | Complexity | Best For |
|----------|--------------|---------------|------------|----------|
| **Patroni** | Yes (etcd/Consul/ZK) | 30-60s | Medium | Production, Kubernetes |
| **Stolon** | Yes (etcd/Consul) | 30-60s | Medium | Kubernetes |
| **repmgr** | No (PostgreSQL) | 60s+ | Low | Simple deployments |
| **PAF/Pacemaker** | No (Corosync) | 15-30s | High | Enterprise, datacenter |
| **pg_auto_failover** | No (Monitor node) | 15-25s | Low | Simple HA |

### 2. Middleware/Proxy Solutions
Solutions that provide connection routing, pooling, or load balancing:

| Solution | Connection Pooling | Load Balancing | Failover | Best For |
|----------|-------------------|----------------|----------|----------|
| **Pgpool-II** | Yes | Query-aware | Script-based | Read scaling + pooling |
| **HAProxy** | No | TCP/HTTP | Health check | Routing only |
| **PgBouncer** | Yes | No | No | Pure pooling |

### 3. Distributed PostgreSQL (Scale-Out)
Solutions that enable horizontal scaling through sharding:

| Solution | Sharding | Multi-Master | PostgreSQL Compat | Best For |
|----------|----------|--------------|-------------------|----------|
| **Citus** | Hash | No | Extension | Multi-tenant SaaS |
| **EDB BDR** | No | Yes | Full | Global active-active |

### 4. Cloud Managed Services
Fully managed PostgreSQL offerings:

| Solution | Provider | Failover Time | Max Replicas | Best For |
|----------|----------|---------------|--------------|----------|
| **Aurora PostgreSQL** | AWS | 15-30s | 15 | AWS-native apps |
| **Cloud SQL** | Google | 60-120s | 10 | GCP-native apps |
| **AlloyDB** | Google | <60s | Multiple | Analytics + OLTP |
| **Azure Flexible** | Microsoft | 60-120s | 10 | Azure-native apps |
| **RDS Multi-AZ** | AWS | 60-120s | 5 | Simple managed HA |

### 5. Serverless / Modern PostgreSQL
Next-generation PostgreSQL platforms:

| Solution | Scale to Zero | Branching | Best For |
|----------|---------------|-----------|----------|
| **Neon** | Yes | Yes (instant) | Dev/test, variable workloads |
| **Supabase** | Limited | No | BaaS, rapid development |

### 6. NewSQL / Distributed SQL
PostgreSQL wire-compatible distributed databases:

| Solution | PostgreSQL Compat | Consistency | Write Scaling | Best For |
|----------|-------------------|-------------|---------------|----------|
| **CockroachDB** | Partial | Serializable | Yes | Global consistency |
| **YugabyteDB** | High | Configurable | Yes | PostgreSQL migration |

## Comparison Matrix

### Failover Characteristics

| Solution | Auto Failover | Typical Time | Data Loss Risk | Split Brain Prevention |
|----------|---------------|--------------|----------------|----------------------|
| Patroni | Yes | 30-60s | Low (configurable) | DCS consensus |
| Stolon | Yes | 30-60s | Low | DCS consensus |
| repmgr | Yes | 60s+ | Medium | Witness nodes |
| PAF/Pacemaker | Yes | 15-30s | Low | Quorum + STONITH |
| pg_auto_failover | Yes | 15-25s | Low | Monitor node |
| Pgpool-II | Yes | 30s | Medium | Watchdog |
| Aurora | Yes | 15-30s | Zero | AWS managed |
| Neon | Yes | 5-10s | Zero | Safekeeper consensus |
| CockroachDB | Yes | 1-5s | Zero | Raft consensus |
| YugabyteDB | Yes | 2-5s | Zero | Raft consensus |

### Scalability

| Solution | Read Scaling | Write Scaling | Max Size | Sharding |
|----------|--------------|---------------|----------|----------|
| Patroni | Replicas | Single node | Node limit | No |
| Stolon | Replicas | Single node | Node limit | No |
| repmgr | Replicas | Single node | Node limit | No |
| Pgpool-II | Load balance | Single node | Node limit | No |
| Citus | Replicas | Horizontal | Unlimited | Yes (hash) |
| EDB BDR | Multi-master | Multi-master | Unlimited | No |
| Aurora | 15 replicas | Single writer | 128TB | No |
| Neon | Branch | Single writer | Unlimited | No |
| CockroachDB | Follower reads | Horizontal | Unlimited | Yes (range) |
| YugabyteDB | Read replicas | Horizontal | Unlimited | Yes (hash/range) |

### Operational Complexity

| Solution | Setup Complexity | Day-2 Operations | Monitoring | Expertise Required |
|----------|------------------|------------------|------------|-------------------|
| Patroni | Medium | Medium | Good (API) | Distributed systems |
| Stolon | Medium | Medium | Limited | Kubernetes |
| repmgr | Low | Medium | Events | PostgreSQL |
| PAF/Pacemaker | High | High | Complex | Linux HA |
| pg_auto_failover | Low | Low | FSM states | Basic |
| Pgpool-II | High | High | Stats page | Middleware |
| Aurora | Low | Low | CloudWatch | AWS |
| Neon | Low | Low | Dashboard | Basic |
| CockroachDB | Medium | Medium | Built-in UI | Distributed systems |
| YugabyteDB | Medium | Medium | Built-in UI | Distributed systems |

## Decision Framework

### Choose Patroni if:
- ✅ You want the most battle-tested open-source HA solution
- ✅ You're deploying on Kubernetes
- ✅ Your team has distributed systems experience
- ✅ You need extensive customization options
- ❌ Avoid if you can't manage an etcd/Consul cluster

### Choose pg_auto_failover if:
- ✅ You want simple HA without external DCS
- ✅ You have a 2-node primary/secondary setup
- ✅ You're using Microsoft/Citus ecosystem
- ❌ Avoid if you need more than 2-3 nodes typically

### Choose Aurora if:
- ✅ You're AWS-centric and want fully managed
- ✅ You need fast failover (<30s)
- ✅ You want easy read scaling (up to 15 replicas)
- ❌ Avoid if you need multi-cloud or cost optimization

### Choose Neon if:
- ✅ You want serverless with scale-to-zero
- ✅ You need instant database branching
- ✅ Your workload is variable/unpredictable
- ❌ Avoid if you can't tolerate 2-5s cold starts

### Choose Citus if:
- ✅ You're building multi-tenant SaaS
- ✅ You need horizontal write scaling
- ✅ Your data has a clear distribution key
- ❌ Avoid if you have heavy cross-tenant queries

### Choose CockroachDB/YugabyteDB if:
- ✅ You need global distribution with consistency
- ✅ You need horizontal write scaling
- ✅ You can accept partial PostgreSQL compatibility
- ❌ Avoid if you need full PostgreSQL feature set

### Choose EDB BDR if:
- ✅ You need true active-active multi-master
- ✅ You can afford commercial licensing
- ✅ You need global deployment with local latency
- ❌ Avoid if you're budget-constrained

## Quick Reference: Failure Scenarios

### Primary Node Crashes

| Solution | Detection Time | Promotion Time | Client Impact |
|----------|---------------|----------------|---------------|
| Patroni | TTL expiry (30s) | ~5s | Reconnect required |
| pg_auto_failover | Health check (10-15s) | ~5s | Reconnect required |
| Aurora | Instant | ~5-10s | DNS update, retry |
| CockroachDB | Raft timeout (~1s) | ~1s | Automatic |

### Network Partition

| Solution | Behavior | Split Brain Risk |
|----------|----------|------------------|
| Patroni | Minority loses DCS, demotes | None (DCS quorum) |
| repmgr | Depends on witness | Requires witness nodes |
| Aurora | AWS handles | None (AWS managed) |
| CockroachDB | Minority read-only | None (Raft majority) |

### Storage Failure

| Solution | Behavior | Data Safety |
|----------|----------|-------------|
| Patroni | Node fails, failover | Depends on sync mode |
| Aurora | Transparent (6x replication) | Safe (4/6 quorum) |
| Neon | Transparent (S3 backed) | Safe (safekeeper + S3) |
| CockroachDB | Transparent (Raft) | Safe (majority) |

## Cost Considerations

### Open Source (Infrastructure Costs Only)
- Patroni, Stolon, repmgr, pg_auto_failover, Pgpool-II
- Cost: 3+ nodes for HA + DCS nodes (if required)

### Cloud Managed (Service + Storage)
- Aurora: Instance hours + storage + I/O
- Cloud SQL: Instance hours + storage
- AlloyDB: Instance hours + storage

### Serverless (Pay-per-Use)
- Neon: Compute hours + storage
- Aurora Serverless v2: ACU-seconds + storage

### Commercial License
- EDB BDR: Enterprise licensing
- CockroachDB: Enterprise features
- YugabyteDB: Yugabyte Platform features

## Migration Paths

### From Single PostgreSQL to HA

```
Simplest path:
1. pg_auto_failover (add monitor + secondary)
2. Or: repmgr (add standby + daemon)

Enterprise path:
1. Patroni + etcd cluster
2. Or: Aurora (if AWS)
```

### From PostgreSQL to Distributed

```
With PostgreSQL compatibility priority:
1. YugabyteDB (higher compatibility)
2. Or: Citus (if multi-tenant)

With consistency priority:
1. CockroachDB (serializable)
2. Or: EDB BDR (true PostgreSQL)
```

### From On-Prem to Cloud

```
Lift and shift:
1. RDS PostgreSQL (minimal changes)
2. Or: Aurora (AWS native)

Modernization:
1. Neon (serverless)
2. Or: CockroachDB Cloud
```

## Conclusion

The PostgreSQL HA landscape offers solutions for every need:

- **Simple HA**: pg_auto_failover, repmgr
- **Production HA**: Patroni, Stolon
- **Enterprise HA**: PAF/Pacemaker, EDB BDR
- **Cloud Managed**: Aurora, Cloud SQL, AlloyDB
- **Serverless**: Neon, Aurora Serverless
- **Distributed Scale**: Citus, CockroachDB, YugabyteDB

The best choice depends on:
1. **Scale requirements**: Single-node vs distributed
2. **Consistency needs**: Strong vs eventual
3. **Operational capacity**: Self-managed vs fully managed
4. **Budget**: Open source vs commercial
5. **Environment**: On-prem vs cloud vs hybrid
6. **PostgreSQL compatibility**: Full vs partial

Start with the simplest solution that meets your requirements, and evolve as needs grow.
