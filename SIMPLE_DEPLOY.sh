#!/bin/bash

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "Simple PostgreSQL HA Deployment"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "This will deploy each service from its directory."
echo "You'll need to select 'Create new service' for each."
echo ""
echo "Press Ctrl+C to cancel, or Enter to continue..."
read

# Helper function to deploy a service
deploy_service() {
    local dir=$1
    local name=$2

    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "Deploying: $name"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo ""
    echo "When prompted:"
    echo "  1. Select 'Create a new service'"
    echo "  2. Name it: $name"
    echo ""
    echo "Press Enter to continue..."
    read

    cd "$dir"

    # Try to deploy
    railway up || {
        echo "❌ Deployment failed for $name"
        echo "You may need to:"
        echo "  1. Run 'railway service' to create/link the service"
        echo "  2. Then run 'railway up' to deploy"
        cd ..
        return 1
    }

    cd ..
    echo "✅ $name deployed"
}

# Deploy services
deploy_service "etcd-1" "etcd-1"
deploy_service "etcd-2" "etcd-2"
deploy_service "etcd-3" "etcd-3"

echo ""
echo "⏳ Waiting for etcd cluster to stabilize..."
sleep 15

deploy_service "postgres-patroni" "postgres-1"
deploy_service "postgres-patroni" "postgres-2"
deploy_service "postgres-patroni" "postgres-3"

echo ""
echo "⏳ Waiting for PostgreSQL cluster to form..."
sleep 20

deploy_service "pgpool" "pgpool"

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "✅ Deployment Complete!"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""
echo "Next: Configure environment variables in Railway Dashboard"
