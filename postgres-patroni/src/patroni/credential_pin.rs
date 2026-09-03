//! Pin the credentials a node's dataset actually runs with.
//!
//! Patroni creates the superuser / replication / app roles exactly once, in
//! `post_bootstrap`, and never re-syncs their passwords afterwards — but
//! `patroni.yml` (and from it Patroni's `pgpass`, the rewind credentials and
//! every wrapper-side `PGPASSWORD`) is re-rendered from the environment on
//! every boot. On Railway, `PATRONI_SUPERUSER_PASSWORD` and
//! `PATRONI_REPLICATION_PASSWORD` are references to `POSTGRES_PASSWORD`, and a
//! redeploy re-resolves variables. So editing `POSTGRES_PASSWORD` and
//! redeploying half-applies: the leader's roles keep the old password while
//! every restarted node's `pgpass` carries the new one — replicas cannot
//! authenticate to the primary and the cluster degrades to a lone leader with
//! streaming replication dead. The platform's variable editor promises the
//! opposite ("changes the variable without updating the actual database
//! password"); redis-ha (#46) and mysql-ha honor that promise by pinning.
//!
//! This module does the same for Postgres: the credentials the cluster was
//! bootstrapped with are persisted next to the data (`$PGDATA/.railway_credentials`,
//! mode 0600, owned by postgres like everything else in PGDATA) and, on every
//! later boot of a volume that carries a cluster, they override whatever the
//! variables say — with a loud warning and a telemetry event, so the drift is
//! visible. A fresh volume (first boot, conversion, scale-up) takes the
//! variables as-is, exactly as before.
//!
//! The pin lives inside PGDATA on purpose: `pg_basebackup` and pgBackRest copy
//! it into clones and backups, so a replica seeded from a pinned leader boots
//! coherent with it (the same reason the forced-archive-GUCs sentinel lives
//! there). Existing clusters that predate the pin adopt their current
//! variables as the pin on the first boot of this image — at that point the
//! variables are, by construction, the credentials the cluster runs with.
//!
//! Break-glass: `PATRONI_CREDENTIALS_FROM_ENV=true` makes the variables win
//! for that boot and re-pins them — for an operator who has ALREADY rotated
//! the roles with `ALTER ROLE` and now wants the variables to follow.

use anyhow::{Context, Result};
use common::{ConfigExt, Telemetry, TelemetryEvent};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use tracing::{info, warn};

use super::config::Config;

/// File inside PGDATA holding the pinned credentials.
pub const CREDENTIAL_PIN_FILE: &str = ".railway_credentials";

/// Env knob: when "true", the variables win this boot and are re-pinned.
pub const CREDENTIALS_FROM_ENV_VAR: &str = "PATRONI_CREDENTIALS_FROM_ENV";

/// The passwords a dataset runs with. Usernames are not pinned — changing a
/// username is a different operation (a new role), not a rotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedCredentials {
    pub superuser_pass: String,
    pub repl_pass: String,
    pub app_pass: String,
}

impl PinnedCredentials {
    pub fn from_config(config: &Config) -> Self {
        Self {
            superuser_pass: config.superuser_pass.clone(),
            repl_pass: config.repl_pass.clone(),
            app_pass: config.app_pass.clone(),
        }
    }
}

/// What [`apply_credential_pin`] did this boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinOutcome {
    /// No cluster on this volume yet — the variables apply as-is; the pin is
    /// written by post-bootstrap (leader) or inherited from the clone source
    /// (replicas).
    FreshVolume,
    /// A cluster without a pin (predates this image): the current variables
    /// were adopted as the pin.
    Adopted,
    /// The variables match the pin; nothing to do.
    Matches,
    /// The variables drifted from the pin; the pinned values were kept.
    /// Carries the config fields that were overridden.
    KeptPinned(Vec<&'static str>),
    /// `PATRONI_CREDENTIALS_FROM_ENV=true`: the variables won and were re-pinned.
    RepinnedFromEnv,
}

