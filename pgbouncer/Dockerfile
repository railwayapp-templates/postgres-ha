FROM alpine:3.19

RUN apk add --no-cache pgbouncer openssl

# Create directories with proper ownership
RUN mkdir -p /etc/pgbouncer /var/run/pgbouncer /var/log/pgbouncer /etc/pgbouncer/certs && \
    chown -R nobody:nobody /etc/pgbouncer /var/run/pgbouncer /var/log/pgbouncer

COPY pgbouncer/pgbouncer.ini /etc/pgbouncer/pgbouncer.ini
COPY pgbouncer/entrypoint.sh /entrypoint.sh

RUN chmod +x /entrypoint.sh && \
    chown nobody:nobody /etc/pgbouncer/pgbouncer.ini /entrypoint.sh

USER nobody

EXPOSE 6432

HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=3 \
    CMD pg_isready -h 127.0.0.1 -p 6432 || exit 1

ENTRYPOINT ["/entrypoint.sh"]
