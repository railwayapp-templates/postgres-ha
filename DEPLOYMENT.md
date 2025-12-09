# Step-by-Step Deployment Guide

This guide walks you through deploying the HA PostgreSQL cluster to Railway manually.

## Prerequisites

- Railway account (Pro plan recommended for volume snapshots)
- Railway project created
- Private networking enabled (automatic in newer projects)

## Deployment Overview

We'll deploy **7 services** in this order:
1. etcd-1, etcd-2, etcd-3 (consensus layer - must start first)
2. postgres-1, postgres-2, postgres-3 (database nodes)
3. pgpool (connection pooler with built-in failover watcher)

**Estimated time**: 15-20 minutes

---

## Phase 1: Prepare Shared Variables

Before deploying any services, set up shared environment variables in your Railway project.

### In Railway Dashboard:

1. Go to your project
2. Click on "Variables" (left sidebar)
3. Select "Shared" tab
4. Add these variables:

```bash
# Database credentials
POSTGRES_USER=railway
POSTGRES_PASSWORD=<click "Generate" for secure password>
POSTGRES_DB=railway

# Replication credentials
PATRONI_REPLICATION_USERNAME=replicator
PATRONI_REPLICATION_PASSWORD=<click "Generate" for secure password>

# Cluster settings
PATRONI_SCOPE=pg-ha-cluster
PATRONI_TTL=30
PATRONI_LOOP_WAIT=10
PATRONI_ETCD_HOSTS=etcd-1.railway.internal:2379,etcd-2.railway.internal:2379,etcd-3.railway.internal:2379

# Optional: For failover watcher (get from Railway project settings)
RAILWAY_API_TOKEN=<your-project-token>
RAILWAY_PROJECT_ID=<your-project-id>
RAILWAY_ENVIRONMENT_ID=<your-environment-id>
```

**Save these variables** - all services will reference them.

---

## Phase 2: Deploy etcd Cluster (3 services)

**Why first?** Patroni needs etcd running before PostgreSQL can start.

### Service 1: etcd-1

1. Click **"+ New Service"** in Railway
2. Select **"Empty Service"**
3. Name it: `etcd-1`
4. Go to **Settings** → **Source**:
   - Choose "Dockerfile"
   - Set root directory: `templates/postgres-ha/etcd-1`
5. Go to **Variables** tab → Add these **service-specific** variables:

```bash
ETCD_NAME=etcd-1
ETCD_INITIAL_CLUSTER=etcd-1=http://etcd-1.railway.internal:2380,etcd-2=http://etcd-2.railway.internal:2380,etcd-3=http://etcd-3.railway.internal:2380
ETCD_INITIAL_CLUSTER_STATE=new
ETCD_INITIAL_CLUSTER_TOKEN=railway-pg-ha
ETCD_LISTEN_CLIENT_URLS=http://0.0.0.0:2379
ETCD_ADVERTISE_CLIENT_URLS=http://etcd-1.railway.internal:2379
ETCD_LISTEN_PEER_URLS=http://0.0.0.0:2380
ETCD_INITIAL_ADVERTISE_PEER_URLS=http://etcd-1.railway.internal:2380
ETCD_DATA_DIR=/etcd-data
```

6. Go to **Settings** → **Deploy**:
   - Start command: `/usr/local/bin/etcd`
   - Restart policy: Always
7. Click **"Deploy"**

### Service 2: etcd-2

Repeat the same steps, but change:
- Service name: `etcd-2`
- Root directory: `templates/postgres-ha/etcd-2`
- Variables: Replace all `etcd-1` with `etcd-2` in the URLs

```bash
ETCD_NAME=etcd-2
ETCD_ADVERTISE_CLIENT_URLS=http://etcd-2.railway.internal:2379
ETCD_INITIAL_ADVERTISE_PEER_URLS=http://etcd-2.railway.internal:2380
# (ETCD_INITIAL_CLUSTER stays the same - it lists all 3 nodes)
```

### Service 3: etcd-3

Repeat for etcd-3:
- Service name: `etcd-3`
- Root directory: `templates/postgres-ha/etcd-3`
- Variables: Replace with `etcd-3` in URLs

**Wait for all 3 etcd services to be healthy** (green status) before continuing.

### Verify etcd Cluster

Check logs of any etcd service - you should see:
```
etcd cluster is ready
member X is healthy
```

---

## Phase 3: Deploy PostgreSQL + Patroni (3 services)

