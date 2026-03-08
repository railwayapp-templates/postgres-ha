ARG HAPROXY_VERSION=3.2

# ============================================================
# Rust builder stage: Compile haproxy-entrypoint binary
# Build context must be repo root (.) to access workspace
# ============================================================
FROM rust:1.83-alpine AS rust-builder

RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static

WORKDIR /build

# Copy entire workspace (Cargo needs all members to load)
COPY Cargo.toml Cargo.lock ./
COPY common ./common
COPY etcd ./etcd
COPY haproxy ./haproxy
COPY postgres-patroni ./postgres-patroni

# Build release binary (statically linked with musl)
RUN cargo build --release -p haproxy-entrypoint

# ============================================================
# Final stage: HAProxy with Rust entrypoint
# ============================================================
FROM haproxy:${HAPROXY_VERSION}-alpine

USER root

# Install postgresql-client and bash for Railway's Database UI to work via SSH tunnel
# (bash is required because the UI runs: bash +H -c 'psql ...')
RUN apk add --no-cache postgresql-client bash

# Copy Rust binary
COPY --from=rust-builder --chmod=755 /build/target/release/haproxy-entrypoint /entrypoint

RUN chown -R haproxy:haproxy /usr/local/etc/haproxy

USER haproxy

EXPOSE 5432 5433 8404

HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8404/stats || exit 1

ENTRYPOINT ["/entrypoint"]
