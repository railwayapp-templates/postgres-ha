FROM haproxy:2.9-alpine

USER root

COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh && \
    chown -R haproxy:haproxy /usr/local/etc/haproxy

USER haproxy

EXPOSE 5432 5433 8404

HEALTHCHECK --interval=10s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8404/stats || exit 1

ENTRYPOINT ["/entrypoint.sh"]
