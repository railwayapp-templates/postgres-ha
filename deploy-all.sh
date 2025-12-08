#!/bin/bash
set -e

cd /Users/paulocabral/software/railway/mono/templates/postgres-ha

echo "Deploying all services..."
echo ""

# Deploy etcd services
for i in 1 2 3; do
    echo "=== Deploying etcd-$i ==="
    cd etcd-$i
    railway up --service etcd-$i --detach 2>&1 || {
        echo "Service etcd-$i doesn't exist, skipping for now"
    }
    cd ..
done

# Deploy postgres services
for i in 1 2 3; do
    echo "=== Deploying postgres-$i ==="
    cd postgres-patroni
    railway up --service postgres-$i --detach 2>&1 || {
        echo "Service postgres-$i doesn't exist, skipping for now"
    }
    cd ..
done

# Deploy pgpool
echo "=== Deploying pgpool ==="
cd pgpool
railway up --service pgpool --detach 2>&1 || {
    echo "Service pgpool doesn't exist, skipping for now"
}
cd ..

echo ""
echo "Done! Check Railway dashboard for deployment status."
