# PostgreSQL High Availability Template for Railway

This template deploys a production-ready, highly-available PostgreSQL cluster on Railway with automatic failover, read replicas, and transparent connection routing.

## Features

- **3-node PostgreSQL cluster** with streaming replication
- **Automatic failover** in <10 seconds using Patroni
- **2 standby replicas** for automatic failover
- **Connection pooling** with Pgpool-II (3 replicas for HA)
- **Transparent routing** - single endpoint for all database connections
- **Automatic recovery** - failed nodes rejoin as replicas
- **Health monitoring** - Patroni REST API for cluster status

## Architecture

```
Application
    ↓
Pgpool-II (3 replicas) ← Load balanced endpoint
    ↓
PostgreSQL Cluster
    ├─ postgres-1 (Leader)  ← All queries
    ├─ postgres-2 (Standby) ← Failover ready
    └─ postgres-3 (Standby) ← Failover ready
         ↓
    etcd (3 nodes) ← Distributed consensus
```

## Services Deployed

1. **postgres-1, postgres-2, postgres-3** - PostgreSQL 17 with Patroni orchestration
2. **etcd-1, etcd-2, etcd-3** - Distributed key-value store for leader election
3. **pgpool** - Connection pooler and query router with built-in failover watcher

**Total**: 7 services

## Quick Start

### Deploy to Railway

1. Click "Deploy Template" in Railway marketplace
2. Configure variables (or use defaults):
   - `POSTGRES_USER` - Database username (default: `railway`)
   - `POSTGRES_PASSWORD` - Auto-generated secure password
   - `POSTGRES_DB` - Database name (default: `railway`)
3. Wait for deployment (~2-3 minutes)
4. Connect using the provided `DATABASE_URL`

### Connection String

Once deployed, connect to your database via Pgpool-II:

```bash
# From Railway private network (other services in same project)
postgresql://railway:${POSTGRES_PASSWORD}@pgpool.railway.internal:5432/railway

# From external (via TCP proxy)
postgresql://railway:${POSTGRES_PASSWORD}@${PGPOOL_TCP_PROXY_DOMAIN}/railway
```

Pgpool-II automatically routes all queries to the current primary node. The `patroni-watcher` process monitors the Patroni cluster and updates Pgpool's backend configuration when leadership changes, ensuring transparent failover.

## Configuration

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `POSTGRES_USER` | `railway` | Database superuser |
| `POSTGRES_PASSWORD` | Auto-generated | Superuser password |
| `POSTGRES_DB` | `railway` | Default database name |
| `PATRONI_SCOPE` | `pg-ha-cluster` | Cluster identifier |
| `PATRONI_TTL` | `30` | Leader lease TTL (seconds) |
| `PATRONI_LOOP_WAIT` | `10` | Health check interval |
| `PGPOOL_NUM_INIT_CHILDREN` | `32` | Connection pool workers per Pgpool instance |
| `PGPOOL_MAX_POOL` | `4` | Cached connections per worker |

### Scaling Pgpool-II

Pgpool-II is stateless and can be scaled horizontally:

```toml
# In pgpool/railway.toml
[deploy]
numReplicas = 5  # Scale to 5 instances
```

**Connection capacity**: `numReplicas × num_init_children × max_pool`
- Default: `3 × 32 × 4 = 384 connections`
- With 10 replicas: `10 × 32 × 4 = 1,280 connections`

## Monitoring

### Health Checks

Each service exposes health endpoints:

```bash
# PostgreSQL + Patroni
curl http://postgres-1.railway.internal:8008/health
curl http://postgres-1.railway.internal:8008/cluster  # Full cluster status

# etcd
curl http://etcd-1.railway.internal:2379/health

# Pgpool-II
pg_isready -h pgpool.railway.internal -p 5432
```

### Cluster Status

Check Patroni cluster status from any PostgreSQL node:

```bash
curl http://postgres-1.railway.internal:8008/cluster
```

Response:
```json
{
  "members": [
    {
      "name": "postgres-1",
      "role": "leader",
      "state": "running",
      "timeline": 1,
      "lag": 0
    },
    {
      "name": "postgres-2",
      "role": "replica",
      "state": "streaming",
      "timeline": 1,
      "lag": 0
    },
    {
      "name": "postgres-3",
      "role": "replica",
      "state": "streaming",
      "timeline": 1,
      "lag": 0
    }
  ]
}
```

## Failover Behavior

### Automatic Failover (Primary Crashes)

**Timeline**:
```
T+0s   postgres-1 (leader) crashes
T+2s   Patroni detects failure via etcd
T+4s   postgres-2 elected as new leader
T+6s   Pgpool-II detects new primary
T+10s  Failover complete
```

**Impact**:
- Existing write connections: Dropped (apps retry)
- Existing read connections: Unaffected
- New connections: Routed to new primary
- **Total downtime**: ~5-10 seconds

### Automatic Recovery (Failed Node Returns)