### Service 4: postgres-1

1. Click **"+ New Service"**
2. Select **"Empty Service"**
3. Name it: `postgres-1`
4. Go to **Settings** → **Source**:
   - Choose "Dockerfile"
   - Root directory: `templates/postgres-ha/postgres-patroni`
5. Go to **Variables** tab → Add:

```bash
# Service-specific
PATRONI_NAME=postgres-1

# Reference shared variables
PATRONI_SCOPE=${{shared.PATRONI_SCOPE}}
PATRONI_ETCD_HOSTS=${{shared.PATRONI_ETCD_HOSTS}}
PATRONI_TTL=${{shared.PATRONI_TTL}}
PATRONI_LOOP_WAIT=${{shared.PATRONI_LOOP_WAIT}}

# Database credentials (reference shared)
POSTGRES_USER=${{shared.POSTGRES_USER}}
POSTGRES_PASSWORD=${{shared.POSTGRES_PASSWORD}}
POSTGRES_DB=${{shared.POSTGRES_DB}}

# Replication (reference shared)
PATRONI_REPLICATION_USERNAME=${{shared.PATRONI_REPLICATION_USERNAME}}
PATRONI_REPLICATION_PASSWORD=${{shared.PATRONI_REPLICATION_PASSWORD}}

# Data directory
PGDATA=/var/lib/postgresql/data
```

6. Go to **Settings** → **Deploy**:
   - Start command: `patroni /etc/patroni/patroni.yml`
   - Restart policy: Always

7. **IMPORTANT**: Add a volume:
   - Go to **Settings** → **Volumes**
   - Click **"+ New Volume"**
   - Mount path: `/var/lib/postgresql/data`
   - Size: `10` GB (or larger)

8. Click **"Deploy"**

### Service 5: postgres-2

Repeat for postgres-2:
- Service name: `postgres-2`
- Root directory: `templates/postgres-ha/postgres-patroni` (same as postgres-1)
- **Only difference**: `PATRONI_NAME=postgres-2`
- Add volume: `/var/lib/postgresql/data` (10GB)
- All other variables reference shared (same as postgres-1)

### Service 6: postgres-3

Repeat for postgres-3:
- Service name: `postgres-3`
- Root directory: `templates/postgres-ha/postgres-patroni` (same)
- **Only difference**: `PATRONI_NAME=postgres-3`
- Add volume: `/var/lib/postgresql/data` (10GB)

**Wait for all 3 PostgreSQL services to start.**

### Verify PostgreSQL Cluster

Check logs of `postgres-1` - you should see:
```
INFO: establishing a new patroni connection to the postgres cluster
INFO: Lock owner: postgres-1; I am postgres-1
INFO: no action. I am (postgres-1), the leader with the lock
```

Check logs of `postgres-2` and `postgres-3`:
```
INFO: no action. I am (postgres-X), a secondary, and following a leader (postgres-1)
```

This confirms:
- ✅ postgres-1 is the leader
- ✅ postgres-2 and postgres-3 are replicas

---

## Phase 4: Deploy Pgpool-II

### Service 7: pgpool

1. Click **"+ New Service"**
2. Select **"Empty Service"**
3. Name it: `pgpool`
4. Go to **Settings** → **Source**:
   - Choose "Dockerfile"
   - Root directory: `templates/postgres-ha/pgpool`
5. Go to **Variables** tab → Add:

```bash
# Backend configuration (hardcoded in pgpool.conf, but passwords needed)
POSTGRES_PASSWORD=${{shared.POSTGRES_PASSWORD}}
REPLICATION_PASSWORD=${{shared.PATRONI_REPLICATION_PASSWORD}}

# Optional: Tuning
PGPOOL_NUM_INIT_CHILDREN=32
PGPOOL_MAX_POOL=4
```

6. Go to **Settings** → **Deploy**:
   - Start command: `pgpool -n -f /etc/pgpool-II/pgpool.conf -F /etc/pgpool-II/pcp.conf`
   - Restart policy: Always
   - **Replicas**: `3` (this enables horizontal scaling!)

7. Go to **Settings** → **Networking**:
   - Enable **"Public Networking"** (generates public domain)
   - Enable **"TCP Proxy"** on port `5432` (for external connections)

8. Click **"Deploy"**

**Wait for pgpool to start.** Check logs for:
```
Pgpool-II configuration:
  Backends: postgres-1, postgres-2, postgres-3
```

---

## Phase 5: Get Connection String