pub fn pin_path(data_dir: &str) -> String {
    format!("{data_dir}/{CREDENTIAL_PIN_FILE}")
}

/// The pinned credentials on this volume, or None on a fresh volume. An
/// unreadable or unparseable pin is logged and treated as absent — the
/// variables then apply, which is the pre-pin behaviour, never a crash.
pub fn read_credential_pin(data_dir: &str) -> Option<PinnedCredentials> {
    let path = pin_path(data_dir);
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(error = %e, path, "credential pin unreadable — variables apply this boot");
            return None;
        }
    };
    match serde_json::from_str::<PinnedCredentials>(&content) {
        Ok(pin) => Some(pin),
        Err(e) => {
            warn!(error = %e, path, "credential pin unparseable — variables apply this boot");
            None
        }
    }
}

/// Persist the pin atomically (temp file + rename), mode 0600.
pub fn write_credential_pin(data_dir: &str, creds: &PinnedCredentials) -> Result<()> {
    let path = pin_path(data_dir);
    let tmp = format!("{path}.tmp");
    let json = serde_json::to_string_pretty(creds).context("serialize credential pin")?;
    std::fs::write(&tmp, json).with_context(|| format!("write {tmp}"))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {tmp}"))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename {tmp} -> {path}"))?;
    Ok(())
}

/// Whether this boot asked for the break-glass path
/// (`PATRONI_CREDENTIALS_FROM_ENV=true`). Read once by the caller and passed
/// into [`apply_credential_pin`] so the decision is explicit (and testable
/// without touching the process environment).
pub fn credentials_from_env_requested() -> bool {
    bool::env_parse(CREDENTIALS_FROM_ENV_VAR, false)
}

