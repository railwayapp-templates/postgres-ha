FROM railwayapp/pgpool:4

USER root

ENV PGPOOL_ENABLE_POOL_HBA=yes

RUN apt-get update && apt-get install -y python3 python3-requests \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

COPY patroni-watcher.py /opt/patroni-watcher.py
COPY pool_hba.conf /opt/bitnami/pgpool/conf/pool_hba.conf
COPY run.sh /opt/bitnami/scripts/pgpool/run.sh
RUN chmod +x /opt/patroni-watcher.py /opt/bitnami/scripts/pgpool/run.sh

USER 1001

EXPOSE 5432 9898

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s \
  CMD pg_isready -h localhost -p 5432 -U postgres || exit 1
