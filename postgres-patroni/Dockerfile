# Multi-stage Dockerfile for PostgreSQL with optional Patroni HA
#
# Build variants:
#   slim (default) - Matches postgres-ssl, ~400MB
#   ha             - Adds Patroni + supervisord, ~550MB
#
# Usage:
#   docker build --build-arg POSTGRES_VERSION=17 --build-arg BUILD_VARIANT=slim -t postgres:17 .
#   docker build --build-arg POSTGRES_VERSION=17 --build-arg BUILD_VARIANT=ha -t postgres:17-ha .

ARG POSTGRES_VERSION=17
ARG BUILD_VARIANT=slim

# ============================================================
# Base stage: Common to both slim and HA
# ============================================================
FROM postgres:${POSTGRES_VERSION} AS base

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssl \
    sudo \
    && rm -rf /var/lib/apt/lists/*

# Allow postgres user to execute specific commands as root
RUN echo "postgres ALL=(root) NOPASSWD: /usr/bin/mkdir, /bin/chown, /bin/chmod, /usr/bin/openssl" > /etc/sudoers.d/postgres

# ============================================================
# Slim stage: Standalone PostgreSQL with SSL (matches postgres-ssl)
# ============================================================
FROM base AS slim

COPY --chmod=755 init-ssl.sh /docker-entrypoint-initdb.d/init-ssl.sh
COPY --chmod=755 wrapper.sh /usr/local/bin/wrapper.sh

EXPOSE 5432

HEALTHCHECK --interval=30s --timeout=3s --start-period=30s --retries=3 \
    CMD pg_isready -U ${POSTGRES_USER:-postgres} || exit 1

ENTRYPOINT ["wrapper.sh"]
CMD ["postgres", "-p", "5432", "-c", "listen_addresses=*"]

# ============================================================
# HA stage: Patroni + supervisord for high availability
# ============================================================
FROM base AS ha

ARG PATRONI_VERSION=4.0.4

# Install HA dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 \
    python3-pip \
    curl \
    supervisor \
    && rm -rf /var/lib/apt/lists/*

# Install setuptools first (needed for ydiff's legacy setup.py)
RUN pip3 install --no-cache-dir --break-system-packages setuptools

# Install Patroni and dependencies
RUN pip3 install --no-cache-dir --break-system-packages \
    psycopg2-binary \
    patroni[etcd]==${PATRONI_VERSION}

# Create required directories
RUN mkdir -p /home/postgres /var/run/postgresql \
    && chown -R postgres:postgres /home/postgres /var/run/postgresql

# Copy all scripts
COPY --chmod=755 init-ssl.sh /docker-entrypoint-initdb.d/init-ssl.sh
COPY --chmod=755 wrapper.sh /usr/local/bin/wrapper.sh
COPY --chmod=755 post_bootstrap.sh /post_bootstrap.sh
COPY --chmod=755 patroni-runner.sh /usr/local/bin/patroni-runner.sh
COPY --chmod=755 patroni-watchdog.sh /usr/local/bin/patroni-watchdog.sh
COPY --chmod=755 supervisor-exit-handler.sh /usr/local/bin/supervisor-exit-handler.sh
COPY supervisord.conf /etc/supervisor/conf.d/patroni.conf

EXPOSE 5432 8008

# Aggressive health check for faster split-brain detection
HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=2 \
    CMD if [ "${PATRONI_ENABLED:-false}" = "true" ]; then \
            curl -sf http://localhost:8008/health || exit 1; \
        else \
            pg_isready -U ${POSTGRES_USER:-postgres} || exit 1; \
        fi

ENTRYPOINT ["wrapper.sh"]
CMD ["postgres", "-p", "5432", "-c", "listen_addresses=*"]
