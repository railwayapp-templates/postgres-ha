# Dockerfile for PostgreSQL with Patroni HA
#
# Usage:
#   docker build --build-arg POSTGRES_VERSION=17 -t postgres:17-ha .

ARG POSTGRES_VERSION=17

# ============================================================
# Rust builder stage: Compile all Rust binaries
# ============================================================
FROM rust:1.83-bookworm AS rust-builder

WORKDIR /build

# Copy entire workspace (Cargo needs all members to load)
COPY Cargo.toml Cargo.lock ./
COPY common ./common
COPY etcd ./etcd
COPY haproxy ./haproxy
COPY postgres-patroni ./postgres-patroni

# Build release binaries
RUN cargo build --release -p postgres-patroni

# Minimal stage with just binaries (used as cache target in CI)
FROM scratch AS rust-builder-local-output
COPY --from=rust-builder /build/target/release/postgres-wrapper /postgres-wrapper
COPY --from=rust-builder /build/target/release/post-bootstrap /post-bootstrap
COPY --from=rust-builder /build/target/release/patroni-runner /patroni-runner
COPY --from=rust-builder /build/target/release/on-role-change /on-role-change

# ============================================================
# Final stage: PostgreSQL with Patroni HA
# ============================================================
FROM postgres:${POSTGRES_VERSION} AS ha

ARG PATRONI_VERSION=4.1.0

RUN apt-get update && apt-get install -y --no-install-recommends \
    openssl \
    sudo \
    python3 \
    python3-pip \
    curl \
    && rm -rf /var/lib/apt/lists/*

# Allow postgres user to execute specific commands as root
RUN echo "postgres ALL=(root) NOPASSWD: /usr/bin/mkdir, /bin/chown, /bin/chmod, /usr/bin/openssl" > /etc/sudoers.d/postgres

# Install Patroni and dependencies
RUN pip3 install --no-cache-dir --break-system-packages \
    setuptools \
    psycopg2-binary \
    patroni[etcd]==${PATRONI_VERSION}

# Create required directories
RUN mkdir -p /home/postgres /var/run/postgresql /etc/patroni \
    && chown -R postgres:postgres /home/postgres /var/run/postgresql /etc/patroni

# Copy SSL init script and Rust binaries
COPY --chmod=755 postgres-patroni/init-ssl.sh /docker-entrypoint-initdb.d/init-ssl.sh
COPY --from=rust-builder-local-output --chmod=755 /postgres-wrapper /usr/local/bin/postgres-wrapper
COPY --from=rust-builder-local-output --chmod=755 /post-bootstrap /usr/local/bin/post-bootstrap
COPY --from=rust-builder-local-output --chmod=755 /patroni-runner /usr/local/bin/patroni-runner
COPY --from=rust-builder-local-output --chmod=755 /on-role-change /usr/local/bin/on-role-change

EXPOSE 5432 8008

HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=2 \
    CMD if [ "${PATRONI_ENABLED:-false}" = "true" ]; then \
            curl -sf http://localhost:8008/health || exit 1; \
        else \
            pg_isready -U ${POSTGRES_USER:-postgres} || exit 1; \
        fi

ENTRYPOINT ["postgres-wrapper"]
CMD ["postgres", "-p", "5432", "-c", "listen_addresses=*"]
