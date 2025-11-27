FROM railwayapp/pgpool:4

USER root

# Force rebuild 2025-11-27-01-16

# Disable pgpool health checks AND sr_check - let patroni-watcher control backends
ENV PGPOOL_ENABLE_HEALTH_CHECK=no
ENV PGPOOL_ENABLE_POOL_HBA=on
ENV PGPOOL_SR_CHECK_PERIOD=0

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

# Create wrapper entrypoint to fix missing env var and start watcher
COPY <<'EOF' /entrypoint-wrapper.sh
#!/bin/bash
set -e

# Workaround: Railway sometimes doesn't resolve PGPOOL_HEALTH_CHECK_PASSWORD
# Copy from PGPOOL_POSTGRES_PASSWORD if empty
if [ -z "${PGPOOL_HEALTH_CHECK_PASSWORD}" ] && [ -n "${PGPOOL_POSTGRES_PASSWORD}" ]; then
  echo "PGPOOL_HEALTH_CHECK_PASSWORD is empty, copying from PGPOOL_POSTGRES_PASSWORD"
  export PGPOOL_HEALTH_CHECK_PASSWORD="${PGPOOL_POSTGRES_PASSWORD}"
fi

# Background task to add admin user to pcp.conf after it's created
if [ -n "${PGPOOL_ADMIN_USERNAME}" ] && [ -n "${PGPOOL_ADMIN_PASSWORD}" ]; then
  (
    echo "[entrypoint] Will add ${PGPOOL_ADMIN_USERNAME} to pcp.conf after it's created..." >&2

    # Wait for pcp.conf to be created by Bitnami's initialization
    for i in {1..60}; do
      if [ -f /opt/bitnami/pgpool/conf/pcp.conf ]; then
        echo "[entrypoint] pcp.conf found, adding admin user..." >&2

        # Generate MD5 hash for PCP (format: md5<md5(password+username)>)
        HASH=$(echo -n "${PGPOOL_ADMIN_PASSWORD}${PGPOOL_ADMIN_USERNAME}" | md5sum | cut -d' ' -f1)
        echo "${PGPOOL_ADMIN_USERNAME}:md5${HASH}" >> /opt/bitnami/pgpool/conf/pcp.conf

        echo "[entrypoint] Admin user ${PGPOOL_ADMIN_USERNAME} added to pcp.conf with hash md5${HASH}" >&2
        echo "[entrypoint] pcp.conf contents:" >&2
        cat /opt/bitnami/pgpool/conf/pcp.conf >&2
        break
      fi
      echo "[entrypoint] Waiting for pcp.conf (attempt $i)..." >&2
      sleep 1
    done
  ) &
fi

# Start patroni-watcher in background (it will wait for pgpool to be ready)
python3 /usr/local/bin/patroni-watcher.py &
WATCHER_PID=$!

# Trap to kill watcher on exit
cleanup() {
  echo "[entrypoint] Shutting down, killing watcher..."
  kill $WATCHER_PID 2>/dev/null || true
}
trap cleanup SIGTERM SIGINT EXIT

# Call original Bitnami entrypoint
exec /opt/bitnami/scripts/pgpool/entrypoint.sh "$@"
EOF

RUN chmod +x /entrypoint-wrapper.sh

USER 1001

ENTRYPOINT ["/entrypoint-wrapper.sh"]
CMD ["/opt/bitnami/scripts/pgpool/run.sh"]

EXPOSE 5432 9999

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s \
  CMD pgpool -h localhost -p 5432 -c "SELECT 1" || exit 1
