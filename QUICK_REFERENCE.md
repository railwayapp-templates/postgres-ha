# Quick Reference: Service Configuration

Use this as a checklist when deploying each service.

## Service 1-3: etcd Cluster

| Setting | etcd-1 | etcd-2 | etcd-3 |
|---------|--------|--------|--------|
| **Name** | `etcd-1` | `etcd-2` | `etcd-3` |
| **Root Directory** | `templates/postgres-ha/etcd-1` | `templates/postgres-ha/etcd-2` | `templates/postgres-ha/etcd-3` |
| **Build** | Dockerfile | Dockerfile | Dockerfile |
| **Start Command** | `/usr/local/bin/etcd` | `/usr/local/bin/etcd` | `/usr/local/bin/etcd` |
| **Replicas** | 1 | 1 | 1 |
| **Volume** | None | None | None |

### Variables (per service)

**etcd-1**:
```bash
ETCD_NAME=etcd-1
ETCD_ADVERTISE_CLIENT_URLS=http://etcd-1.railway.internal:2379
ETCD_INITIAL_ADVERTISE_PEER_URLS=http://etcd-1.railway.internal:2380
```

**etcd-2**:
```bash
ETCD_NAME=etcd-2
ETCD_ADVERTISE_CLIENT_URLS=http://etcd-2.railway.internal:2379
ETCD_INITIAL_ADVERTISE_PEER_URLS=http://etcd-2.railway.internal:2380
```

**etcd-3**:
```bash
ETCD_NAME=etcd-3
ETCD_ADVERTISE_CLIENT_URLS=http://etcd-3.railway.internal:2379
ETCD_INITIAL_ADVERTISE_PEER_URLS=http://etcd-3.railway.internal:2380
```

**All three share**:
```bash
ETCD_INITIAL_CLUSTER=etcd-1=http://etcd-1.railway.internal:2380,etcd-2=http://etcd-2.railway.internal:2380,etcd-3=http://etcd-3.railway.internal:2380
ETCD_INITIAL_CLUSTER_STATE=new
ETCD_INITIAL_CLUSTER_TOKEN=railway-pg-ha
ETCD_LISTEN_CLIENT_URLS=http://0.0.0.0:2379
ETCD_LISTEN_PEER_URLS=http://0.0.0.0:2380
ETCD_DATA_DIR=/etcd-data
```

---

## Service 4-6: PostgreSQL Cluster

| Setting | postgres-1 | postgres-2 | postgres-3 |
|---------|------------|------------|------------|
| **Name** | `postgres-1` | `postgres-2` | `postgres-3` |
| **Root Directory** | `templates/postgres-ha/postgres-patroni` | Same | Same |
| **Build** | Dockerfile | Dockerfile | Dockerfile |
| **Start Command** | `patroni /etc/patroni/patroni.yml` | Same | Same |
| **Replicas** | 1 | 1 | 1 |
| **Volume** | `/var/lib/postgresql/data` (10GB) | `/var/lib/postgresql/data` (10GB) | `/var/lib/postgresql/data` (10GB) |

### Variables (per service)

**postgres-1**:
```bash
PATRONI_NAME=postgres-1
```

**postgres-2**:
```bash
PATRONI_NAME=postgres-2
```

**postgres-3**:
```bash
PATRONI_NAME=postgres-3
```

**All three share**:
```bash
# Reference shared variables
PATRONI_SCOPE=${{shared.PATRONI_SCOPE}}
PATRONI_ETCD_HOSTS=${{shared.PATRONI_ETCD_HOSTS}}
PATRONI_TTL=${{shared.PATRONI_TTL}}
PATRONI_LOOP_WAIT=${{shared.PATRONI_LOOP_WAIT}}
POSTGRES_USER=${{shared.POSTGRES_USER}}
POSTGRES_PASSWORD=${{shared.POSTGRES_PASSWORD}}
POSTGRES_DB=${{shared.POSTGRES_DB}}
PATRONI_REPLICATION_USERNAME=${{shared.PATRONI_REPLICATION_USERNAME}}
PATRONI_REPLICATION_PASSWORD=${{shared.PATRONI_REPLICATION_PASSWORD}}
PGDATA=/var/lib/postgresql/data
```

---

## Service 7: Pgpool-II

| Setting | Value |
|---------|-------|
| **Name** | `pgpool` |
| **Root Directory** | `templates/postgres-ha/pgpool` |
| **Build** | Dockerfile |
| **Start Command** | `pgpool -n -f /etc/pgpool-II/pgpool.conf -F /etc/pgpool-II/pcp.conf` |
| **Replicas** | **3** (for HA) |
| **Volume** | None |
| **Public Networking** | Enable (for external access) |
| **TCP Proxy** | Port 5432 |

### Variables

```bash
POSTGRES_PASSWORD=${{shared.POSTGRES_PASSWORD}}
REPLICATION_PASSWORD=${{shared.PATRONI_REPLICATION_PASSWORD}}
PGPOOL_NUM_INIT_CHILDREN=32
PGPOOL_MAX_POOL=4
```

---

## Shared Variables (Set BEFORE Deploying)

Go to Project → Variables → Shared:

```bash
POSTGRES_USER=railway
POSTGRES_PASSWORD=<generate-secure-password>
POSTGRES_DB=railway
PATRONI_REPLICATION_USERNAME=replicator
PATRONI_REPLICATION_PASSWORD=<generate-secure-password>
PATRONI_SCOPE=pg-ha-cluster
PATRONI_TTL=30
PATRONI_LOOP_WAIT=10
PATRONI_ETCD_HOSTS=etcd-1.railway.internal:2379,etcd-2.railway.internal:2379,etcd-3.railway.internal:2379

```

---

## Deployment Order

```
1. Set shared variables ✓
2. Deploy etcd-1, etcd-2, etcd-3 → Wait for all healthy
3. Deploy postgres-1, postgres-2, postgres-3 → Wait for all healthy
4. Deploy pgpool → Wait for healthy
5. Test connection ✓
```

---

## Connection Strings

### Private (from other Railway services)
```bash
postgresql://railway:<password>@pgpool.railway.internal:5432/railway
```

### Public (external applications)
```bash
postgresql://railway:<password>@<pgpool-tcp-proxy-domain>/railway
```

Get TCP proxy domain from: pgpool service → Settings → Networking → TCP Proxy

---

## Health Check Commands

```bash
# etcd
curl http://etcd-1.railway.internal:2379/health

# Patroni cluster status
curl http://postgres-1.railway.internal:8008/cluster

# Pgpool status
psql -h pgpool.railway.internal -U railway -c "SHOW POOL_NODES;"

# Replication status
psql -h pgpool.railway.internal -U railway -c "SELECT * FROM pg_stat_replication;"
```

---

## Common Issues

| Issue | Fix |
|-------|-----|
| etcd won't start | Check all 3 are configured with same INITIAL_CLUSTER |
| PostgreSQL won't start | Verify etcd is running first |
| Pgpool can't connect | Check passwords match, verify PostgreSQL is running |
| No replication | Verify PATRONI_REPLICATION_PASSWORD matches |
| Failover watcher errors | Ensure RAILWAY_API_TOKEN has correct permissions |
