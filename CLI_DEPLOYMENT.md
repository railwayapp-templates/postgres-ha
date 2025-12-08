# Deploy with Railway CLI

Since you've already linked the templates folder to your Railway project, deployment is super simple!

## Quick Start (Automated)

```bash
cd templates/postgres-ha
./deploy.sh
```

This will:
1. Set shared variables (auto-generate passwords)
2. Deploy all 8 services in the correct order
3. Wait for each phase to complete

**That's it!** ✨

---

## Manual Deployment (Step by Step)

If you prefer to deploy each service manually:

### 1. Set Shared Variables

```bash
cd templates/postgres-ha
./scripts/set-shared-variables.sh
```

This generates secure passwords and sets:
- `POSTGRES_USER`, `POSTGRES_PASSWORD`, `POSTGRES_DB`
- `PATRONI_REPLICATION_USERNAME`, `PATRONI_REPLICATION_PASSWORD`
- `PATRONI_SCOPE`, `PATRONI_TTL`, `PATRONI_LOOP_WAIT`, `PATRONI_ETCD_HOSTS`

### 2. Deploy etcd Cluster

```bash
./scripts/deploy-etcd-1.sh
./scripts/deploy-etcd-2.sh
./scripts/deploy-etcd-3.sh
```

Wait for all 3 to be healthy:
```bash
railway logs --service etcd-1
# Look for "etcd cluster is ready"
```

### 3. Deploy PostgreSQL Cluster

```bash
./scripts/deploy-postgres-1.sh
./scripts/deploy-postgres-2.sh
./scripts/deploy-postgres-3.sh
```

Wait for cluster to form:
```bash
railway logs --service postgres-1
# Look for "I am postgres-1, the leader with the lock"
```

### 4. Deploy Pgpool-II

```bash
./scripts/deploy-pgpool.sh
```

**Important**: After deployment, set replicas to 3 in Railway dashboard:
- Go to pgpool service → Settings → Deploy
- Change "Replicas" to `3`
- Click Save

Or edit `pgpool/railway.toml` and set:
```toml
[deploy]
numReplicas = 3
```

Then redeploy:
```bash
cd pgpool
railway up --service pgpool
```

---

## Verify Deployment

### Check All Services

```bash
railway status
```

Should show 7 services all healthy.

### View Logs

```bash
# etcd
railway logs --service etcd-1

# PostgreSQL leader
railway logs --service postgres-1

# Pgpool
railway logs --service pgpool
```

### Get Connection String

```bash
# View all variables
railway variables

# Get DATABASE_URL
railway variables | grep DATABASE_URL
```

Or create it manually:
```bash
# Private (from other Railway services)
postgresql://railway:<password>@pgpool.railway.internal:5432/railway

# Public (via TCP proxy)
# Get domain from: railway domain --service pgpool
postgresql://railway:<password>@<pgpool-domain>/railway
```

---

## Connect to Database

### From Your Local Machine

```bash
# Get connection details
POSTGRES_PASSWORD=$(railway variables | grep POSTGRES_PASSWORD | awk '{print $2}')
PGPOOL_DOMAIN=$(railway domain --service pgpool)

# Connect
psql "postgresql://railway:${POSTGRES_PASSWORD}@${PGPOOL_DOMAIN}/railway"
```

### From Another Railway Service

Add this to your app's environment variables:

```bash
railway variables --service your-app --set 'DATABASE_URL=postgresql://${{shared.POSTGRES_USER}}:${{shared.POSTGRES_PASSWORD}}@pgpool.railway.internal:5432/${{shared.POSTGRES_DB}}'
```

---

## Test the Cluster

### Check Replication Status

```bash
railway run --service postgres-1 psql -U railway -c "SELECT * FROM pg_stat_replication;"
```

Should show 2 replicas (postgres-2, postgres-3).

### Test Failover

```bash
# Stop the leader
railway service --service postgres-1 stop

# Watch pgpool logs for failover detection
railway logs --service pgpool --follow

# Should see patroni-watcher detecting new leader
```

### Restart Failed Node

```bash
railway service --service postgres-1 start

# Watch logs - it should rejoin as replica
railway logs --service postgres-1 --follow

# Look for: "I am postgres-1, a secondary, and following a leader"
```

---

## Useful Commands

```bash
# List all services
railway service list

# View service details
railway service --service postgres-1

# View variables for a service
railway variables --service postgres-1

# Update a variable
railway variables --service pgpool --set PGPOOL_NUM_INIT_CHILDREN=64

# View logs (last 100 lines)
railway logs --service postgres-1

# Stream logs
railway logs --service postgres-1 --follow

# Restart a service
railway service --service postgres-1 restart

# Open Railway dashboard
railway open

# Link to different project
railway link

# Unlink from project
railway unlink
```

---

## Troubleshooting with CLI

### etcd Won't Start

```bash
# Check logs
railway logs --service etcd-1

# Check variables
railway variables --service etcd-1

# Restart
railway service --service etcd-1 restart
```

### PostgreSQL Won't Start

```bash
# Check if etcd is running
railway status | grep etcd

# Check PostgreSQL logs
railway logs --service postgres-1

# Verify etcd connection
railway run --service postgres-1 curl http://etcd-1.railway.internal:2379/health
```

### Check Volume Usage

```bash
railway volume list --service postgres-1
```

### Redeploy a Service

```bash
cd postgres-patroni
railway up --service postgres-1
```

---

## Cleanup / Destroy

To remove all services:

```bash
# Delete each service
railway service delete etcd-1
railway service delete etcd-2
railway service delete etcd-3
railway service delete postgres-1
railway service delete postgres-2
railway service delete postgres-3
railway service delete pgpool

# Or delete entire project (be careful!)
railway project delete
```

---

## Next Steps

1. **Enable TCP proxy for external access**:
   ```bash
   railway domain --service pgpool
   ```

2. **Set up monitoring**:
   - Use Railway's built-in metrics
   - Check pgpool logs for patroni-watcher output

3. **Configure backups**:
   - Railway Pro includes automatic volume snapshots
   - Manual: `pg_dump` via `railway run`

4. **Tune performance**:
   - Edit `postgres-patroni/patroni.yml`
   - Edit `pgpool/pgpool.conf`
   - Redeploy services

5. **Scale Pgpool**:
   - Increase replicas for more connection capacity
   - Update in Railway dashboard or railway.toml
