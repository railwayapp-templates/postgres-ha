FROM railwayapp/pgpool:4

USER root

# Force rebuild 2025-11-27-05-40

# Don't try to use ENV vars to disable health checks - Bitnami ignores them
# We'll modify pgpool.conf directly before starting pgpool
ENV PGPOOL_ENABLE_POOL_HBA=yes

# Install Python and dependencies for patroni-watcher
RUN apt-get update && apt-get install -y \
    python3 \
    python3-pip \
    python3-requests \
    netcat-openbsd \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

# Don't copy custom config - let Bitnami generate from env vars
COPY pool_hba.conf /opt/bitnami/pgpool/conf/pool_hba.conf
COPY patroni-watcher.py /usr/local/bin/patroni-watcher.py
RUN chmod +x /usr/local/bin/patroni-watcher.py

# Backup original run.sh and replace it with our custom version
# This ensures config generation happens, then we modify it
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

# Restore our custom pool_hba.conf (write to etc dir, not conf dir)
echo "[custom-run] Restoring custom pool_hba.conf..." >&2
mkdir -p /opt/bitnami/pgpool/etc
cat > /opt/bitnami/pgpool/etc/pool_hba.conf <<'POOL_HBA_EOF'
local   all         all                               trust
host    all         all         0.0.0.0/0             md5
host    all         all         ::0/0                 md5
POOL_HBA_EOF

# Now modify the generated configs
PGPOOL_CONF="/opt/bitnami/pgpool/conf/pgpool.conf"

if [ -f "$PGPOOL_CONF" ]; then
  echo "[custom-run] Modifying pgpool.conf to disable health checks..." >&2

  # Disable health check by setting period to 0
  sed -i 's/^health_check_period = .*/health_check_period = 0/' "$PGPOOL_CONF"

  # Disable sr_check by setting period to 0
  sed -i 's/^sr_check_period = .*/sr_check_period = 0/' "$PGPOOL_CONF"

  echo "[custom-run] Config modifications complete" >&2
  echo "[custom-run] Health check period: $(grep '^health_check_period' $PGPOOL_CONF)" >&2
  echo "[custom-run] SR check period: $(grep '^sr_check_period' $PGPOOL_CONF)" >&2
else
  echo "[custom-run] ERROR: pgpool.conf not found after setup" >&2
  ls -la /opt/bitnami/pgpool/conf/ >&2 || true
  exit 1
fi

# Configure PCP admin user - create pcp.conf since setup.sh doesn't create it
PCP_CONF="/opt/bitnami/pgpool/conf/pcp.conf"
if [ -n "${PGPOOL_ADMIN_USERNAME}" ] && [ -n "${PGPOOL_ADMIN_PASSWORD}" ]; then
  echo "[custom-run] Creating pcp.conf with admin user..." >&2
  HASH=$(echo -n "${PGPOOL_ADMIN_PASSWORD}" | md5sum | cut -d' ' -f1)
  echo "${PGPOOL_ADMIN_USERNAME}:md5${HASH}" > "$PCP_CONF"
  chmod 644 "$PCP_CONF"
  echo "[custom-run] pcp.conf created" >&2
else
  echo "[custom-run] WARNING: PGPOOL_ADMIN_USERNAME or PGPOOL_ADMIN_PASSWORD not set, pcp.conf not created" >&2
fi

# Start patroni-watcher in background
echo "[custom-run] Starting patroni-watcher..." >&2
python3 /usr/local/bin/patroni-watcher.py &
WATCHER_PID=$!

# Trap to kill watcher on exit
cleanup() {
  echo "[custom-run] Shutting down watcher PID $WATCHER_PID..." >&2
  kill $WATCHER_PID 2>/dev/null || true
}
trap cleanup SIGTERM SIGINT EXIT

# Start pgpool (from original Bitnami run.sh)
echo "[custom-run] Starting pgpool..." >&2
info "** Starting Pgpool-II **"

exec "${PGPOOL_BIN_DIR}/pgpool" -n -f "${PGPOOL_CONF_DIR}/pgpool.conf" -F "${PGPOOL_CONF_DIR}/pcp.conf"
EOF

RUN chmod +x /opt/bitnami/scripts/pgpool/run.sh

USER 1001

EXPOSE 5432 9999

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s \
  CMD pgpool -h localhost -p 5432 -c "SELECT 1" || exit 1
