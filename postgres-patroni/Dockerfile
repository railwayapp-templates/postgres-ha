# Dockerfile for PostgreSQL with Patroni HA
#
# Usage:
#   docker build --build-arg POSTGRES_VERSION=17 -t postgres:17-ha .
#
# For CI with pre-built binaries (skips Rust compilation):
#   docker build --build-arg POSTGRES_VERSION=17 \
#     --build-context rust-binaries=docker-image://ghcr.io/org/postgres-rust-binaries:latest \
#     -t postgres:17-ha .

ARG POSTGRES_VERSION=18

# Exact upstream tag to build FROM. Defaults to the (floating) major, so
# every existing caller is unchanged; the per-minor CI builds pass e.g.
# 16.12 so an already-published minor tag keeps being rebuilt from exactly
# that upstream patch while still receiving new wrapper binaries and
# refreshed apt/pip layers. POSTGRES_VERSION stays the MAJOR either way —
# apt package names (postgresql-<major>-pgvector) are per-major only.
ARG POSTGRES_IMAGE_TAG=${POSTGRES_VERSION}

# ============================================================
# Rust builder stage: Compile all Rust binaries (local builds)
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

# Stage for CI: pull pre-built binaries (overridden via --build-context rust-binaries=...)
FROM scratch AS rust-binaries
COPY --from=rust-builder-local-output / /

# ============================================================
# Final stage: PostgreSQL with Patroni HA
# ============================================================
FROM postgres:${POSTGRES_IMAGE_TAG} AS ha

ARG PATRONI_VERSION=4.1.0

ARG POSTGRES_VERSION
# CI passes its content key here (see the build workflow's content gate):
# the value only changes when the repo code, the upstream base, or Debian's
# pending-security-fix set moved, so this layer re-runs exactly when there is
# something to pick up — and stays byte-cached (stable digest) otherwise.
# Upstream never re-mints a superseded patch tag, so the apt-get upgrade
# below is the only way OS security updates reach a pinned old minor.
ARG APT_REFRESH=none
RUN echo "refresh key: ${APT_REFRESH}" \
    && apt-get update && apt-get upgrade -y \
    && apt-get install -y --no-install-recommends \
    openssl \
    sudo \
    python3 \
    python3-pip \
    curl \
    pgbackrest \
    postgresql-${POSTGRES_VERSION}-pgvector \
    && rm -rf /var/lib/apt/lists/*

# Allow postgres user to execute specific commands as root
RUN echo "postgres ALL=(root) NOPASSWD: /usr/bin/mkdir, /bin/chown, /bin/chmod, /usr/bin/openssl" > /etc/sudoers.d/postgres

# Install Patroni and dependencies
RUN pip3 install --no-cache-dir --break-system-packages \
    setuptools \
    psycopg2-binary \
    patroni[etcd]==${PATRONI_VERSION}

# Create required directories. The pgBackRest spool lives on the data
# volume ($PGDATA/pgbackrest-spool) so segments staged but not yet pushed
# survive container restarts; patroni-runner creates it at runtime when
# WAL_ARCHIVE_BUCKET is set, so nothing to pre-create here.
RUN mkdir -p /home/postgres /var/run/postgresql /etc/patroni /etc/pgbackrest \
    && chown -R postgres:postgres /home/postgres /var/run/postgresql /etc/patroni /etc/pgbackrest

# Copy SSL init script, archive-push wrapper, and Rust binaries
COPY --chmod=755 postgres-patroni/init-ssl.sh /docker-entrypoint-initdb.d/init-ssl.sh
COPY --chmod=755 postgres-patroni/pgbackrest-archive-push-wrapper.sh /usr/local/bin/pgbackrest-archive-push-wrapper.sh
COPY --chmod=755 postgres-patroni/pgbackrest-archive-get-wrapper.sh /usr/local/bin/pgbackrest-archive-get-wrapper.sh
COPY --chmod=755 postgres-patroni/pgbackrest-replica-restore-wrapper.sh /usr/local/bin/pgbackrest-replica-restore-wrapper.sh
COPY --from=rust-binaries --chmod=755 /postgres-wrapper /usr/local/bin/postgres-wrapper
COPY --from=rust-binaries --chmod=755 /post-bootstrap /usr/local/bin/post-bootstrap
COPY --from=rust-binaries --chmod=755 /patroni-runner /usr/local/bin/patroni-runner
COPY --from=rust-binaries --chmod=755 /on-role-change /usr/local/bin/on-role-change

EXPOSE 5432 8008

HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=2 \
    CMD if [ "${PATRONI_ENABLED:-false}" = "true" ]; then \
            curl -sf http://localhost:8008/health || exit 1; \
        else \
            pg_isready -U ${POSTGRES_USER:-postgres} || exit 1; \
        fi

ENTRYPOINT ["postgres-wrapper"]
CMD ["postgres", "-p", "5432", "-c", "listen_addresses=*"]
