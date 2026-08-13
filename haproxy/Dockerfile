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

# Minimal stage with just binaries (used as cache target in CI)
FROM scratch AS rust-builder-local-output
COPY --from=rust-builder /build/target/release/haproxy-entrypoint /haproxy-entrypoint

# Stage for CI: pull pre-built binaries (overridden via --build-context rust-binaries=...)
FROM scratch AS rust-binaries
COPY --from=rust-builder-local-output / /

# ============================================================
# Final stage: HAProxy with Rust entrypoint
# ============================================================
FROM haproxy:${HAPROXY_VERSION}-alpine

USER root

# CI passes its content key here (see the build workflow's content gate):
# the value only changes when the repo code, the upstream base, or alpine's
# pending-security-fix set moved, so this layer re-runs exactly when there is
# something to pick up — and stays byte-cached (stable digest) otherwise.
ARG PKG_REFRESH=none

# Install postgresql-client and bash for Railway's Database UI to work via SSH tunnel
# (bash is required because the UI runs: bash +H -c 'psql ...')
RUN echo "refresh key: ${PKG_REFRESH}" \
    && apk upgrade --no-cache \
    && apk add --no-cache postgresql-client bash

# Copy Rust binary
COPY --from=rust-binaries --chmod=755 /haproxy-entrypoint /entrypoint

RUN chown -R haproxy:haproxy /usr/local/etc/haproxy

USER haproxy

EXPOSE 5432 5433 8404

HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8404/stats || exit 1

ENTRYPOINT ["/entrypoint"]
