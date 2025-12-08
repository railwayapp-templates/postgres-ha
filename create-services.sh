#!/bin/bash
set -e

# Get project and environment IDs from railway status
PROJECT_ID=$(railway status 2>&1 | grep -i project | awk '{print $2}' || echo "")
ENV=$(railway status 2>&1 | grep -i environment | awk '{print $2}' || echo "production")

echo "Project: soothing-serenity"
echo "Environment: $ENV"
echo ""

# Service names to create
SERVICES=("etcd-1" "etcd-2" "etcd-3" "postgres-1" "postgres-2" "postgres-3" "pgpool")

echo "Creating 7 services..."
echo ""

for service in "${SERVICES[@]}"; do
    echo "Deploying $service..."

    # Find the correct directory
    case $service in
        etcd-*)
            DIR="$service"
            ;;
        postgres-*)
            DIR="postgres-patroni"
            ;;
        *)
            DIR="$service"
            ;;
    esac

    cd "$DIR"

    # Deploy with service name
    railway up --service "$service" --detach 2>&1 || {
        echo "Creating new service $service and deploying..."
        railway up --detach 2>&1 | head -5
    }

    cd ..
    echo "✓ $service deployed"
    echo ""
done

echo "All services deployed!"