### For Applications in Same Railway Project (Private)

Use private networking (fastest, no egress costs):

```bash
DATABASE_URL=postgresql://${{shared.POSTGRES_USER}}:${{shared.POSTGRES_PASSWORD}}@pgpool.railway.internal:5432/${{shared.POSTGRES_DB}}
```

Add this as a shared variable, and reference it in your app:
```bash
DATABASE_URL=${{shared.DATABASE_URL}}
```

### For External Applications (Public)

1. Go to `pgpool` service
2. Go to **Settings** → **Networking**
3. Copy the **TCP Proxy Domain** (e.g., `xyz.proxy.rlwy.net:12345`)

Use this connection string:
```bash
postgresql://railway:<password>@xyz.proxy.rlwy.net:12345/railway
```

---

## Testing Your Cluster

### 1. Connect to Database

From any service in your Railway project:

```bash
psql $DATABASE_URL
```

Or from your local machine (using TCP proxy):
```bash
psql postgresql://railway:<password>@xyz.proxy.rlwy.net:12345/railway
```

### 2. Verify Replication

```sql
-- Check replication status
SELECT * FROM pg_stat_replication;

-- Should show 2 replicas (postgres-2, postgres-3)
```

### 3. Test Read/Write Splitting

Pgpool automatically routes queries:

```sql
-- This goes to PRIMARY (postgres-1)
INSERT INTO test_table VALUES (1, 'test');

-- This goes to REPLICAS (postgres-2 or postgres-3)
SELECT * FROM test_table;
```

### 4. Check Cluster Status

Get Patroni cluster info:

```bash
# From any postgres service logs, or use Railway's service shell
curl http://postgres-1.railway.internal:8008/cluster
```

Response shows leader and replicas:
```json
{
  "members": [
    {"name": "postgres-1", "role": "leader", "state": "running"},
    {"name": "postgres-2", "role": "replica", "state": "streaming"},
    {"name": "postgres-3", "role": "replica", "state": "streaming"}
  ]
}
```

---

## Testing Failover

### Simulate Primary Failure

1. Go to `postgres-1` service in Railway
2. Click **"Restart"** or temporarily stop it

**What happens**:
```
T+0s   postgres-1 goes down
T+2s   Patroni detects via etcd
T+4s   postgres-2 elected as new leader
T+6s   Pgpool detects new primary
T+10s  Failover complete
```

### Verify New Leader

Check pgpool logs for patroni-watcher output showing the leader change.

Check Patroni:
```bash
curl http://postgres-2.railway.internal:8008/cluster
# postgres-2 should now show role: "leader"
```

### Verify Application Connectivity

Your application should automatically reconnect to the new primary (via pgpool). No configuration changes needed!

---

## Troubleshooting

### etcd Won't Start

**Error**: `etcdserver: member already bootstrapped`

**Fix**: Delete all etcd volumes and redeploy fresh.

### PostgreSQL Won't Start

**Error**: `could not connect to etcd`

**Fix**:
1. Verify all 3 etcd services are running
2. Check `PATRONI_ETCD_HOSTS` variable matches etcd service names
3. Ensure private networking is enabled

### Pgpool Connection Refused

**Error**: `could not connect to server`

**Fix**:
1. Check all 3 PostgreSQL services are running
2. Verify passwords match in pgpool and PostgreSQL
3. Check pgpool logs for backend health status

### Replication Not Working

**Error**: Replicas not streaming

**Fix**:
1. Check `PATRONI_REPLICATION_PASSWORD` matches across all services
2. Verify pg_hba.conf allows replication connections
3. Check PostgreSQL logs for authentication errors

---

## Success Checklist

- [ ] All 3 etcd services are healthy (green)
- [ ] All 3 PostgreSQL services are healthy (green)
- [ ] postgres-1 shows "I am the leader" in logs
- [ ] postgres-2 and postgres-3 show "I am a secondary" in logs
- [ ] pgpool service is healthy (green)
- [ ] pgpool logs show patroni-watcher detecting leader
- [ ] Can connect via `DATABASE_URL`
- [ ] Can query database successfully
- [ ] `pg_stat_replication` shows 2 active replicas

**Congratulations! Your HA PostgreSQL cluster is running!** 🎉

---

## Next Steps

- Set up volume backups (Railway Pro)
- Configure monitoring/alerting
- Test failover scenarios
- Tune PostgreSQL performance settings
- Add SSL/TLS for connections