When `postgres-1` recovers:

```
T+0s   postgres-1 restarts
T+3s   Patroni registers with etcd
T+4s   Discovers postgres-2 is leader
T+5s   Rejoins as replica
T+10s  Begins streaming replication
```

**Result**: Original primary rejoins as **replica**, not leader.

### Manual Switchover

To manually switch leaders (zero-downtime):

```bash
# From inside any PostgreSQL container
patronictl -c /etc/patroni/patroni.yml switchover

# Follow prompts to select new leader
```

## Local Development

Test the cluster locally with Docker Compose:

```bash
cd templates/postgres-ha
docker-compose up -d
```

This starts all services on your local machine.

Connect to the cluster:
```bash
psql postgresql://railway:railway@localhost:5432/railway
```

## Troubleshooting

### Cluster won't start

1. Check etcd is healthy:
   ```bash
   curl http://etcd-1.railway.internal:2379/health
   ```

2. Check Patroni logs for PostgreSQL services:
   ```
   Railway Dashboard → postgres-1 → Logs
   ```

3. Verify private networking is enabled in Railway project settings

### Split-brain (multiple leaders)

This should never happen due to etcd quorum, but if it does:

1. Stop all PostgreSQL services
2. Clear etcd data: `etcdctl del --prefix /service/`
3. Restart services in order: etcd → postgres-1 → postgres-2 → postgres-3

### High replication lag

Check lag from Patroni API:
```bash
curl http://postgres-2.railway.internal:8008/ | jq '.replication[0].lag'
```

If lag is high (>1GB):
- Check network connectivity between nodes
- Increase `wal_keep_size` in patroni.yml
- Consider increasing PostgreSQL resources

### Pgpool connection errors

1. Check backends status:
   ```bash
   psql -h pgpool.railway.internal -U postgres -c "SHOW POOL_NODES;"
   ```

2. Verify passwords are correct (must match across all services)

3. Check Pgpool logs for authentication errors

## Cost Estimation

**Resource allocation**:
- 3 PostgreSQL: 2 vCPU, 2GB RAM each + 10GB volume
- 3 etcd: 0.5 vCPU, 512MB RAM each
- 1 Pgpool: 0.5 vCPU, 512MB RAM

**Estimated cost (Railway Pro)**:
- Compute: ~$60-120/month
- Storage: ~$7.50/month (30GB)
- **Total**: ~$70-130/month

**Comparison**:
- AWS RDS Multi-AZ (db.t4g.small): ~$120/month
- GCP Cloud SQL HA: ~$80/month

## Backups

Railway Pro includes automatic volume snapshots:
- Frequency: Daily
- Retention: 6 days (daily), 27 days (weekly), 89 days (monthly)
- Max 10 backups per volume

**Manual backup**:
```bash
pg_dump -h pgpool.railway.internal -U railway railway > backup.sql
```

**Restore from snapshot**:
1. Railway Dashboard → postgres-1 → Volumes → Snapshots
2. Click "Restore" on desired snapshot
3. Create new service from snapshot

## Upgrading PostgreSQL

To upgrade from PostgreSQL 17 to a newer version:

1. Create a logical backup:
   ```bash
   pg_dumpall -h pgpool.railway.internal -U railway > cluster_backup.sql
   ```

2. Deploy new template with updated PostgreSQL version

3. Restore data:
   ```bash
   psql -h new-pgpool.railway.internal -U railway < cluster_backup.sql
   ```

4. Update application `DATABASE_URL` to new cluster

5. Delete old cluster after verification

## Security

- All passwords are auto-generated and encrypted at rest
- Private networking isolates cluster from public internet
- mTLS available for client connections (configure in Pgpool)
- Recommend enabling Railway's 2FA for project access

## Performance Tuning

### PostgreSQL

Edit `patroni.yml` parameters:
```yaml
postgresql:
  parameters:
    shared_buffers: 512MB        # 25% of RAM
    effective_cache_size: 2GB    # 50-75% of RAM
    max_connections: 300         # Increase for high concurrency
    work_mem: 4MB                # Per-query memory
```

### Pgpool-II

Edit `pgpool.conf`:
```conf
num_init_children = 64           # More connection workers
max_pool = 8                     # More cached connections per worker
```

Redeploy services after configuration changes.

## Support

- **Issues**: https://github.com/railwayapp/examples/issues
- **Documentation**: https://docs.railway.app/
- **Community**: https://discord.gg/railway

## License

MIT License - See LICENSE file for details

## Credits

Built with:
- [PostgreSQL](https://www.postgresql.org/) - World's most advanced open source database
- [Patroni](https://patroni.readthedocs.io/) - Template for PostgreSQL HA with Python
- [etcd](https://etcd.io/) - Distributed reliable key-value store
- [Pgpool-II](https://www.pgpool.net/) - Middleware for PostgreSQL clustering
- [Railway](https://railway.app/) - Infrastructure, instantly
