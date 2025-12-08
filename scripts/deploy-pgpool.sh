#!/bin/bash
set -e

cd pgpool

echo "Creating pgpool service..."
railway service create pgpool 2>/dev/null || echo "Service pgpool already exists"

echo "Setting pgpool variables..."
railway variables --service pgpool --set 'POSTGRES_PASSWORD=${{shared.POSTGRES_PASSWORD}}'
railway variables --service pgpool --set 'REPLICATION_PASSWORD=${{shared.PATRONI_REPLICATION_PASSWORD}}'
railway variables --service pgpool --set 'PGPOOL_BACKEND_NODES=0:${{postgres-1.PGHOST}}:${{postgres-1.PGPORT}},1:${{postgres-2.PGHOST}}:${{postgres-2.PGPORT}},2:${{postgres-3.PGHOST}}:${{postgres-3.PGPORT}}'

echo "Deploying pgpool..."
railway up --service pgpool --detach

cd ..
echo "✅ pgpool deployed"
echo ""
echo "⚠️  Note: Set numReplicas to 3 in Railway dashboard for HA"
echo "   Settings → Deploy → Replicas → 3"
