#!/bin/bash
set -e

cd postgres-patroni

echo "Creating postgres-1 service..."
railway service create postgres-1 2>/dev/null || echo "Service postgres-1 already exists"

echo "Setting postgres-1 variables..."
railway variables --service postgres-1 --set PATRONI_NAME=postgres-1
railway variables --service postgres-1 --set 'PATRONI_SCOPE=${{shared.PATRONI_SCOPE}}'
railway variables --service postgres-1 --set 'PATRONI_ETCD_HOSTS=${{shared.PATRONI_ETCD_HOSTS}}'
railway variables --service postgres-1 --set 'PATRONI_TTL=${{shared.PATRONI_TTL}}'
railway variables --service postgres-1 --set 'PATRONI_LOOP_WAIT=${{shared.PATRONI_LOOP_WAIT}}'
railway variables --service postgres-1 --set 'POSTGRES_USER=${{shared.POSTGRES_USER}}'
railway variables --service postgres-1 --set 'POSTGRES_PASSWORD=${{shared.POSTGRES_PASSWORD}}'
railway variables --service postgres-1 --set 'POSTGRES_DB=${{shared.POSTGRES_DB}}'
railway variables --service postgres-1 --set 'PATRONI_REPLICATION_USERNAME=${{shared.PATRONI_REPLICATION_USERNAME}}'
railway variables --service postgres-1 --set 'PATRONI_REPLICATION_PASSWORD=${{shared.PATRONI_REPLICATION_PASSWORD}}'
railway variables --service postgres-1 --set PGDATA=/var/lib/postgresql/data
railway variables --service postgres-1 --set PATRONI_ENABLED=true

echo "Adding volume to postgres-1..."
railway volume add --service postgres-1 --mount-path /var/lib/postgresql/data 2>/dev/null || echo "Volume already exists"

echo "Deploying postgres-1..."
railway up --service postgres-1 --detach

cd ..
echo "✅ postgres-1 deployed"
