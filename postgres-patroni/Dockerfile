FROM postgres:17

ARG PATRONI_VERSION=4.0.4

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssl \
    sudo \
    python3 \
    python3-pip \
    curl \
    supervisor \
    && rm -rf /var/lib/apt/lists/*

# Install setuptools first (required for ydiff dependency)
RUN pip3 install --no-cache-dir --break-system-packages setuptools \
    && pip3 install --no-cache-dir --break-system-packages \
    patroni[etcd]==${PATRONI_VERSION}

# Allow the postgres user to execute certain commands as root without a password
RUN echo "postgres ALL=(root) NOPASSWD: /usr/bin/mkdir, /bin/chown, /bin/chmod, /usr/bin/openssl" > /etc/sudoers.d/postgres

RUN mkdir -p /home/postgres /var/run/postgresql \
    && chown -R postgres:postgres /home/postgres /var/run/postgresql

COPY post_bootstrap.sh /post_bootstrap.sh
COPY wrapper.sh /usr/local/bin/wrapper.sh
# Single location for init-ssl.sh - matches postgres-ssl template
# Auto-runs during initdb (standalone), called by post_bootstrap.sh (Patroni)
COPY --chmod=755 init-ssl.sh /docker-entrypoint-initdb.d/init-ssl.sh

# Phase 2: Supervisord and watchdog for split-brain prevention (RFC-007)
COPY supervisord.conf /etc/supervisor/conf.d/patroni.conf
COPY patroni-runner.sh /usr/local/bin/patroni-runner.sh
COPY patroni-watchdog.sh /usr/local/bin/patroni-watchdog.sh
COPY supervisor-exit-handler.sh /usr/local/bin/supervisor-exit-handler.sh

RUN chmod +x /post_bootstrap.sh /usr/local/bin/wrapper.sh \
    /usr/local/bin/patroni-runner.sh /usr/local/bin/patroni-watchdog.sh \
    /usr/local/bin/supervisor-exit-handler.sh

EXPOSE 5432 8008

# Aggressive health check for faster split-brain detection (Phase 1)
# - 5s interval with 2 retries = ~15s max detection time
HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=2 \
    CMD if [ "${PATRONI_ENABLED:-false}" = "true" ]; then \
            curl -sf http://localhost:8008/health || exit 1; \
        else \
            pg_isready -U ${POSTGRES_USER:-postgres} || exit 1; \
        fi

ENTRYPOINT ["wrapper.sh"]
CMD ["postgres"]
