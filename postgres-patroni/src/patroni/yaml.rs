//! Patroni YAML configuration generation

use super::Config;
use anyhow::Result;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tracing::info;

/// Generate Patroni YAML configuration.
///
/// `wal_level` is the level seeded into `bootstrap.dcs.postgresql.parameters`.
/// Pass `logical` to preserve logical replication on an adopted cluster that
/// was already running it; otherwise `replica` (the HA default). The caller
/// resolves this from the on-disk cluster via `pgbackrest::read_wal_level`.
pub fn generate_patroni_config(config: &Config, wal_level: &str) -> String {
    // Opt-in pgBackRest archiving: adds archive_mode/archive_command/
    // archive_timeout to the cluster's postgresql.parameters when
    // WAL_ARCHIVE_BUCKET is set.
    //
    // archive_mode=on (industry-mainstream Patroni + WAL archiving setting):
    // every node carries the same config, but only the current Patroni
    // leader actually fires archive_command. Standbys hold the pgBackRest
    // binary and config so promotion instantly enables archiving with no
    // config change. Residual failover RPO under `on` is archive_timeout
    // (60s) plus failover-detection time.
    //
    // archive_command points at the never-halt wrapper rather than calling
    // pgbackrest directly. The wrapper measures pg_wal/ on archive failures
    // and drops segments past WAL_DROP_THRESHOLD_MB (default 5GiB, matching
    // pgBackRest's own archive-push-queue-max=5GiB in
    // /etc/pgbackrest/pgbackrest.conf, rendered by patroni-runner) to keep
    // Postgres running; only the two known no-recovery-possible errors
    // (bad creds, deleted bucket) bypass the threshold and drop immediately.
    // PITR window truncates on either path; DB stays up. This is the
    // explicit architectural reason we picked pgBackRest over wal-g.
    // track_commit_timestamp lets pg_last_committed_xact() return the
    // wall-clock time of the last commit. The PITR picker uses that as its
    // upper bound: `recovery_target_time` only matches commit record
    // timestamps, so on an idle DB the archive head keeps ticking with empty
    // WAL while the latest reachable target stays pinned at the last commit.
    // Without this GUC the picker falls back to lastArchivedAt and the user
    // can pick an unreachable target. Mirrors postgres-ssl PRs #52 + #58.
    let pgbackrest_archive_params = if config.wal_archive_bucket.is_some() {
        format!(
            "        archive_mode: \"on\"\n        archive_command: \"/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p\"\n        archive_timeout: {}\n        track_commit_timestamp: \"on\"\n",
            config.archive_timeout_secs,
        )
    } else {
        String::new()
    };

    // archive_mode and track_commit_timestamp are PGC_POSTMASTER — they can
    // only be applied by restarting PostgreSQL, not via reload. Bootstrap.dcs
    // only seeds DCS on the very first cluster init; on existing clusters,
    // DCS might start empty (etcd reset, timing race with the DCS reconciler,
    // or first-time PITR enable on a running cluster). To guarantee Postgres
    // always starts with these params active — regardless of DCS state — also
    // place them in Patroni's local postgresql.parameters section. DCS takes
    // priority when both are present and agree; if DCS is empty at startup
    // the local value fills the gap and avoids the reload→pending_restart trap.
    let pgbackrest_local_params = if config.wal_archive_bucket.is_some() {
        "    archive_mode: \"on\"\n    track_commit_timestamp: \"on\"\n".to_string()
    } else {
        String::new()
    };

    // Throttle replica creation so a re-seed can never monopolize the
    // leader's volume: pg_basebackup is a single sequential stream read
    // directly off the live leader, and unthrottled it runs at the volume's
    // throughput ceiling, starving production queries for the entire copy.
    // The 20M default is well under the observed per-volume read ceiling,
    // leaving production the dominant share while a re-seed runs.
    // checkpoint: fast skips the spread-checkpoint wait (up to
    // checkpoint_timeout of apparent hang before the first byte lands).
    // config.basebackup_max_rate is the validated POSTGRES_BASEBACKUP_MAX_RATE
    // override for oversized emergencies, resolved in Config::from_env.
    format!(
        r#"scope: {scope}
name: {name}

restapi:
  listen: ":::8008"
  connect_address: {connect_address}:8008

etcd3:
  hosts: {etcd_hosts}

bootstrap:
  dcs:
    ttl: {ttl}
    loop_wait: {loop_wait}
    retry_timeout: {retry_timeout}
    maximum_lag_on_failover: 1048576
    failsafe_mode: true
    synchronous_mode: {synchronous_mode}
    postgresql:
      use_pg_rewind: true
      use_slots: true
      remove_data_directory_on_rewind_failure: true
      remove_data_directory_on_diverged_timelines: true
      parameters:
        wal_level: {wal_level}
        hot_standby: "on"
        max_wal_senders: 10
        max_replication_slots: 10
        max_connections: 500
        password_encryption: scram-sha-256
        shared_preload_libraries: pg_stat_statements
{pgbackrest_archive_params}
  initdb:
    - encoding: UTF8
    - data-checksums
    - username: {superuser}

  pg_hba:
    - local all all trust
    - hostssl replication {repl_user} 0.0.0.0/0 scram-sha-256
    - hostssl replication {repl_user} ::/0 scram-sha-256
    - hostssl all all 0.0.0.0/0 scram-sha-256
    - hostssl all all ::/0 scram-sha-256
    - host replication {repl_user} 0.0.0.0/0 scram-sha-256
    - host replication {repl_user} ::/0 scram-sha-256
    - host all all 0.0.0.0/0 scram-sha-256
    - host all all ::/0 scram-sha-256

  post_bootstrap: /usr/local/bin/post-bootstrap

postgresql:
  listen: "*:5432"
  connect_address: {connect_address}:5432
  data_dir: {data_dir}
  basebackup:
    max-rate: {basebackup_max_rate}
    checkpoint: fast
  pgpass: /tmp/pgpass
  callbacks:
    on_role_change: /usr/local/bin/on-role-change
  authentication:
    replication:
      username: "{repl_user}"
      password: "{repl_pass}"
    superuser:
      username: "{superuser}"
      password: "{superuser_pass}"
    rewind:
      username: "{superuser}"
      password: "{superuser_pass}"
  app_user:
    username: "{app_user}"
    password: "{app_pass}"
    database: "{app_db}"
  parameters:
    unix_socket_directories: /var/run/postgresql
    ssl: "on"
    ssl_cert_file: "{certs_dir}/server.crt"
    ssl_key_file: "{certs_dir}/server.key"
    ssl_ca_file: "{certs_dir}/root.crt"
{pgbackrest_local_params}"#,
        scope = config.scope,
        name = config.name,
        connect_address = config.connect_address,
        etcd_hosts = config.etcd_hosts,
        ttl = config.ttl,
        loop_wait = config.loop_wait,
        retry_timeout = config.retry_timeout,
        superuser = config.superuser,
        superuser_pass = config.superuser_pass,
        repl_user = config.repl_user,
        repl_pass = config.repl_pass,
        app_user = config.app_user,
        app_pass = config.app_pass,
        app_db = config.app_db,
        data_dir = config.data_dir,
        certs_dir = config.certs_dir,
        synchronous_mode = config.synchronous_mode,
        basebackup_max_rate = config.basebackup_max_rate,
        pgbackrest_archive_params = pgbackrest_archive_params,
        pgbackrest_local_params = pgbackrest_local_params,
    )
}

