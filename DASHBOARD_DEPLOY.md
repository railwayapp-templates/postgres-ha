# Deploy via Railway Dashboard (Easiest Method)

Since Railway CLI requires interactive prompts, using the dashboard is actually faster.

## Step 1: Create All Services (2 minutes)

Go to https://railway.app/project/soothing-serenity

Click **"+ New"** 7 times to create these services:
1. `etcd-1`
2. `etcd-2`
3. `etcd-3`
4. `postgres-1`
5. `postgres-2`
6. `postgres-3`
7. `pgpool`

For each service:
- Click "+ New" → "Empty Service"
- Click on the service → Settings → Change name

---

## Step 2: Link Source Code (1 minute per service)

### For etcd-1:
1. Click on `etcd-1` service
2. Settings → Source → **"Repo"**
3. Select your GitHub repo (or click "Deploy from GitHub repo")
4. Root Directory: `templates/postgres-ha/etcd-1`
5. Save

### For etcd-2:
- Root Directory: `templates/postgres-ha/etcd-2`

### For etcd-3:
- Root Directory: `templates/postgres-ha/etcd-3`

### For postgres-1:
- Root Directory: `templates/postgres-ha/postgres-patroni`

### For postgres-2:
- Root Directory: `templates/postgres-ha/postgres-patroni` (same as postgres-1)

### For postgres-3:
- Root Directory: `templates/postgres-ha/postgres-patroni` (same as postgres-1)

### For pgpool:
- Root Directory: `templates/postgres-ha/pgpool`

---

## Step 3: Set Shared Variables

Click on your project (not a service) → Variables → **"Shared"** tab:

```bash
POSTGRES_USER=railway
POSTGRES_PASSWORD=<click "Generate" - creates random password>
POSTGRES_DB=railway
PATRONI_REPLICATION_USERNAME=replicator
PATRONI_REPLICATION_PASSWORD=<click "Generate">
PATRONI_SCOPE=pg-ha-cluster
PATRONI_TTL=30
PATRONI_LOOP_WAIT=10
PATRONI_ETCD_HOSTS=etcd-1.railway.internal:2379,etcd-2.railway.internal:2379,etcd-3.railway.internal:2379
```

Click "Add" for each variable.

---

## Step 4: Set Service-Specific Variables

### etcd-1 Variables:
Click `etcd-1` → Variables → Add these:

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

### etcd-2 Variables:
Same as etcd-1, but change:
```bash
ETCD_NAME=etcd-2
ETCD_ADVERTISE_CLIENT_URLS=http://etcd-2.railway.internal:2379
ETCD_INITIAL_ADVERTISE_PEER_URLS=http://etcd-2.railway.internal:2380
```
(ETCD_INITIAL_CLUSTER stays the same)

### etcd-3 Variables:
Same pattern:
```bash
ETCD_NAME=etcd-3
ETCD_ADVERTISE_CLIENT_URLS=http://etcd-3.railway.internal:2379
ETCD_INITIAL_ADVERTISE_PEER_URLS=http://etcd-3.railway.internal:2380
```

### postgres-1 Variables:
```bash
PATRONI_NAME=postgres-1
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

### postgres-2 Variables:
Same as postgres-1, but:
```bash
PATRONI_NAME=postgres-2
```

### postgres-3 Variables:
Same as postgres-1, but:
```bash
PATRONI_NAME=postgres-3
```

### pgpool Variables:
```bash
POSTGRES_PASSWORD=${{shared.POSTGRES_PASSWORD}}
REPLICATION_PASSWORD=${{shared.PATRONI_REPLICATION_PASSWORD}}
PGPOOL_NUM_INIT_CHILDREN=32
PGPOOL_MAX_POOL=4
```

## Step 5: Add Volumes (PostgreSQL only)

For each of `postgres-1`, `postgres-2`, `postgres-3`:

1. Click service → Settings → **Volumes**
2. Click **"+ New Volume"**
3. Mount Path: `/var/lib/postgresql/data`
4. Size: `10` GB
5. Click "Add"

---

## Step 6: Configure Pgpool Replicas

1. Click `pgpool` service
2. Settings → Deploy
3. Find **"Replicas"** setting
4. Change from `1` to `3`
5. Save

---

## Step 7: Deploy!

Services should auto-deploy once source and variables are set. If not:

1. Click each service
2. Click **"Deploy"** button

**Order matters:**
1. Deploy etcd-1, etcd-2, etcd-3 first
2. Wait for all 3 to be healthy (green)
3. Then deploy postgres-1, postgres-2, postgres-3
4. Finally deploy pgpool

---

## Verify Deployment

Check each service shows green "Active" status. Click on them to see logs:

**etcd-1 logs should show:**
```
etcd cluster is ready
health check passed
```

**postgres-1 logs should show:**
```
INFO: I am postgres-1, the leader with the lock
```

**postgres-2/3 logs should show:**
```
INFO: I am postgres-X, a secondary, and following a leader
```

**pgpool logs should show:**
```
Pgpool-II configuration:
  Backends: postgres-1, postgres-2, postgres-3
```

---

## Get Connection String

Once all services are healthy:

1. Click `pgpool` service
2. Settings → Networking → **TCP Proxy**
3. Enable TCP proxy on port `5432`
4. Copy the domain (e.g., `abc123.proxy.rlwy.net:12345`)

**Connection string:**
```
postgresql://railway:<your-postgres-password>@abc123.proxy.rlwy.net:12345/railway
```

Get the password from: Project → Variables → Shared → POSTGRES_PASSWORD

---

## Troubleshooting

**etcd won't start:**
- Check all 3 have the same `ETCD_INITIAL_CLUSTER` value
- Ensure private networking is enabled

**postgres won't start:**
- Make sure all 3 etcd services are healthy first
- Check `PATRONI_ETCD_HOSTS` matches the etcd service names

**No replication:**
- Verify `PATRONI_REPLICATION_PASSWORD` is set correctly
- Check postgres logs for authentication errors

**Can't connect:**
- Make sure pgpool has TCP proxy enabled
- Check firewall settings
- Verify passwords match

---

## Total Time: ~15-20 minutes

The dashboard method is actually faster than CLI for this because you can see everything visually and don't need to context-switch between terminals.
