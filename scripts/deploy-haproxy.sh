#!/bin/bash
set -e

cd haproxy

echo "Creating haproxy service..."
railway service create haproxy 2>/dev/null || echo "Service haproxy already exists"

echo "Deploying haproxy..."
railway up --service haproxy --detach

cd ..
echo "✅ haproxy deployed (3 replicas configured in railway.toml)"
echo ""
echo "Ports:"
echo "  5432 - Primary (read-write)"
echo "  5433 - Replicas (read-only)"
echo "  8404 - Stats dashboard"
