# Repository Structure

```
postgres-ha/
├── README.md                          # Main documentation
├── STRUCTURE.md                       # This file
├── docker-compose.yml                 # Local development setup
├── .gitignore
│
├── .github/
│   └── workflows/
│       └── build-and-push.yml         # CI/CD for building Docker images
│
├── postgres-patroni/                  # PostgreSQL + Patroni service
│   ├── Dockerfile                     # Multi-stage build (slim + ha variants)
│   ├── Dockerfile.13                  # PostgreSQL 13 variant
│   ├── Dockerfile.14                  # PostgreSQL 14 variant
│   ├── Dockerfile.15                  # PostgreSQL 15 variant
│   ├── Dockerfile.16                  # PostgreSQL 16 variant
│   ├── Dockerfile.17                  # PostgreSQL 17 variant
│   ├── railway.toml                   # Railway service config
│   ├── wrapper.sh                     # Entry point (routes to Patroni or standalone)
│   ├── patroni-runner.sh              # Generates patroni.yml and starts Patroni
│   ├── post_bootstrap.sh              # Runs after PostgreSQL initialization
│   ├── init-ssl.sh                    # SSL certificate generation
│   └── get-postgres-version.sh        # Fetches latest minor version from Docker Hub
│
├── etcd/                              # etcd distributed consensus
│   ├── Dockerfile                     # etcd v3.5.16 image
│   ├── entrypoint.sh                  # Smart bootstrap with leader election
│   └── railway.toml                   # Railway service config
│
├── haproxy/                           # HAProxy load balancer
│   ├── Dockerfile                     # HAProxy 3.2 alpine image
│   ├── entrypoint.sh                  # Config generation and startup
│   ├── haproxy.cfg                    # Template config (for local dev)
│   └── railway.toml                   # Railway service config
│
└── rfcs/                              # Design documents and RFCs
```

## Deployment Instructions

### For Railway (Production)

Each subfolder represents a separate Railway service. Deploy in this order:

1. **etcd** - Deploy 3 times with different names:
   - Service 1: Set `ETCD_NAME=etcd-1`
   - Service 2: Set `ETCD_NAME=etcd-2`
   - Service 3: Set `ETCD_NAME=etcd-3`
2. **postgres-patroni** - Deploy 3 times with different names:
   - Service 1: Set `PATRONI_NAME=postgres-1`
   - Service 2: Set `PATRONI_NAME=postgres-2`
   - Service 3: Set `PATRONI_NAME=postgres-3`
3. **haproxy** - Deploy last (routes traffic to PostgreSQL nodes)

### For Local Testing

```bash
docker-compose up -d
```

Connects to:
- Primary (read-write): `postgresql://railway:railway@localhost:5432/railway`
- Replicas (read-only): `postgresql://railway:railway@localhost:5433/railway`

## File Descriptions

### postgres-patroni/

- **Dockerfile**: Multi-stage build supporting two variants:
  - `slim`: Standalone PostgreSQL with SSL (~400MB)
  - `ha`: Adds Patroni for high availability (~550MB)
- **Dockerfile.{13-17}**: Version-specific Dockerfiles for PostgreSQL 13-17
- **wrapper.sh**: Entry point that routes to Patroni (if `PATRONI_ENABLED=true`) or standard PostgreSQL
- **patroni-runner.sh**: Dynamically generates `patroni.yml` from environment variables and starts Patroni
- **post_bootstrap.sh**: Runs after PostgreSQL init to create replicator user and configure SSL
- **init-ssl.sh**: Generates self-signed SSL certificates for PostgreSQL
- **get-postgres-version.sh**: Queries Docker Hub API to get latest minor version for a major release
- **railway.toml**: Defines build and deploy settings

### etcd/

- **Dockerfile**: Uses official etcd image with custom entrypoint
- **entrypoint.sh**: Smart bootstrap logic:
  - Determines bootstrap leader (alphabetically first node)
  - Leader bootstraps single-node cluster for instant quorum
  - Other nodes wait for leader, add themselves, then join
  - Handles stale data cleanup on failed bootstrap
- **railway.toml**: Service configuration

### haproxy/

- **Dockerfile**: HAProxy 3.2 on Alpine Linux
- **entrypoint.sh**: Generates HAProxy config from `POSTGRES_NODES` environment variable
- **haproxy.cfg**: Template config for local development (entrypoint.sh generates production config)
- **railway.toml**: Service configuration

### .github/workflows/

- **build-and-push.yml**: GitHub Actions workflow that:
  - Builds PostgreSQL images for versions 13-17
  - Builds etcd and HAProxy images
  - Pushes to GitHub Container Registry (ghcr.io)
  - Runs weekly and on push to main
  - Supports multi-arch (amd64, arm64)

## Key Configuration Points

### Private Networking

All services use Railway's private networking for service-to-service communication:

```
postgres-1.railway.internal:5432  (PostgreSQL)
postgres-1.railway.internal:8008  (Patroni API)
postgres-2.railway.internal:5432
postgres-2.railway.internal:8008
postgres-3.railway.internal:5432
postgres-3.railway.internal:8008
etcd-1.railway.internal:2379      (etcd client)
etcd-1.railway.internal:2380      (etcd peer)
etcd-2.railway.internal:2379
etcd-2.railway.internal:2380
etcd-3.railway.internal:2379
etcd-3.railway.internal:2380
haproxy.railway.internal:5432     (primary)
haproxy.railway.internal:5433     (replicas)
haproxy.railway.internal:8404     (stats)
```

### Shared Variables

These must be set as shared environment variables in Railway:

- `POSTGRES_USER` (default: railway)
- `POSTGRES_PASSWORD` (auto-generate secure password)
- `POSTGRES_DB` (default: railway)
- `PATRONI_REPLICATION_PASSWORD` (auto-generate)

### Per-Service Variables

**etcd** (set individually per instance):
- `ETCD_NAME`: etcd-1, etcd-2, or etcd-3
- `ETCD_INITIAL_CLUSTER`: Full cluster membership string

**postgres-patroni** (set individually per instance):
- `PATRONI_NAME`: postgres-1, postgres-2, or postgres-3
- `PATRONI_ENABLED`: true (to enable HA mode)

**haproxy**:
- `POSTGRES_NODES`: Node list in format `host:pgport:patroniport,...`

### HAProxy Health Checks

HAProxy uses Patroni's REST API for health checks:

- `/primary` - Returns 200 if node is the current leader
- `/replica` - Returns 200 if node is a healthy replica

This enables automatic routing without a separate watcher process.

## Build Process

1. Railway reads `railway.toml` from each service folder
2. Builds Docker image using specified Dockerfile
3. Deploys with configured replicas and restart policy
4. Injects environment variables
5. Starts container with configured start command
6. Monitors via health checks

## Docker Images

Pre-built images are available on GitHub Container Registry:

```
ghcr.io/<owner>/postgres-ha/postgres-patroni:17
ghcr.io/<owner>/postgres-ha/postgres-patroni:16
ghcr.io/<owner>/postgres-ha/postgres-patroni:15
ghcr.io/<owner>/postgres-ha/postgres-patroni:14
ghcr.io/<owner>/postgres-ha/postgres-patroni:13
ghcr.io/<owner>/postgres-ha/etcd:3.5.16
ghcr.io/<owner>/postgres-ha/haproxy:3.2
```
