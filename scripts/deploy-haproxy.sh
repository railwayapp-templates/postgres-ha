#!/bin/bash
set -e

echo "Creating haproxy service..."
railway service create haproxy 2>/dev/null || echo "Service haproxy already exists"

echo "Setting haproxy variables..."
# Format: hostname:pgport:patroniport,hostname:pgport:patroniport,...
railway variables --service haproxy --set 'POSTGRES_NODES=${{postgres-1.RAILWAY_PRIVATE_DOMAIN}}:5432:8008,${{postgres-2.RAILWAY_PRIVATE_DOMAIN}}:5432:8008,${{postgres-3.RAILWAY_PRIVATE_DOMAIN}}:5432:8008'
railway variables --service haproxy --set 'HAPROXY_MAX_CONN=1000'
railway variables --service haproxy --set 'HAPROXY_CHECK_INTERVAL=3s'

echo "Deploying haproxy..."
railway up --service haproxy --detach

echo "✅ haproxy deployed (3 replicas configured in railway.toml)"
echo ""
echo "Ports:"
echo "  5432 - Primary (read-write)"
echo "  5433 - Replicas (read-only)"
echo "  8404 - Stats dashboard"
