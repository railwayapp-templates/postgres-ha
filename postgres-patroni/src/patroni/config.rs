//! Patroni configuration from environment variables

use crate::{pgdata, ssl_dir};
use anyhow::Result;
use common::ConfigExt;
use std::env;

/// Configuration for Patroni runner
pub struct Config {
    pub scope: String,
    pub name: String,
    pub connect_address: String,
    pub etcd_hosts: String,
    pub superuser: String,
    pub superuser_pass: String,
    pub repl_user: String,
    pub repl_pass: String,
    pub app_user: String,
    pub app_pass: String,
    pub app_db: String,
    pub data_dir: String,
    pub certs_dir: String,
    pub ttl: String,
    pub loop_wait: String,
    pub retry_timeout: String,
    pub health_check_interval: u64,
    pub health_check_timeout: u64,
    pub max_failures: u32,
    pub startup_grace_period: u64,
    /// Maximum time to wait for Patroni to become healthy during startup.
    /// If exceeded, we exit(1) to trigger container restart and recovery.
    /// Must be >= startup_grace_period. Default: 300 seconds (5 minutes).
    pub max_startup_timeout: u64,
    pub adopt_existing_data: bool,
    /// If true and node has no existing data, wait for cluster leader in etcd
    /// before starting Patroni. Used during HA conversion to prevent replicas
    /// from racing with the primary for leadership.
    pub wait_for_leader: bool,
    /// If true, enable synchronous replication mode. Ensures at least one
    /// replica has received the data before a write is acknowledged.
    pub synchronous_mode: bool,
    /// This cluster's own archive bucket. Tool-agnostic name read from
    /// `WAL_ARCHIVE_BUCKET`; patroni-runner translates it (and the matching
    /// `WAL_ARCHIVE_KEY` / `_SECRET` / `_REGION` / `_ENDPOINT` / `_PATH`)
    /// into pgBackRest's native `PGBACKREST_REPO1_S3_*` (or `_REPO2_*` in
    /// dual-repo mode) so pgBackRest reads them natively. When set, Patroni
    /// DCS gets `archive_mode=on` + the archive-push wrapper as
    /// `archive_command` + `archive_timeout`, and patroni-runner renders
    /// `/etc/pgbackrest/pgbackrest.conf`. Only the current Patroni leader
    /// fires archive_command; standbys carry the same config + binary so
    /// promotion instantly enables archiving. pgBackRest runs in async mode
    /// with `archive-push-queue-max=5GiB` — when the queue trips, WAL is
    /// dropped and Postgres keeps running rather than halting on a full
    /// `pg_wal`. When unset, Patroni config is generated as it always was.
    pub wal_archive_bucket: Option<String>,
    /// Source cluster's bucket on a PITR-restored volume, read by
    /// `archive-get` during recovery only. Translated by patroni-runner from
    /// `WAL_RECOVER_FROM_*` to pgBackRest's `PGBACKREST_REPO1_S3_*`. Under
    /// the new-service restore design (per RFC) HA restore creates a fresh
    /// single-node service rather than restoring in place, so this is rare
    /// in HA but kept for symmetry with postgres-ssl.
    pub wal_recover_from_bucket: Option<String>,
    /// Target timestamp for point-in-time recovery (ISO 8601). When set on a
    /// restored volume, the runner stages `recovery.signal` + recovery
    /// settings before Patroni starts Postgres.
    pub pitr_target_time: Option<String>,
    /// Target xid for point-in-time recovery (decimal transaction ID).
    /// When set, takes priority over `pitr_target_time` because xid is the
    /// only target type postgres can match exactly on an idle source —
    /// recovery_target_time requires a commit record AFTER target before
    /// firing recovery_target_action=promote, and on an idle DB no such
    /// record exists. The PITR picker (mono's createServiceFromPITR
    /// mutation) sets this when it clamped target down to the source's
    /// `lastCommittedTxnAt`. Mirrors postgres-ssl PR #63.
    pub pitr_target_xid: Option<String>,
    /// `archive_timeout` written into Patroni's bootstrap.dcs and asserted by
    /// the DCS reconciler when archiving is enabled. Default 60s. Operators
    /// raise it on idle DBs to cut S3 cost or lower it for tighter RPO.
    pub archive_timeout_secs: i64,
}

impl Config {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self> {
        let name = String::env_required("PATRONI_NAME")?;
        let connect_address = String::env_required("RAILWAY_PRIVATE_DOMAIN")?;
        let etcd_hosts = String::env_required("PATRONI_ETCD3_HOSTS")?;

        Ok(Self {
            scope: String::env_or("PATRONI_SCOPE", "railway-pg-ha"),
            name,
            connect_address,
            etcd_hosts,
            superuser: String::env_or("PATRONI_SUPERUSER_USERNAME", "postgres"),
            superuser_pass: env::var("PATRONI_SUPERUSER_PASSWORD").unwrap_or_default(),
            repl_user: String::env_or("PATRONI_REPLICATION_USERNAME", "replicator"),
            repl_pass: env::var("PATRONI_REPLICATION_PASSWORD").unwrap_or_default(),
            app_user: String::env_or("POSTGRES_USER", "postgres"),
            app_pass: env::var("POSTGRES_PASSWORD").unwrap_or_default(),
            app_db: env::var("POSTGRES_DB")
                .or_else(|_| env::var("PGDATABASE"))
                .unwrap_or_else(|_| "railway".to_string()),
            data_dir: pgdata(),
            certs_dir: ssl_dir(),
            // Constraint: loop_wait + 2*retry_timeout <= ttl
            // 10 + 2*17 = 44 <= 45 (1s buffer)
            ttl: String::env_or("PATRONI_TTL", "45"),
            loop_wait: String::env_or("PATRONI_LOOP_WAIT", "10"),
            retry_timeout: String::env_or("PATRONI_RETRY_TIMEOUT", "17"),
            health_check_interval: u64::env_parse("PATRONI_HEALTH_CHECK_INTERVAL", 5),
            health_check_timeout: u64::env_parse("PATRONI_HEALTH_CHECK_TIMEOUT", 5),
            max_failures: u32::env_parse("PATRONI_MAX_HEALTH_FAILURES", 3),
            startup_grace_period: u64::env_parse("PATRONI_STARTUP_GRACE_PERIOD", 60),
            max_startup_timeout: u64::env_parse("PATRONI_MAX_STARTUP_TIMEOUT", 300),
            adopt_existing_data: bool::env_parse("PATRONI_ADOPT_EXISTING_DATA", false),
            wait_for_leader: bool::env_parse("PATRONI_WAIT_FOR_LEADER", false),
            synchronous_mode: bool::env_parse("PATRONI_SYNCHRONOUS_MODE", false),
            wal_archive_bucket: env::var("WAL_ARCHIVE_BUCKET")
                .ok()
                .filter(|s| !s.is_empty()),
            wal_recover_from_bucket: env::var("WAL_RECOVER_FROM_BUCKET")
                .ok()
                .filter(|s| !s.is_empty()),
            pitr_target_time: env::var("POSTGRES_RECOVERY_TARGET_TIME")
                .ok()
                .filter(|s| !s.is_empty()),
            pitr_target_xid: env::var("POSTGRES_RECOVERY_TARGET_XID")
                .ok()
                .filter(|s| !s.is_empty()),
            archive_timeout_secs: env::var("POSTGRES_ARCHIVE_TIMEOUT")
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .filter(|v| *v > 0)
                .unwrap_or(60),
        })
    }
}