/// Reconcile `config`'s passwords with the pin on this volume. See the module
/// doc for the contract. `has_cluster_data` is the runner's "pg_control +
/// bootstrap marker" test — a volume that carries a Patroni-bootstrapped
/// cluster, as opposed to an empty volume or stale data about to be cloned
/// over. `credentials_from_env` is [`credentials_from_env_requested`].
pub fn apply_credential_pin(
    config: &mut Config,
    has_cluster_data: bool,
    credentials_from_env: bool,
    telemetry: &Telemetry,
) -> PinOutcome {
    // The pin lives inside PGDATA so that clones inherit it, but
    // `has_cluster_data` is keyed on the volume-root bootstrap marker, which
    // `post_bootstrap` writes only on the node that bootstrapped the cluster.
    // A replica seeded by pg_basebackup therefore carries the pin and never
    // the marker, and would read as a fresh volume on every boot. An existing
    // pin is itself proof this volume carries a cluster, so honor it whatever
    // the marker says; the marker still gates ADOPTING the variables onto a
    // volume that has no pin yet.
    let existing_pin = read_credential_pin(&config.data_dir);

    if !has_cluster_data && existing_pin.is_none() {
        return PinOutcome::FreshVolume;
    }

    let from_env = PinnedCredentials::from_config(config);

    if credentials_from_env {
        match write_credential_pin(&config.data_dir, &from_env) {
            Ok(()) => warn!(
                "{CREDENTIALS_FROM_ENV_VAR}=true: credentials taken from the variables and \
                 re-pinned — the roles must already carry these passwords (ALTER ROLE), or \
                 replication and rewind break on this node"
            ),
            Err(e) => warn!(error = %e, "failed to re-pin credentials from the variables"),
        }
        return PinOutcome::RepinnedFromEnv;
    }

    let Some(pinned) = existing_pin else {
        // An existing cluster that predates the pin: its variables are, by
        // construction, the credentials it runs with (nothing else ever set
        // them) — adopt them.
        match write_credential_pin(&config.data_dir, &from_env) {
            Ok(()) => info!(
                path = pin_path(&config.data_dir),
                "no credential pin on this volume — pinned the current variables"
            ),
            Err(e) => warn!(
                error = %e,
                "failed to write the credential pin — variables apply this boot and every boot \
                 until the write succeeds"
            ),
        }
        return PinOutcome::Adopted;
    };

    if pinned == from_env {
        return PinOutcome::Matches;
    }

    let mut overridden: Vec<&'static str> = Vec::new();
    if pinned.superuser_pass != config.superuser_pass {
        config.superuser_pass = pinned.superuser_pass.clone();
        overridden.push("PATRONI_SUPERUSER_PASSWORD");
    }
    if pinned.repl_pass != config.repl_pass {
        config.repl_pass = pinned.repl_pass.clone();
        overridden.push("PATRONI_REPLICATION_PASSWORD");
    }
    if pinned.app_pass != config.app_pass {
        config.app_pass = pinned.app_pass.clone();
        overridden.push("POSTGRES_PASSWORD");
    }

    warn!(
        drifted = ?overridden,
        "credential variables differ from the passwords this cluster's roles carry — keeping \
         the pinned credentials; editing the variable does not rotate the database password \
         (set {CREDENTIALS_FROM_ENV_VAR}=true only after rotating the roles with ALTER ROLE)"
    );
    telemetry.send(TelemetryEvent::ComponentError {
        component: "patroni-runner".to_string(),
        error: format!(
            "credential variables drifted from the pinned credentials ({}); kept the pinned ones",
            overridden.join(", ")
        ),
        context: "startup".to_string(),
    });

    PinOutcome::KeptPinned(overridden)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patroni::RestapiAddressSource;
    use tempfile::tempdir;

    fn config_at(data_dir: &str) -> Config {
        Config {
            scope: "s".into(),
            name: "n".into(),
            connect_address: "n".into(),
            restapi_connect_address: "n:8008".into(),
            restapi_address_source: RestapiAddressSource::PrivateDomain,
            etcd_hosts: "etcd:2379".into(),
            superuser: "postgres".into(),
            superuser_pass: "su-env".into(),
            repl_user: "replicator".into(),
            repl_pass: "repl-env".into(),
            app_user: "app".into(),
            app_pass: "app-env".into(),
            app_db: "railway".into(),
            data_dir: data_dir.to_string(),
            certs_dir: format!("{data_dir}/certs"),
            ttl: "45".into(),
            loop_wait: "10".into(),
            retry_timeout: "17".into(),
            health_check_interval: 5,
            health_check_timeout: 5,
            max_failures: 3,
            startup_grace_period: 60,
            max_startup_timeout: 1800,
            adopt_existing_data: false,
            wait_for_leader: false,
            synchronous_mode: false,
            failsafe_mode: true,
            wal_archive_bucket: None,
            wal_recover_from_bucket: None,
            pitr_target_time: None,
            pitr_target_xid: None,
            archive_timeout_secs: 60,
            basebackup_max_rate: "20M".into(),
            max_slot_wal_keep_size: "512000MB".into(),
        }
    }

    // Off-Railway, Telemetry::from_env is disabled: send() only logs.
    fn telemetry() -> Telemetry {
        Telemetry::from_env("credential-pin-test")
    }

    #[test]
    fn pin_round_trips_hostile_passwords() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let creds = PinnedCredentials {
            superuser_pass: "p# w\"or\\d'$$".into(),
            repl_pass: "ünï©ødé\n".into(),
            app_pass: String::new(),
        };
        write_credential_pin(data_dir, &creds).unwrap();
        assert_eq!(read_credential_pin(data_dir), Some(creds));
        let mode = std::fs::metadata(pin_path(data_dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn pin_is_absent_on_a_fresh_volume_or_when_unparseable() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        assert_eq!(read_credential_pin(data_dir), None);
        std::fs::write(pin_path(data_dir), "not json").unwrap();
        assert_eq!(read_credential_pin(data_dir), None);
    }

    #[test]
    fn fresh_volume_takes_the_variables_and_writes_nothing() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let mut config = config_at(data_dir);
        assert_eq!(
            apply_credential_pin(&mut config, false, false, &telemetry()),
            PinOutcome::FreshVolume
        );
        assert_eq!(config.superuser_pass, "su-env");
        assert_eq!(read_credential_pin(data_dir), None);
    }

    #[test]
    fn replica_without_the_bootstrap_marker_still_honors_its_pin() {
        // post_bootstrap writes the volume-root marker only on the node that
        // bootstrapped the cluster, so a basebackup-seeded replica boots with
        // has_cluster_data = false for the life of the volume — while carrying
        // the pin its clone copied out of the leader's PGDATA. The pin has to
        // win there too, or the replica renders drifted variables into its
        // pgpass and cannot authenticate to the leader.
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let pinned = PinnedCredentials {
            superuser_pass: "su-active".into(),
            repl_pass: "repl-active".into(),
            app_pass: "app-active".into(),
        };
        write_credential_pin(data_dir, &pinned).unwrap();

        let mut config = config_at(data_dir);
        let outcome = apply_credential_pin(&mut config, false, false, &telemetry());
        assert_eq!(
            outcome,
            PinOutcome::KeptPinned(vec![
                "PATRONI_SUPERUSER_PASSWORD",
                "PATRONI_REPLICATION_PASSWORD",
                "POSTGRES_PASSWORD",
            ])
        );
        assert_eq!(config.superuser_pass, "su-active");
        assert_eq!(config.repl_pass, "repl-active");
        assert_eq!(config.app_pass, "app-active");
        assert_eq!(read_credential_pin(data_dir), Some(pinned));
    }

    #[test]
    fn cluster_without_a_pin_adopts_the_variables() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let mut config = config_at(data_dir);
        assert_eq!(
            apply_credential_pin(&mut config, true, false, &telemetry()),
            PinOutcome::Adopted
        );
        assert_eq!(
            read_credential_pin(data_dir),
            Some(PinnedCredentials::from_config(&config))
        );
        // Second boot with the same variables is a no-op.
        assert_eq!(
            apply_credential_pin(&mut config, true, false, &telemetry()),
            PinOutcome::Matches
        );
    }

    #[test]
    fn drifted_variables_are_overridden_by_the_pin() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let pinned = PinnedCredentials {
            superuser_pass: "su-active".into(),
            repl_pass: "repl-active".into(),
            app_pass: "app-env".into(), // unchanged
        };
        write_credential_pin(data_dir, &pinned).unwrap();

        let mut config = config_at(data_dir);
        let outcome = apply_credential_pin(&mut config, true, false, &telemetry());
        assert_eq!(
            outcome,
            PinOutcome::KeptPinned(vec![
                "PATRONI_SUPERUSER_PASSWORD",
                "PATRONI_REPLICATION_PASSWORD",
            ])
        );
        assert_eq!(config.superuser_pass, "su-active");
        assert_eq!(config.repl_pass, "repl-active");
        assert_eq!(config.app_pass, "app-env");
        // The pin itself is untouched by a drifted boot.
        assert_eq!(read_credential_pin(data_dir), Some(pinned));
    }

    #[test]
    fn break_glass_env_wins_and_repins() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        write_credential_pin(
            data_dir,
            &PinnedCredentials {
                superuser_pass: "old".into(),
                repl_pass: "old".into(),
                app_pass: "old".into(),
            },
        )
        .unwrap();
        let mut config = config_at(data_dir);
        let outcome = apply_credential_pin(&mut config, true, true, &telemetry());
        assert_eq!(outcome, PinOutcome::RepinnedFromEnv);
        assert_eq!(config.superuser_pass, "su-env");
        assert_eq!(
            read_credential_pin(data_dir),
            Some(PinnedCredentials::from_config(&config))
        );
    }
}
