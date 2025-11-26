# PostgreSQL HA Solutions Comparison Table

## Quick Reference

| Solution | Type | PostgreSQL | Failover Time | Multi-Master | Managed |
|----------|------|------------|---------------|--------------|---------|
| **Patroni** | HA Manager | Native | 30-60s | No | No |
| **repmgr** | HA Manager | Native | 60s+ | No | No |
| **Pgpool-II** | Middleware | Native | 30s | No | No |
| **RDS Multi-AZ** | Cloud Managed | Native | 60-120s | No | Yes |
| **Cloud SQL** | Cloud Managed | Native | ~60s | No | Yes |
| **Aurora** | Cloud Managed | Compatible | 15-30s | No | Yes |
| **Vitess/PlanetScale** | Distributed | MySQL Only | 15-20s | No | Yes |
| **Neon** | Serverless | Native | 5-10s | No | Yes |

---

## Detailed Comparison

### Architecture & Design

| Aspect | Patroni | repmgr | Pgpool-II | RDS Multi-AZ | Cloud SQL | Aurora | Vitess | Neon |
|--------|---------|--------|-----------|--------------|-----------|--------|--------|------|
| **Architecture** | Agent per node | Agent per node | Proxy middleware | Managed instances | Managed instances | Shared storage | Sharded MySQL | Separated compute/storage |
| **Database** | PostgreSQL | PostgreSQL | PostgreSQL | PostgreSQL | PostgreSQL | PostgreSQL-compat | MySQL | PostgreSQL |
| **Replication** | Streaming | Streaming | Streaming | Streaming | Streaming | Storage-level | Binlog | WAL + Storage |
| **External DCS** | Yes (etcd/Consul) | No | No (Watchdog) | No (AWS) | No (GCP) | No (AWS) | Yes (etcd/ZK) | No |
| **Proxy Included** | No (use HAProxy) | No | Yes | No | No | No | Yes (VTGate) | Yes |

### High Availability

| Aspect | Patroni | repmgr | Pgpool-II | RDS Multi-AZ | Cloud SQL | Aurora | Vitess | Neon |
|--------|---------|--------|-----------|--------------|-----------|--------|--------|------|
| **Failover Time** | 30-60s | 60s+ | 30s | 60-120s | ~60s | 15-30s | 15-20s | 5-10s |
| **Automatic Failover** | Yes | Yes | Yes (script) | Yes | Yes | Yes | Yes | Yes |
| **Data Loss Risk** | Configurable | Medium | Medium | Zero | Zero | Zero | Low | Zero |
| **Split Brain Prevention** | DCS consensus | Witness nodes | Watchdog | AWS managed | GCP managed | AWS managed | Raft | Safekeeper consensus |
| **Standby Readable** | Via HAProxy | Via HAProxy | Yes | No | No | Yes (replicas) | Yes (replicas) | Yes (branches) |
| **Sync Replication** | Optional | Optional | Optional | Always | Always | Storage-level | Semi-sync | Safekeeper quorum |

### Scalability

| Aspect | Patroni | repmgr | Pgpool-II | RDS Multi-AZ | Cloud SQL | Aurora | Vitess | Neon |
|--------|---------|--------|-----------|--------------|-----------|--------|--------|------|
| **Read Scaling** | Add replicas | Add replicas | Load balance | Separate replicas | Separate replicas | Up to 15 replicas | Shards + replicas | Branches |
| **Write Scaling** | Single node | Single node | Single node | Single node | Single node | Single node | Horizontal (shards) | Single node |
| **Max Read Replicas** | Unlimited | Unlimited | Unlimited | 5 | 10 | 15 | Unlimited | Unlimited branches |
| **Sharding** | No | No | No | No | No | No | Yes (built-in) | No |
| **Auto-scaling** | No | No | No | No | No | Serverless v2 | Yes | Yes |
| **Scale to Zero** | No | No | No | No | No | Serverless v2 | No | Yes |

### Operations & Management

| Aspect | Patroni | repmgr | Pgpool-II | RDS Multi-AZ | Cloud SQL | Aurora | Vitess | Neon |
|--------|---------|--------|-----------|--------------|-----------|--------|--------|------|
| **Setup Complexity** | Medium | Low | High | Low | Low | Low | High | Low |
| **Operational Burden** | Medium | Medium | High | Low | Low | Low | High | Low |
| **Managed Service** | No | No | No | Yes | Yes | Yes | PlanetScale | Yes |
| **OS Access** | Yes | Yes | Yes | No | No | No | Self-hosted: Yes | No |
| **Custom Extensions** | All | All | All | Most | Most | Most | N/A (MySQL) | Most |
| **Online DDL** | Standard PG | Standard PG | Standard PG | Standard PG | Standard PG | Standard PG | Zero-downtime | Standard PG |
| **Backups** | Manual/pgBackRest | Manual | Manual | Automated | Automated | Automated | Manual | Automated |
| **PITR** | Manual setup | Manual setup | Manual setup | Built-in | Built-in | Built-in | Manual | Built-in |

### Features

| Feature | Patroni | repmgr | Pgpool-II | RDS Multi-AZ | Cloud SQL | Aurora | Vitess | Neon |
|---------|---------|--------|-----------|--------------|-----------|--------|--------|------|
| **Connection Pooling** | External | External | Built-in | External | External | External | VTGate | Built-in |
| **Load Balancing** | Via HAProxy | External | Built-in | External | External | Reader endpoint | VTGate | N/A |
| **Query Caching** | No | No | Yes | No | No | No | No | No |
| **Read/Write Split** | Via HAProxy | External | Built-in | Manual | Manual | Endpoints | Automatic | Manual |
| **Database Branching** | No | No | No | No | No | No | Yes | Yes |
| **Multi-Region** | Manual | Manual | Manual | Cross-region replica | Cross-region replica | Global Database | Yes | Limited |
| **REST API** | Yes | No | No | AWS API | GCP API | AWS API | Yes | Yes |