/// Update pg_hba.conf to add replication entries for adopted data
pub fn update_pg_hba_for_replication(config: &Config) -> Result<()> {
    let pg_hba_path = format!("{}/pg_hba.conf", config.data_dir);

    if !Path::new(&pg_hba_path).exists() {
        return Ok(());
    }

    info!(user = %config.repl_user, "Checking pg_hba.conf for replication");

    let content = fs::read_to_string(&pg_hba_path)?;

    if content.contains(&format!("replication {}", config.repl_user))
        || content.contains(&format!("replication\t{}", config.repl_user))
    {
        info!("Replication entries already exist");
        return Ok(());
    }

    info!("Adding replication entries to pg_hba.conf");

    let new_entries = format!(
        r#"# Replication entries for {}
hostssl replication {} 0.0.0.0/0 scram-sha-256
hostssl replication {} ::/0 scram-sha-256
host replication {} 0.0.0.0/0 scram-sha-256
host replication {} ::/0 scram-sha-256

"#,
        config.repl_user, config.repl_user, config.repl_user, config.repl_user, config.repl_user
    );

    let new_content = format!("{}{}", new_entries, content);
    fs::write(&pg_hba_path, new_content)?;
    fs::set_permissions(&pg_hba_path, std::fs::Permissions::from_mode(0o600))?;

    info!("pg_hba.conf updated");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(wal_archive_bucket: Option<&str>) -> Config {
        Config {
            scope: "test-scope".into(),
            name: "test-node".into(),
            connect_address: "test-node".into(),
            etcd_hosts: "etcd-1:2379".into(),
            superuser: "postgres".into(),
            superuser_pass: "pw".into(),
            repl_user: "repl".into(),
            repl_pass: "pw".into(),
            app_user: "app".into(),
            app_pass: "pw".into(),
            app_db: "app".into(),
            data_dir: "/var/lib/postgresql/data/pgdata".into(),
            certs_dir: "/certs".into(),
            ttl: "30".into(),
            loop_wait: "10".into(),
            retry_timeout: "10".into(),
            health_check_interval: 5,
            health_check_timeout: 3,
            max_failures: 3,
            startup_grace_period: 30,
            max_startup_timeout: 1800,
            adopt_existing_data: false,
            wait_for_leader: false,
            synchronous_mode: false,
            wal_archive_bucket: wal_archive_bucket.map(String::from),
            wal_recover_from_bucket: None,
            pitr_target_time: None,
            pitr_target_xid: None,
            archive_timeout_secs: 60,
            basebackup_max_rate: "20M".into(),
        }
    }

    #[test]
    fn basebackup_is_throttled_with_and_without_archiving() {
        for cfg in [test_config(Some("bucket")), test_config(None)] {
            let yaml = generate_patroni_config(&cfg, "replica");
            assert!(yaml.contains(
                "  data_dir: /var/lib/postgresql/data/pgdata\n  basebackup:\n    max-rate: 20M\n    checkpoint: fast\n  pgpass: /tmp/pgpass\n"
            ));
        }
    }

    #[test]
    fn basebackup_max_rate_renders_from_config() {
        let mut cfg = test_config(None);
        cfg.basebackup_max_rate = "64M".into();
        let yaml = generate_patroni_config(&cfg, "replica");
        assert!(yaml.contains("  basebackup:\n    max-rate: 64M\n    checkpoint: fast\n"));
    }
}
