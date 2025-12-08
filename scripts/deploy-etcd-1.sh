#!/bin/bash
set -e

cd etcd

echo "Creating etcd-1 service..."
railway service create etcd-1 2>/dev/null || echo "Service etcd-1 already exists"

echo "Setting etcd-1 variables..."
railway variables --service etcd-1 --set ETCD_NAME=etcd-1
railway variables --service etcd-1 --set ETCD_INITIAL_CLUSTER="etcd-1=http://etcd-1.railway.internal:2380,etcd-2=http://etcd-2.railway.internal:2380,etcd-3=http://etcd-3.railway.internal:2380"
railway variables --service etcd-1 --set ETCD_INITIAL_CLUSTER_STATE=new
railway variables --service etcd-1 --set ETCD_INITIAL_CLUSTER_TOKEN=railway-pg-ha
railway variables --service etcd-1 --set ETCD_LISTEN_CLIENT_URLS="http://0.0.0.0:2379"
railway variables --service etcd-1 --set ETCD_ADVERTISE_CLIENT_URLS="http://etcd-1.railway.internal:2379"
railway variables --service etcd-1 --set ETCD_LISTEN_PEER_URLS="http://0.0.0.0:2380"
railway variables --service etcd-1 --set ETCD_INITIAL_ADVERTISE_PEER_URLS="http://etcd-1.railway.internal:2380"
railway variables --service etcd-1 --set ETCD_DATA_DIR=/etcd-data
railway variables --service etcd-1 --set ETCD_ENABLE_V2=true

echo "Deploying etcd-1..."
railway up --service etcd-1 --detach

cd ..
echo "✅ etcd-1 deployed"