### Cost & Licensing

| Aspect | Patroni | repmgr | Pgpool-II | RDS Multi-AZ | Cloud SQL | Aurora | Vitess | Neon |
|--------|---------|--------|-----------|--------------|-----------|--------|--------|------|
| **License** | MIT | GPL | BSD | Proprietary | Proprietary | Proprietary | Apache 2.0 | Proprietary |
| **Software Cost** | Free | Free | Free | N/A | N/A | N/A | Free | N/A |
| **Infrastructure** | Self-managed | Self-managed | Self-managed | AWS managed | GCP managed | AWS managed | Self/PlanetScale | Neon managed |
| **Pricing Model** | Infrastructure | Infrastructure | Infrastructure | Instance hours | Instance hours | Instance + I/O | Instance/Usage | Compute + Storage |
| **Standby Cost** | Full instance | Full instance | Full instance | Full instance | Full instance | Included | Per shard | Minimal |
| **HA Premium** | 2x+ infra | 2x+ infra | 2x+ infra | 2x | 2x | ~1.2x (shared storage) | Per shard | Usage-based |

### Failure Scenarios

| Scenario | Patroni | repmgr | Pgpool-II | RDS Multi-AZ | Cloud SQL | Aurora | Vitess | Neon |
|----------|---------|--------|-----------|--------------|-----------|--------|--------|------|
| **Primary Crash** | Auto-failover 30-60s | Auto-failover 60s+ | Script failover 30s | Auto-failover 60-120s | Auto-failover ~60s | Auto-failover 15-30s | Per-shard failover 15-20s | Auto-failover 5-10s |
| **Network Partition** | DCS quorum decides | Witness needed | Watchdog quorum | AWS handles | GCP handles | AWS handles | Raft per shard | Safekeeper quorum |
| **Storage Failure** | Instance fails | Instance fails | Instance fails | EBS redundancy | PD redundancy | 6-way storage | Per-shard storage | S3 backed |
| **Zone Failure** | Failover to other zone | Failover to other zone | Failover to other zone | Failover to standby | Failover to standby | Failover to replica | Per-shard failover | Transparent |
| **Region Failure** | Manual DR | Manual DR | Manual DR | Promote cross-region replica | Promote cross-region replica | Promote secondary region | Cross-region shards | Manual |

---

## Decision Matrix

### When to Choose Each Solution

| If you need... | Best Choice | Runner-up |
|----------------|-------------|-----------|
| **Simplest open-source HA** | repmgr | pg_auto_failover |
| **Battle-tested open-source** | Patroni | Stolon |
| **Kubernetes deployment** | Patroni | Stolon |
| **Connection pooling + HA** | Pgpool-II | Patroni + PgBouncer |
| **AWS managed with low ops** | RDS Multi-AZ | Aurora |
| **AWS with fastest failover** | Aurora | RDS Multi-AZ |
| **GCP managed** | Cloud SQL | AlloyDB |
| **Horizontal write scaling** | Vitess/PlanetScale | Citus |
| **Serverless with branching** | Neon | Aurora Serverless |
| **Scale to zero** | Neon | Aurora Serverless v2 |
| **Lowest failover time** | Neon (5-10s) | Aurora (15-30s) |
| **Zero data loss guarantee** | Aurora/RDS/Cloud SQL | Patroni (sync mode) |
| **Multi-region active-active** | Vitess | EDB BDR |
| **Full PostgreSQL compatibility** | Patroni/repmgr | RDS/Cloud SQL |
| **Lowest cost** | Patroni/repmgr | Cloud SQL |
| **Development/testing** | Neon | Local Patroni |

---

## Summary Comparison Chart

```
                    Failover Speed
                         ▲
                         │
            Neon ●       │
         (5-10s) │       │         ● Vitess
                 │       │           (15-20s)
      Aurora ●   │       │
      (15-30s)   │       │
                 │       │
                 │       │         ● Patroni
                 │       │           (30-60s)
                 │       │    ● Pgpool-II
    Cloud SQL ●  │       │      (30s)
        (~60s)   │       │
                 │       │              ● repmgr
   RDS Multi-AZ ●│       │                (60s+)
      (60-120s)  │       │
                 │       │
                 └───────┴────────────────────────────────────►
                 Managed                              Self-Managed
                         Operational Complexity
```

```
                    Write Scaling
                         ▲
                         │
                         │         ● Vitess
                         │           (horizontal)
                         │
                         │
    ─────────────────────┼─────────────────────────────────────
    All others:          │
    Single-writer        │
    (vertical scaling    │
     only)               │
                         │
                         └────────────────────────────────────►
                                   Read Scaling
                              (All support read replicas)
```

---

## Recommendation Summary

| Workload Type | Recommended | Why |
|---------------|-------------|-----|
| **Small startup, AWS** | RDS Multi-AZ | Simple, managed, cost-effective |
| **Small startup, GCP** | Cloud SQL | Simple, managed, integrated |
| **Growing SaaS, AWS** | Aurora | Fast failover, read scaling |
| **Enterprise, on-prem** | Patroni | Flexible, battle-tested |
| **Legacy migration** | repmgr | Simplest upgrade path |
| **Variable workload** | Neon | Scale to zero, branching |
| **Global scale MySQL** | PlanetScale | Horizontal scaling, zero-downtime DDL |
| **Dev/Test environments** | Neon | Instant branching, free tier |
| **Connection pooling focus** | Pgpool-II + Patroni | Best of both worlds |
