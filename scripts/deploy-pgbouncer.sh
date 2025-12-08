#!/bin/bash
set -e

cd pgbouncer

echo "Creating pgbouncer service..."
railway service create pgbouncer 2>/dev/null || echo "Service pgbouncer already exists"

echo "Setting pgbouncer variables..."
railway variables --service pgbouncer --set 'PGBOUNCER_USER=${{shared.POSTGRES_USER}}'
railway variables --service pgbouncer --set 'PGBOUNCER_PASSWORD=${{shared.POSTGRES_PASSWORD}}'
railway variables --service pgbouncer --set 'HAPROXY_HOST=haproxy.railway.internal'
railway variables --service pgbouncer --set 'PGBOUNCER_MAX_CLIENT_CONN=1000'
railway variables --service pgbouncer --set 'PGBOUNCER_DEFAULT_POOL_SIZE=20'
railway variables --service pgbouncer --set 'PGBOUNCER_POOL_MODE=transaction'

echo "Deploying pgbouncer..."
railway up --service pgbouncer --detach

cd ..
echo "✅ pgbouncer deployed (3 replicas configured in railway.toml)"
echo ""
echo "Port: 6432 - Pooled connections"
