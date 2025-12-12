# PostgreSQL High Availability Template for Railway

This template deploys a production-ready, highly-available PostgreSQL cluster on Railway with automatic failover, read replicas, and transparent connection routing.

## Features

- **3-node PostgreSQL cluster** with streaming replication
- **Automatic failover** in <10 seconds using Patroni
- **2 standby replicas** for automatic failover and read scaling
- **HAProxy load balancer** with separate read-write and read-only endpoints
- **Transparent routing** - single endpoint for all database connections
- **Automatic recovery** - failed nodes rejoin as replicas
- **Health monitoring** - Patroni REST API for cluster status
- **Multi-version support** - PostgreSQL 13, 14, 15, 16, and 17

## Architecture

```
Application
    ↓
HAProxy ← Load balanced endpoint
    ├─ :5432 (read-write) → Primary only
    └─ :5433 (read-only)  → Replicas (round-robin)
    ↓
PostgreSQL Cluster
    ├─ postgres-1 (Leader)  ← Writes + Reads
    ├─ postgres-2 (Standby) ← Reads + Failover ready
    └─ postgres-3 (Standby) ← Reads + Failover ready
         ↓
    etcd (3 nodes) ← Distributed consensus
```

## Services Deployed

1. **postgres-1, postgres-2, postgres-3** - PostgreSQL with Patroni orchestration
2. **etcd-1, etcd-2, etcd-3** - Distributed key-value store for leader election
3. **haproxy** - Load balancer with automatic primary detection

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

### Connection Strings

Once deployed, connect to your database via HAProxy:

```bash
# Primary (read-write) - From Railway private network
postgresql://railway:${POSTGRES_PASSWORD}@haproxy.railway.internal:5432/railway

# Replicas (read-only) - For read scaling
postgresql://railway:${POSTGRES_PASSWORD}@haproxy.railway.internal:5433/railway

# From external (via TCP proxy)
postgresql://railway:${POSTGRES_PASSWORD}@${HAPROXY_TCP_PROXY_DOMAIN}/railway
```

HAProxy automatically routes connections:
- **Port 5432**: Routes to current Patroni leader (read-write)
- **Port 5433**: Load-balances across healthy replicas (read-only)

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
| `POSTGRES_NODES` | (required for HAProxy) | Node list in format `host:pgport:patroniport,...` |
| `HAPROXY_MAX_CONN` | `1000` | Maximum concurrent connections |
| `HAPROXY_CHECK_INTERVAL` | `3s` | Backend health check interval |

### Scaling HAProxy

HAProxy is stateless and can be scaled horizontally via Railway replicas:

```toml
# In haproxy/railway.toml
[deploy]
numReplicas = 3
```

## Monitoring

### Health Checks

Each service exposes health endpoints:

```bash
# PostgreSQL + Patroni
curl http://postgres-1.railway.internal:8008/health
curl http://postgres-1.railway.internal:8008/cluster  # Full cluster status
curl http://postgres-1.railway.internal:8008/primary  # 200 if primary
curl http://postgres-1.railway.internal:8008/replica  # 200 if replica

# etcd
curl http://etcd-1.railway.internal:2379/health

# HAProxy stats dashboard
curl http://haproxy.railway.internal:8404/stats
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

### HAProxy Stats

Access the HAProxy stats dashboard at `http://haproxy.railway.internal:8404/stats` for real-time backend health and connection metrics.

## Failover Behavior

### Automatic Failover (Primary Crashes)

**Timeline**:
```
T+0s   postgres-1 (leader) crashes
T+3s   HAProxy health check fails (3s interval)
T+6s   HAProxy marks backend DOWN (fall 3)
T+8s   Patroni elects new leader via etcd
T+10s  HAProxy routes to new primary
```

**Impact**:
- Existing write connections: Dropped (apps retry)
- Existing read connections: Unaffected (if using :5433)
- New connections: Routed to new primary
- **Total downtime**: ~10 seconds

### Automatic Recovery (Failed Node Returns)

When `postgres-1` recovers:

