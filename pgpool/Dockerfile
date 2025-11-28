FROM railwayapp/pgpool:4

USER root

# Force rebuild - restored watcher with retry logic and periodic resync
ARG CACHE_BUST=2025-11-28-watcher-v3-original

ENV PGPOOL_ENABLE_POOL_HBA=yes

# Python + requests for watcher, netcat for diagnostics
RUN apt-get update && apt-get install -y \
    python3 \
    python3-requests \
    netcat-openbsd \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

# Copy watcher script
COPY patroni-watcher.py /opt/patroni-watcher.py
RUN chmod +x /opt/patroni-watcher.py

# Copy custom pool_hba.conf
COPY pool_hba.conf /opt/bitnami/pgpool/conf/pool_hba.conf

# Backup original run.sh and replace it with our custom version
RUN cp /opt/bitnami/scripts/pgpool/run.sh /opt/bitnami/scripts/pgpool/run.sh.orig

COPY <<'EOF' /opt/bitnami/scripts/pgpool/run.sh
#!/bin/bash
set -e

echo "[custom-run] Starting Bitnami pgpool initialization..." >&2

# Source Bitnami's setup functions
. /opt/bitnami/scripts/libpgpool.sh
. /opt/bitnami/scripts/libos.sh

# Run Bitnami's initialization (generates configs from env vars)
echo "[custom-run] Running Bitnami setup..." >&2
info "** Starting Pgpool-II setup **"
/opt/bitnami/scripts/pgpool/setup.sh
info "** Pgpool-II setup finished! **"

# Restore our custom pool_hba.conf
echo "[custom-run] Restoring custom pool_hba.conf..." >&2
mkdir -p /opt/bitnami/pgpool/etc
cat > /opt/bitnami/pgpool/etc/pool_hba.conf <<'POOL_HBA_EOF'
local   all         all                               trust
host    all         all         0.0.0.0/0             md5
host    all         all         ::0/0                 md5
POOL_HBA_EOF

# Verify pgpool.conf exists
PGPOOL_CONF="/opt/bitnami/pgpool/conf/pgpool.conf"

if [ -f "$PGPOOL_CONF" ]; then
  echo "[custom-run] pgpool.conf found, verifying settings..." >&2

  # Log key settings for debugging
  echo "[custom-run] Health check period: $(grep '^health_check_period' $PGPOOL_CONF || echo 'not set')" >&2
  echo "[custom-run] SR check period: $(grep '^sr_check_period' $PGPOOL_CONF || echo 'not set')" >&2
  echo "[custom-run] Load balance mode: $(grep '^load_balance_mode' $PGPOOL_CONF || echo 'not set')" >&2
  echo "[custom-run] Auto failback: $(grep '^auto_failback' $PGPOOL_CONF || echo 'not set')" >&2
  echo "[custom-run] Backend clustering mode: $(grep '^backend_clustering_mode' $PGPOOL_CONF || echo 'not set')" >&2
else
  echo "[custom-run] ERROR: pgpool.conf not found after setup" >&2
  ls -la /opt/bitnami/pgpool/conf/ >&2 || true
  exit 1
fi

# Generate pcp.conf using md5sum directly (pg_md5 is unreliable in this image)
echo "[custom-run] Generating pcp.conf for PCP authentication..." >&2
USERNAME="${PGPOOL_ADMIN_USERNAME:-admin}"

HASH=$(printf '%s' "${PGPOOL_ADMIN_PASSWORD}" | md5sum | cut -d' ' -f1)

if [ -z "$HASH" ] || [ ${#HASH} -ne 32 ]; then
  echo "[custom-run] ERROR: md5sum failed to generate valid hash" >&2
  exit 1
fi

echo "${USERNAME}:${HASH}" > /opt/bitnami/pgpool/conf/pcp.conf
chmod 600 /opt/bitnami/pgpool/conf/pcp.conf
echo "[custom-run] pcp.conf generated successfully" >&2

# Create pcppass file for PCP client authentication
echo "localhost:9898:${USERNAME}:${PGPOOL_ADMIN_PASSWORD}" > /tmp/.pcppass
echo "*:9898:${USERNAME}:${PGPOOL_ADMIN_PASSWORD}" >> /tmp/.pcppass
chmod 600 /tmp/.pcppass

# Start patroni watcher in background
echo "[custom-run] Starting patroni watcher in background..." >&2
python3 /opt/patroni-watcher.py &
WATCHER_PID=$!
echo "[custom-run] Watcher started with PID: $WATCHER_PID" >&2

# Start pgpool
echo "[custom-run] Starting pgpool..." >&2
info "** Starting Pgpool-II **"

exec "${PGPOOL_BIN_DIR}/pgpool" -n -f "${PGPOOL_CONF_DIR}/pgpool.conf" -F "${PGPOOL_CONF_DIR}/pcp.conf"
EOF

RUN chmod +x /opt/bitnami/scripts/pgpool/run.sh

USER 1001

EXPOSE 5432 9898

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s \
  CMD pg_isready -h localhost -p 5432 -U postgres || exit 1
