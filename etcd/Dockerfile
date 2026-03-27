ARG ETCD_VERSION=3.6.6

# ============================================================
# Rust builder stage: Compile etcd-entrypoint binary (musl)
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

# Build statically linked release binary
RUN cargo build --release -p etcd-entrypoint

# Minimal stage with just binaries (used as cache target in CI)
FROM scratch AS rust-builder-local-output
COPY --from=rust-builder /build/target/release/etcd-entrypoint /etcd-entrypoint

# Stage for CI: pull pre-built binaries (overridden via --build-context rust-binaries=...)
FROM scratch AS rust-binaries
COPY --from=rust-builder-local-output / /

# ============================================================
# Final stage: etcd with Rust entrypoint
# ============================================================
FROM quay.io/coreos/etcd:v${ETCD_VERSION}

# Copy Rust binary (statically linked, no glibc needed)
COPY --from=rust-binaries --chmod=755 /etcd-entrypoint /entrypoint

ENV ETCD_DATA_DIR=/var/lib/etcd
# Enable gRPC gateway for Patroni etcd3 client compatibility
ENV ETCD_ENABLE_GRPC_GATEWAY=true
# Retry settings (5 min total with defaults: 60 retries * 5s delay)
ENV ETCD_MAX_RETRIES=60
ENV ETCD_RETRY_DELAY=5
ENV ETCD_STABILIZE_TIME=30

EXPOSE 2379 2380

HEALTHCHECK --interval=10s --timeout=5s --start-period=60s --retries=3 \
  CMD etcdctl endpoint health --endpoints=127.0.0.1:2379 || exit 1

ENTRYPOINT ["/entrypoint"]