```
T+0s   postgres-1 restarts
T+3s   Patroni registers with etcd
T+4s   Discovers postgres-2 is leader
T+5s   Rejoins as replica
T+10s  Begins streaming replication
T+12s  HAProxy adds to replica pool
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
docker-compose up -d
```

This starts all 7 services on your local machine.

Connect to the cluster:
```bash
# Read-write (primary)
psql postgresql://railway:railway@localhost:5432/railway

# Read-only (replicas)
psql postgresql://railway:railway@localhost:5433/railway
```

View HAProxy stats:
```bash
open http://localhost:8404/stats
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
- Increase `wal_keep_size` in patroni configuration
- Consider increasing PostgreSQL resources

### HAProxy connection errors

1. Check HAProxy stats:
   ```bash
   curl http://haproxy.railway.internal:8404/stats
   ```

2. Verify backends are healthy:
   ```bash
   curl http://postgres-1.railway.internal:8008/health
   ```

3. Check HAProxy logs for backend failures

## Cost Estimation

**Resource allocation**:
- 3 PostgreSQL: 2 vCPU, 2GB RAM each + 10GB volume
- 3 etcd: 0.5 vCPU, 512MB RAM each
- 1 HAProxy: 0.5 vCPU, 512MB RAM

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
pg_dump -h haproxy.railway.internal -U railway railway > backup.sql
```

**Restore from snapshot**:
1. Railway Dashboard → postgres-1 → Volumes → Snapshots
2. Click "Restore" on desired snapshot
3. Create new service from snapshot

## Upgrading PostgreSQL

This template supports PostgreSQL 13, 14, 15, 16, and 17. To upgrade:

1. Create a logical backup:
   ```bash
   pg_dumpall -h haproxy.railway.internal -U railway > cluster_backup.sql
   ```

2. Deploy new template with updated PostgreSQL version

3. Restore data:
   ```bash
   psql -h new-haproxy.railway.internal -U railway < cluster_backup.sql
   ```

4. Update application `DATABASE_URL` to new cluster

5. Delete old cluster after verification

## Security

- All passwords are auto-generated and encrypted at rest
- Private networking isolates cluster from public internet
- SSL enabled by default for PostgreSQL connections
- Recommend enabling Railway's 2FA for project access

## Performance Tuning

### PostgreSQL

Patroni dynamically generates PostgreSQL configuration. Override via environment variables:

```yaml
postgresql:
  parameters:
    shared_buffers: 512MB        # 25% of RAM
    effective_cache_size: 2GB    # 50-75% of RAM
    max_connections: 300         # Increase for high concurrency
    work_mem: 4MB                # Per-query memory
```

### HAProxy

Adjust via environment variables:

```bash
HAPROXY_MAX_CONN=2000           # More concurrent connections
HAPROXY_CHECK_INTERVAL=1s       # Faster failover detection
HAPROXY_TIMEOUT_CLIENT=60m      # Longer idle connections
```

## Docker Images

Pre-built images are published to GitHub Container Registry:

```bash
# PostgreSQL + Patroni (multiple versions)
ghcr.io/railwayapp/postgres-ha/postgres-patroni:17
ghcr.io/railwayapp/postgres-ha/postgres-patroni:16
ghcr.io/railwayapp/postgres-ha/postgres-patroni:15
ghcr.io/railwayapp/postgres-ha/postgres-patroni:14
ghcr.io/railwayapp/postgres-ha/postgres-patroni:13

# etcd
ghcr.io/railwayapp/postgres-ha/etcd:3.5.16

# HAProxy
ghcr.io/railwayapp/postgres-ha/haproxy:3.2
```

## Support

- **Issues**: https://github.com/railwayapp/postgres-ha/issues
- **Documentation**: https://docs.railway.app/
- **Community**: https://discord.gg/railway

## License

MIT License - See LICENSE file for details

## Credits

Built with:
- [PostgreSQL](https://www.postgresql.org/) - World's most advanced open source database
- [Patroni](https://patroni.readthedocs.io/) - Template for PostgreSQL HA with Python
- [etcd](https://etcd.io/) - Distributed reliable key-value store
- [HAProxy](https://www.haproxy.org/) - Reliable, high-performance TCP/HTTP load balancer
- [Railway](https://railway.app/) - Infrastructure, instantly
