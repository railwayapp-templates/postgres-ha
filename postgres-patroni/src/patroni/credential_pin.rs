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
    /// Password Patroni presents to etcd. Pinned for the same reason as the
    /// role passwords: etcd's `root` user is created ONCE, when the entrypoint
    /// first enables authentication, and nothing ever runs `user passwd`. So
    /// the password etcd accepts is frozen at cluster-creation time while the
    /// variable it came from is not. `None` on a pin written before this field
    /// existed; see [`apply_credential_pin`] for how those are backfilled.
    #[serde(default)]
    pub etcd_pass: Option<String>,
    /// Password this member presents to (and enforces on) the Patroni REST
    /// API. Frozen the same way: the peers enforce what they booted with.
    #[serde(default)]
    pub restapi_pass: Option<String>,
}

impl PinnedCredentials {
    pub fn from_config(config: &Config) -> Self {
        Self {
            superuser_pass: config.superuser_pass.clone(),
            repl_pass: config.repl_pass.clone(),
            app_pass: config.app_pass.clone(),
            etcd_pass: config.etcd_auth.as_ref().map(|c| c.password.clone()),
            restapi_pass: config.restapi_auth.as_ref().map(|c| c.password.clone()),
        }
    }

    /// The control-plane passwords a pre-#130 pin does not carry.
    ///
    /// When the variables have NOT drifted they are, by construction, what the
    /// cluster runs with — adopt them. When they HAVE drifted the originals are
    /// no longer anywhere in the environment, and the only remaining signal is
    /// the pinned superuser password: the template derives both control-plane
    /// passwords from `POSTGRES_PASSWORD`, so that is what etcd's root was
    /// created with and what the peers enforce.
    fn backfill_control_plane(&mut self, from_env: &PinnedCredentials, env_drifted: bool) -> bool {
        let reconstructed = self.superuser_pass.clone();
        let mut filled = false;
        // Only where a credential actually exists this boot. A cluster with no
        // control-plane auth configured keeps `None` and its pin untouched —
        // inventing a password for a credential nothing uses would be a guess
        // written to disk.
        if self.etcd_pass.is_none() && from_env.etcd_pass.is_some() {
            self.etcd_pass = Some(if env_drifted {
                reconstructed.clone()
            } else {
                from_env.etcd_pass.clone().unwrap()
            });
            filled = true;
        }
        if self.restapi_pass.is_none() && from_env.restapi_pass.is_some() {
            self.restapi_pass = Some(if env_drifted {
                reconstructed
            } else {
                from_env.restapi_pass.clone().unwrap()
            });
            filled = true;
        }
        filled
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

    // A pin written before the control-plane passwords were pinned carries
    // neither; fill them in before comparing, and persist the completed pin so
    // the next boot has them. `env_drifted` is decided on the role passwords
    // alone — the only part a pre-#130 pin can speak to.
    let mut pinned = pinned;
    let env_drifted = pinned.superuser_pass != from_env.superuser_pass;
    if pinned.backfill_control_plane(&from_env, env_drifted) {
        match write_credential_pin(&config.data_dir, &pinned) {
            Ok(()) => info!(
                env_drifted,
                "backfilled the control-plane passwords into this volume's credential pin"
            ),
            Err(e) => warn!(error = %e, "failed to persist the backfilled credential pin"),
        }
    }

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
    // The control-plane credentials drift for exactly the same reason and are
    // just as unrecoverable: the template derives them from POSTGRES_PASSWORD,
    // etcd's root password is set once and never updated, and the peers
    // enforce what they booted with. Leaving these on the variables is what
    // made a POSTGRES_PASSWORD edit crash-loop a member on
    // `Etcd3 authentication failed: invalid user ID or password` — verified on
    // a live cluster 2026-09-05, where the edited member lost its leadership
    // and never rejoined while the two it had not restarted stayed healthy.
    if let (Some(pinned_etcd), Some(auth)) = (&pinned.etcd_pass, config.etcd_auth.as_mut()) {
        if *pinned_etcd != auth.password {
            auth.password = pinned_etcd.clone();
            overridden.push("PATRONI_ETCD3_PASSWORD");
        }
    }
    if let (Some(pinned_rest), Some(auth)) = (&pinned.restapi_pass, config.restapi_auth.as_mut()) {
        if *pinned_rest != auth.password {
            auth.password = pinned_rest.clone();
            overridden.push("PATRONI_RESTAPI_PASSWORD");
        }
    }

    if overridden.is_empty() {
        return PinOutcome::Matches;
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
    use crate::patroni::{Credential, RestapiAddressSource};
    use tempfile::tempdir;

    fn config_at(data_dir: &str) -> Config {
        Config {
            scope: "s".into(),
            name: "n".into(),
            connect_address: "n".into(),
            restapi_connect_address: "n:8008".into(),
            restapi_address_source: RestapiAddressSource::PrivateDomain,
            etcd_hosts: "etcd:2379".into(),
            etcd_auth: None,
            restapi_auth: None,
            restapi_auth_enforced: false,
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
            etcd_pass: None,
            restapi_pass: None,
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
            etcd_pass: None,
            restapi_pass: None,
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
            etcd_pass: None,
            restapi_pass: None,
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

    /// A config that carries the control-plane credentials, the way a cluster
    /// deployed from the production template does.
    fn config_with_control_plane(data_dir: &str, etcd_pw: &str, rest_pw: &str) -> Config {
        let mut config = config_at(data_dir);
        config.etcd_auth = Some(Credential {
            username: "root".into(),
            password: etcd_pw.into(),
        });
        config.restapi_auth = Some(Credential {
            username: "postgres".into(),
            password: rest_pw.into(),
        });
        config.restapi_auth_enforced = true;
        config
    }

    /// The bug this pin exists to prevent, extended to the control plane.
    ///
    /// Reproduced on a live cluster 2026-09-05: editing POSTGRES_PASSWORD (which
    /// the platform documents as NOT rotating the database password) left the
    /// role passwords correctly pinned but sent the NEW password to etcd, whose
    /// `root` user is created once and never updated. The member crash-looped on
    /// `Etcd3 authentication failed: invalid user ID or password`, lost its
    /// leadership and never rejoined. Every replica references the root's
    /// password, so one edit arms the whole cluster.
    #[test]
    fn drifted_variables_do_not_reach_etcd_or_the_rest_api() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        write_credential_pin(
            data_dir,
            &PinnedCredentials {
                superuser_pass: "su-active".into(),
                repl_pass: "repl-active".into(),
                app_pass: "app-active".into(),
                etcd_pass: Some("su-active".into()),
                restapi_pass: Some("su-active".into()),
            },
        )
        .unwrap();

        // Every variable followed the edit, control-plane ones included: the
        // template defines them as references to POSTGRES_PASSWORD.
        let mut config = config_with_control_plane(data_dir, "su-edited", "su-edited");
        config.superuser_pass = "su-edited".into();
        config.repl_pass = "repl-active".into();
        config.app_pass = "app-active".into();

        let outcome = apply_credential_pin(&mut config, true, false, &telemetry());
        assert_eq!(
            outcome,
            PinOutcome::KeptPinned(vec![
                "PATRONI_SUPERUSER_PASSWORD",
                "PATRONI_ETCD3_PASSWORD",
                "PATRONI_RESTAPI_PASSWORD",
            ])
        );
        assert_eq!(config.etcd_auth.unwrap().password, "su-active");
        assert_eq!(config.restapi_auth.unwrap().password, "su-active");
    }

    /// A pin written before this field existed, on a cluster whose variables
    /// have NOT drifted: the current values are by construction what the
    /// cluster runs with, so adopt them and persist the completed pin.
    #[test]
    fn a_pin_without_control_plane_fields_adopts_them_when_nothing_drifted() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        write_credential_pin(
            data_dir,
            &PinnedCredentials {
                superuser_pass: "su-env".into(),
                repl_pass: "repl-env".into(),
                app_pass: "app-env".into(),
                etcd_pass: None,
                restapi_pass: None,
            },
        )
        .unwrap();

        let mut config = config_with_control_plane(data_dir, "etcd-secret", "rest-secret");
        let outcome = apply_credential_pin(&mut config, true, false, &telemetry());

        assert_eq!(outcome, PinOutcome::Matches);
        assert_eq!(config.etcd_auth.as_ref().unwrap().password, "etcd-secret");
        let persisted = read_credential_pin(data_dir).unwrap();
        assert_eq!(persisted.etcd_pass.as_deref(), Some("etcd-secret"));
        assert_eq!(persisted.restapi_pass.as_deref(), Some("rest-secret"));
    }

    /// The same old pin, but the edit already happened: the originals are gone
    /// from the environment. The pinned superuser password is the only signal
    /// left, and it is the right one — the template derives both control-plane
    /// passwords from POSTGRES_PASSWORD, so that is what etcd's root was
    /// created with and what the peers enforce.
    #[test]
    fn a_pin_without_control_plane_fields_reconstructs_them_when_already_drifted() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        write_credential_pin(
            data_dir,
            &PinnedCredentials {
                superuser_pass: "su-active".into(),
                repl_pass: "repl-active".into(),
                app_pass: "app-active".into(),
                etcd_pass: None,
                restapi_pass: None,
            },
        )
        .unwrap();

        let mut config = config_with_control_plane(data_dir, "su-edited", "su-edited");
        config.superuser_pass = "su-edited".into();
        config.repl_pass = "repl-active".into();
        config.app_pass = "app-active".into();

        apply_credential_pin(&mut config, true, false, &telemetry());
        assert_eq!(config.etcd_auth.unwrap().password, "su-active");
        assert_eq!(config.restapi_auth.unwrap().password, "su-active");
        assert_eq!(
            read_credential_pin(data_dir).unwrap().etcd_pass.as_deref(),
            Some("su-active")
        );
    }

    /// A cluster with no control-plane auth at all must not have a password
    /// invented for it and written to disk.
    #[test]
    fn no_control_plane_auth_leaves_the_pin_alone() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let pinned = PinnedCredentials {
            superuser_pass: "su-active".into(),
            repl_pass: "repl-active".into(),
            app_pass: "app-active".into(),
            etcd_pass: None,
            restapi_pass: None,
        };
        write_credential_pin(data_dir, &pinned).unwrap();

        let mut config = config_at(data_dir); // etcd_auth / restapi_auth are None
        config.superuser_pass = "su-edited".into();
        apply_credential_pin(&mut config, true, false, &telemetry());

        assert_eq!(read_credential_pin(data_dir), Some(pinned));
    }

    /// An older pin has to keep deserializing — the two fields are additive.
    #[test]
    fn a_pin_file_without_the_new_fields_still_parses() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        std::fs::write(
            pin_path(data_dir),
            r#"{"superuser_pass":"a","repl_pass":"b","app_pass":"c"}"#,
        )
        .unwrap();
        let pin = read_credential_pin(data_dir).expect("an old pin must still parse");
        assert_eq!(pin.superuser_pass, "a");
        assert_eq!(pin.etcd_pass, None);
        assert_eq!(pin.restapi_pass, None);
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
                etcd_pass: None,
                restapi_pass: None,
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
