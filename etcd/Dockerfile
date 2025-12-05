# Use alpine to get shell utilities
FROM alpine:latest AS tools

FROM quay.io/coreos/etcd:v3.5.16

# Copy shell and basic utilities from alpine
COPY --from=tools /bin/sh /bin/sh
COPY --from=tools /bin/mkdir /bin/mkdir
COPY --from=tools /bin/chmod /bin/chmod
COPY --from=tools /lib/ld-musl-*.so.1 /lib/
COPY --from=tools /lib/libc.musl-*.so.1 /lib/

ENV ETCD_ENABLE_V2=true
ENV ETCD_DATA_DIR=/etcd-data/data

COPY entrypoint.sh /entrypoint.sh

EXPOSE 2379 2380

HEALTHCHECK --interval=10s --timeout=5s --start-period=10s \
  CMD etcdctl endpoint health || exit 1

ENTRYPOINT ["/entrypoint.sh"]
