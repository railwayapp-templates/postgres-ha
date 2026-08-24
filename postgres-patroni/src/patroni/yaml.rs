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
    // restore_command gives every standby an archive fallback: when streaming
    // breaks and the leader has already recycled the WAL a standby needs
    // (post-rewind catch-up, long clone, extended disconnect), recovery pulls
    // the missing segments from the stanza's own S3 archive instead of
    // wedging on "requested WAL segment has already been removed". Patroni
    // moves recovery parameters out of postgresql.parameters into the
    // recovery section it writes for standbys (_adjust_recovery_parameters →
    // build_recovery_params), so this reaches every standby and never the
    // primary. It goes through the archive-get wrapper because repo1-path
    // must be resolved at call time — volume marker first, Patroni DCS on a
    // miss (the marker can go stale on standbys after a leader-side path
    // migration): the env Postgres inherits from Patroni still holds the
    // pre-derivation base path, and pgBackRest prefers env over
    // pgbackrest.conf.
    let pgbackrest_archive_params = if config.wal_archive_bucket.is_some() {
        format!(
            "        archive_mode: \"on\"\n        archive_command: \"/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p\"\n        archive_timeout: {}\n        track_commit_timestamp: \"on\"\n        restore_command: \"/usr/local/bin/pgbackrest-archive-get-wrapper.sh %f %p\"\n",
            config.archive_timeout_secs,
        )
    } else {
        String::new()
    };

    // Re-seed replicas from the S3 archive instead of the live leader:
    // pgbackrest restore downloads the base backup from object storage in
    // parallel and replays archived WAL via restore_command, placing zero
    // read load on the leader's volume. pg_basebackup — a single sequential
    // stream read directly off the leader, competing with production
    // queries for the same disk — stays as the fallback for fresh stanzas
    // (no backup in the catalog yet) and restore failures. The wrapper
    // resolves the per-cluster repo1-path at call time (DCS → volume
    // marker → env default): a wiped volume has neither pg_control nor the
    // marker, and the env Patroni inherited holds only the base path.
    let pgbackrest_replica_method = if config.wal_archive_bucket.is_some() {
        "  create_replica_methods:\n    - pgbackrest\n    - basebackup\n  pgbackrest:\n    command: \"/usr/local/bin/pgbackrest-replica-restore-wrapper.sh\"\n    keep_data: true\n    no_params: true\n".to_string()
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
    // max_slot_wal_keep_size bounds what a lagging member's slot can pin on the
    // leader. It lives in the LOCAL parameters, not bootstrap.dcs: the value is
    // derived from this node's own volume, every member can become leader, and
    // bootstrap.dcs only seeds DCS at cluster genesis — existing clusters (the
    // ones already at risk) would never pick it up. NOTE the flip side: for a
    // non-CMDLINE_OPTIONS parameter like this one, Patroni's local config takes
    // precedence over DCS (`_build_effective_configuration`), so a DCS-side
    // value can never override this line — the supported override is
    // POSTGRES_MAX_SLOT_WAL_KEEP_SIZE, and live re-sizing happens via the
    // slot-recovery watcher's ALTER SYSTEM path (slot_recovery.rs), whose
    // auto.conf entry outranks this rendered value. PGC_SIGHUP, so a reload
    // applies it. See Config::resolve_max_slot_wal_keep_size for the sizing
    // and the failure it prevents.
    let pgbackrest_local_params = if config.wal_archive_bucket.is_some() {
        "    archive_mode: \"on\"\n    track_commit_timestamp: \"on\"\n    restore_command: \"/usr/local/bin/pgbackrest-archive-get-wrapper.sh %f %p\"\n".to_string()
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
{pgbackrest_replica_method}  basebackup:
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
    max_slot_wal_keep_size: {max_slot_wal_keep_size}
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
        max_slot_wal_keep_size = config.max_slot_wal_keep_size,
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
            max_slot_wal_keep_size: "512000MB".into(),
        }
    }

    fn test_config_with_timeout(wal_archive_bucket: Option<&str>, archive_timeout_secs: i64) -> Config {
        Config {
            archive_timeout_secs,
            ..test_config(wal_archive_bucket)
        }
    }

    #[test]
    fn restore_command_is_independent_of_archive_timeout_value() {
        // restore_command is a fixed wrapper invocation with no interpolated
        // fields — unlike archive_command's archive_timeout, a custom
        // POSTGRES_ARCHIVE_TIMEOUT must not change its rendered line, and
        // both occurrences (bootstrap DCS + local params) must stay
        // byte-identical to each other regardless of the timeout value.
        for timeout in [60, 120, 5] {
            let yaml = generate_patroni_config(&test_config_with_timeout(Some("bucket"), timeout), "replica");
            let expected = "restore_command: \"/usr/local/bin/pgbackrest-archive-get-wrapper.sh %f %p\"\n";
            let occurrences = yaml.matches(expected).count();
            assert_eq!(
                occurrences, 2,
                "expected exactly 2 identical restore_command lines (bootstrap DCS + local) at timeout={timeout}, found {occurrences}"
            );
            // The archive_timeout line right next to it DOES vary with the value.
            assert!(yaml.contains(&format!("archive_timeout: {timeout}\n")));
        }
    }

    #[test]
    fn max_slot_wal_keep_size_renders_locally_and_regardless_of_archiving() {
        // Local parameters, NOT bootstrap.dcs: the cap is derived from this
        // node's own volume and must reach clusters that already bootstrapped
        // (exactly the ones at risk), which bootstrap.dcs never would.
        for bucket in [Some("bucket"), None] {
            let yaml = generate_patroni_config(&test_config(bucket), "replica");
            let occurrences = yaml.matches("max_slot_wal_keep_size: 512000MB").count();
            assert_eq!(
                occurrences, 1,
                "expected exactly 1 local max_slot_wal_keep_size line (archiving={bucket:?}), found {occurrences}"
            );
        }

        // It must sit under the local `postgresql:` section, after the ssl
        // params — not inside bootstrap.dcs.
        let yaml = generate_patroni_config(&test_config(None), "replica");
        let bootstrap_end = yaml.find("postgresql:\n  listen:").expect("local postgresql section");
        let cap_at = yaml.find("max_slot_wal_keep_size:").expect("cap rendered");
        assert!(
            cap_at > bootstrap_end,
            "max_slot_wal_keep_size must render in the local section, not bootstrap.dcs"
        );
    }

    #[test]
    fn archiving_enabled_seeds_replica_method_and_restore_command() {
        let yaml = generate_patroni_config(&test_config(Some("bucket")), "replica");

        // Replica re-seed method ahead of basebackup, via the call-time
        // repo-path-resolving wrapper.
        assert!(yaml.contains("  create_replica_methods:\n    - pgbackrest\n    - basebackup\n"));
        assert!(yaml.contains("command: \"/usr/local/bin/pgbackrest-replica-restore-wrapper.sh\""));
        assert!(yaml.contains("keep_data: true"));
        assert!(yaml.contains("no_params: true"));

        // Archive fallback for standbys, in both bootstrap DCS seed (8-space
        // indent) and local postgresql.parameters (4-space indent).
        assert!(yaml.contains(
            "        restore_command: \"/usr/local/bin/pgbackrest-archive-get-wrapper.sh %f %p\"\n"
        ));
        assert!(yaml.contains(
            "    restore_command: \"/usr/local/bin/pgbackrest-archive-get-wrapper.sh %f %p\"\n"
        ));
    }

    #[test]
    fn archiving_disabled_omits_replica_method_and_restore_command() {
        let yaml = generate_patroni_config(&test_config(None), "replica");

        assert!(!yaml.contains("create_replica_methods"));
        assert!(!yaml.contains("restore_command"));
        assert!(!yaml.contains("pgbackrest"));
        // The interpolation site must still render valid YAML: with no
        // replica-method block, data_dir is directly followed by the
        // basebackup throttle options.
        assert!(yaml.contains("  data_dir: /var/lib/postgresql/data/pgdata\n  basebackup:"));
    }

    #[test]
    fn replica_method_block_renders_between_data_dir_and_basebackup_options() {
        let yaml = generate_patroni_config(&test_config(Some("bucket")), "replica");
        assert!(yaml.contains(
            "  data_dir: /var/lib/postgresql/data/pgdata\n  create_replica_methods:"
        ));
        assert!(yaml.contains("    no_params: true\n  basebackup:"));
    }

    #[test]
    fn basebackup_is_throttled_with_and_without_archiving() {
        // No data_dir adjacency here: with archiving enabled the
        // replica-method block sits between data_dir and this options
        // block. The throttle itself must render either way.
        for cfg in [test_config(Some("bucket")), test_config(None)] {
            let yaml = generate_patroni_config(&cfg, "replica");
            assert!(yaml.contains(
                "  basebackup:\n    max-rate: 20M\n    checkpoint: fast\n  pgpass: /tmp/pgpass\n"
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
