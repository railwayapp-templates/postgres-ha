# Use alpine to get shell utilities
FROM alpine:latest AS tools

FROM quay.io/coreos/etcd:v3.5.16

# Copy shell and utilities from alpine for our entrypoint script
COPY --from=tools /bin/sh /bin/sh
COPY --from=tools /bin/mkdir /bin/mkdir
COPY --from=tools /bin/chmod /bin/chmod
COPY --from=tools /bin/sleep /bin/sleep
COPY --from=tools /bin/kill /bin/kill
COPY --from=tools /bin/date /bin/date
COPY --from=tools /usr/bin/seq /usr/bin/seq
COPY --from=tools /lib/ld-musl-*.so.1 /lib/
COPY --from=tools /lib/libc.musl-*.so.1 /lib/

# Copy entrypoint script
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

RUN mkdir -p /etcd-data && chmod 700 /etcd-data

ENV ETCD_DATA_DIR=/etcd-data
# Retry settings (5 min total with defaults: 60 retries * 5s delay)
ENV ETCD_MAX_RETRIES=60
ENV ETCD_RETRY_DELAY=5
ENV ETCD_STABILIZE_TIME=30

EXPOSE 2379 2380

HEALTHCHECK --interval=10s --timeout=5s --start-period=60s --retries=3 \
  CMD etcdctl endpoint health --endpoints=http://127.0.0.1:2379 || exit 1

ENTRYPOINT ["/entrypoint.sh"]
