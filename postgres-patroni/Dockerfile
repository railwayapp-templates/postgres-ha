FROM postgres:17

ARG PATRONI_VERSION=4.0.4

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssl \
    sudo \
    python3 \
    python3-pip \
    python3-psycopg2 \
    python3-yaml \
    python3-requests \
    curl \
    && rm -rf /var/lib/apt/lists/*

RUN pip3 install --no-cache-dir --break-system-packages \
    patroni[etcd]==${PATRONI_VERSION}

# Allow postgres user to manage Railway volume permissions
RUN echo "postgres ALL=(root) NOPASSWD: /bin/mkdir, /bin/chown, /bin/chmod, /usr/bin/openssl" > /etc/sudoers.d/postgres

RUN mkdir -p /home/postgres /var/run/postgresql \
    && chown -R postgres:postgres /home/postgres /var/run/postgresql

COPY docker-entrypoint.sh /docker-entrypoint.sh
COPY post_bootstrap.sh /post_bootstrap.sh
COPY wrapper.sh /usr/local/bin/wrapper.sh
RUN chmod +x /docker-entrypoint.sh /post_bootstrap.sh /usr/local/bin/wrapper.sh

EXPOSE 5432 8008

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=3 \
    CMD if [ "${PATRONI_ENABLED:-false}" = "true" ]; then \
            curl -f http://localhost:8008/health || exit 1; \
        else \
            pg_isready -U ${POSTGRES_USER:-postgres} || exit 1; \
        fi

ENTRYPOINT ["wrapper.sh"]
CMD ["postgres"]
