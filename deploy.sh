#!/bin/bash
set -e

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Railway PostgreSQL HA Cluster Deployment"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

# Check if railway CLI is installed
if ! command -v railway &> /dev/null; then
    echo "❌ Railway CLI not found!"
    echo "Install it with: npm i -g @railway/cli"
    echo "Or: brew install railway"
    exit 1
fi

# Check if we're linked to a project
if ! railway status &> /dev/null; then
    echo "❌ Not linked to a Railway project!"
    echo "Run: railway link"
    echo "Or: railway init"
    exit 1
fi

echo "✓ Railway CLI found"
echo "✓ Linked to project"
echo ""

# Get project info
PROJECT_INFO=$(railway status)
echo "Project: $PROJECT_INFO"
echo ""

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Step 1: Set Shared Variables"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

read -p "Set shared variables now? (y/n) " -n 1 -r
echo ""
if [[ $REPLY =~ ^[Yy]$ ]]; then
    ./scripts/set-shared-variables.sh
else
    echo "⚠️  Make sure to set shared variables before services start!"
fi

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Step 2: Deploy etcd Cluster (3 nodes)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

echo "Deploying etcd-1..."
./scripts/deploy-etcd-1.sh

echo ""
echo "Deploying etcd-2..."
./scripts/deploy-etcd-2.sh

echo ""
echo "Deploying etcd-3..."
./scripts/deploy-etcd-3.sh

echo ""
echo "⏳ Waiting for etcd cluster to be healthy..."
sleep 10

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Step 3: Deploy PostgreSQL Cluster (3 nodes)"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

echo "Deploying postgres-1..."
./scripts/deploy-postgres-1.sh

echo ""
echo "Deploying postgres-2..."
./scripts/deploy-postgres-2.sh

echo ""
echo "Deploying postgres-3..."
./scripts/deploy-postgres-3.sh

echo ""
echo "⏳ Waiting for PostgreSQL cluster to form..."
sleep 15

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Step 4: Deploy Pgpool-II"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

./scripts/deploy-pgpool.sh

echo ""
echo "⏳ Waiting for Pgpool to be ready..."
sleep 10

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "✅ Deployment Complete!"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "Next steps:"
echo "1. Check service status: railway status"
echo "2. View logs: railway logs --service <service-name>"
echo "3. Get connection string: railway variables --service pgpool"
echo ""
echo "Connection string (private):"
echo "postgresql://\${POSTGRES_USER}:\${POSTGRES_PASSWORD}@pgpool.railway.internal:5432/\${POSTGRES_DB}"
echo ""
