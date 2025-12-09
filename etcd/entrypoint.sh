#!/bin/sh
# Just run etcd - it handles peer discovery and retries internally
exec /usr/local/bin/etcd
