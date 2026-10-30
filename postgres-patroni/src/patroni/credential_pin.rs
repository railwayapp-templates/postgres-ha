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
//! visible. A fresh volume (first boot, scale-up) takes the variables as-is,
//! exactly as before. A volume adopted from vanilla data
//! (`PATRONI_ADOPT_EXISTING_DATA=true`) is a cluster from the boot that adopts
//! it: Patroni never bootstraps it, so `post_bootstrap` never pins it, and the
//! runner adopts the variables as its pin on that same boot instead.
//!
//! The pin lives inside PGDATA on purpose: `pg_basebackup` and pgBackRest copy
//! it into clones and backups, so a replica seeded from a pinned leader boots
//! coherent with it (the same reason the forced-archive-GUCs sentinel lives
//! there). The volume keeps a copy of it, and of a staged rotation's journal,
//! outside PGDATA ([`pin_copy_path`], [`journal_copy_path`]): a reinitialize
//! empties PGDATA, and a member that boots mid-rotation on an empty PGDATA
//! must still know what it was pinned to and what it is carrying out, or it
//! presents the stale variables to etcd and is stopped. PGDATA's own files win
//! whenever they exist; the copies are read only when it has none. Existing clusters that predate the pin adopt their current
//! variables as the pin on the first boot of this image — at that point the
//! variables are, by construction, the credentials the cluster runs with.
//!
//! Break-glass: `PATRONI_CREDENTIALS_FROM_ENV=true` makes the variables win
//! for that boot and re-pins them — for an operator who has ALREADY rotated
//! the roles with `ALTER ROLE` and now wants the variables to follow.

use anyhow::{Context, Result};
use common::{ConfigExt, Telemetry, TelemetryEvent};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use tracing::{info, warn};

use super::config::Config;

/// File inside PGDATA holding the pinned credentials.
pub const CREDENTIAL_PIN_FILE: &str = ".railway_credentials";

/// File inside PGDATA holding the request of a staged rotation, from its
/// `prepare` step until `verify` removes it.
pub const ROTATION_JOURNAL_FILE: &str = ".railway_rotation";

/// The volume's copy of the pin, at the volume root, outside PGDATA. A
/// reinitialize, and the runner's incomplete-clone wipe, empty PGDATA and with
/// it the pin and the journal. A member that boots in that state in the middle
/// of a rotation (PGDATA wiped, the clone not yet complete) knows neither the
/// credentials it was pinned to nor the rotation it is carrying out, so it
/// presents the stale variables to etcd, whose root password the rotation has
/// already moved, and the credential pre-flight stops it; every later boot
/// finds the same empty PGDATA and stops the same way, until the rotation
/// fails. The copies survive the wipe. They are fallbacks: PGDATA's own files
/// win whenever they exist (a completed clone brings the leader's). Distinct
/// names, so a volume whose PGDATA is the volume root keeps the two apart.
pub const CREDENTIAL_PIN_COPY_FILE: &str = ".railway_credentials.volume";
/// The volume's copy of the rotation journal; see [`CREDENTIAL_PIN_COPY_FILE`].
pub const ROTATION_JOURNAL_COPY_FILE: &str = ".railway_rotation.volume";

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

pub fn pin_copy_path(volume_root: &str) -> String {
    format!("{volume_root}/{CREDENTIAL_PIN_COPY_FILE}")
}

pub fn journal_path(data_dir: &str) -> String {
    format!("{data_dir}/{ROTATION_JOURNAL_FILE}")
}

pub fn journal_copy_path(volume_root: &str) -> String {
    format!("{volume_root}/{ROTATION_JOURNAL_COPY_FILE}")
}

/// Which file a boot took its pin from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinSource {
    /// PGDATA's own pin.
    Pgdata,
    /// PGDATA has no pin (wiped, or a clone still in progress): the volume's copy.
    VolumeCopy,
}

/// The pin a boot reconciles against: PGDATA's own when it has one, else the
/// volume's copy. None when neither exists (a fresh volume).
pub fn pin_for_boot(data_dir: &str, volume_root: &str) -> Option<(PinnedCredentials, PinSource)> {
    if let Some(pin) = read_credential_pin(data_dir) {
        return Some((pin, PinSource::Pgdata));
    }
    read_credential_pin_copy(volume_root).map(|pin| (pin, PinSource::VolumeCopy))
}

/// The pinned credentials on this volume, or None on a fresh volume. An
/// unreadable or unparseable pin is logged and treated as absent — the
/// variables then apply, which is the pre-pin behaviour, never a crash.
pub fn read_credential_pin(data_dir: &str) -> Option<PinnedCredentials> {
    read_pin_file(&pin_path(data_dir))
}

/// The volume's copy of the pin (see [`CREDENTIAL_PIN_COPY_FILE`]), or None.
pub fn read_credential_pin_copy(volume_root: &str) -> Option<PinnedCredentials> {
    read_pin_file(&pin_copy_path(volume_root))
}

fn read_pin_file(path: &str) -> Option<PinnedCredentials> {
    let content = match std::fs::read_to_string(path) {
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
    let json = serde_json::to_string_pretty(creds).context("serialize credential pin")?;
    write_private(&pin_path(data_dir), json.as_bytes())
}

/// Persist the volume's copy of the pin (see [`CREDENTIAL_PIN_COPY_FILE`]).
pub fn write_credential_pin_copy(volume_root: &str, creds: &PinnedCredentials) -> Result<()> {
    let json = serde_json::to_string_pretty(creds).context("serialize credential pin")?;
    write_private(&pin_copy_path(volume_root), json.as_bytes())
}

/// Bring the volume's copy up to date with `pin` when it differs: the first
/// boot of this image on an already pinned cluster, a replica whose pin came
/// with its clone and that never rebooted since, or a copy write that failed.
/// Best-effort, like every copy write.
pub fn sync_credential_pin_copy(volume_root: &str, pin: &PinnedCredentials) {
    if read_credential_pin_copy(volume_root).as_ref() == Some(pin) {
        return;
    }
    if let Err(e) = write_credential_pin_copy(volume_root, pin) {
        warn!(error = %e, volume_root, "could not write the volume's copy of the credential pin");
    }
}

/// Pin `creds` in PGDATA and keep the volume's copy current. PGDATA's write is
/// the result; a copy that cannot be written is logged, never fatal: the copy
/// is a fallback for a boot on an empty PGDATA, not the pin itself.
pub fn write_credential_pin_and_copy(
    data_dir: &str,
    volume_root: &str,
    creds: &PinnedCredentials,
) -> Result<()> {
    write_credential_pin(data_dir, creds)?;
    if let Err(e) = write_credential_pin_copy(volume_root, creds) {
        warn!(error = %e, volume_root, "could not write the volume's copy of the credential pin");
    }
    Ok(())
}

/// Record a staged rotation's request in PGDATA and at the volume root. As
/// with the pin, PGDATA's write is the result and the copy is best-effort.
pub fn write_rotation_journal(data_dir: &str, volume_root: &str, body: &[u8]) -> Result<()> {
    write_private(&journal_path(data_dir), body)?;
    if let Err(e) = write_private(&journal_copy_path(volume_root), body) {
        warn!(error = %e, volume_root, "could not write the volume's copy of the rotation journal");
    }
    Ok(())
}

/// The staged rotation this volume carries, if any: PGDATA's journal when it
/// has one, else the volume's copy (the boot after a wipe, before the clone
/// brings the leader's).
pub fn read_rotation_journal(data_dir: &str, volume_root: &str) -> Option<Vec<u8>> {
    match std::fs::read(journal_path(data_dir)) {
        Ok(body) => return Some(body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(error = %e, "rotation journal unreadable; trying the volume's copy"),
    }
    match std::fs::read(journal_copy_path(volume_root)) {
        Ok(body) => {
            info!("PGDATA carries no rotation journal; using the volume's copy");
            Some(body)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            warn!(error = %e, "the volume's copy of the rotation journal is unreadable");
            None
        }
    }
}

/// A rotation that has been verified (or restored) leaves no journal anywhere.
pub fn clear_rotation_journal(data_dir: &str, volume_root: &str) -> Result<()> {
    remove_if_present(&journal_path(data_dir))?;
    if let Err(e) = remove_if_present(&journal_copy_path(volume_root)) {
        warn!(error = %e, volume_root, "could not remove the volume's copy of the rotation journal");
    }
    Ok(())
}

fn remove_if_present(path: &str) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {path}")),
    }
}

/// Write `body` to `path` atomically (temp file + rename), mode 0600, and
/// fsync the directory so the rename is durable.
fn write_private(path: &str, body: &[u8]) -> Result<()> {
    let tmp = format!("{path}.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(body)?;
    file.sync_all()?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod {tmp}"))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {tmp} -> {path}"))?;
    let dir = std::path::Path::new(path)
        .parent()
        .context("path without a parent")?;
    std::fs::File::open(dir)?.sync_all()?;
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
/// bootstrap marker (written earlier, or by this boot's adoption)" test — a
/// volume that carries a Patroni-managed cluster, as opposed to an empty
/// volume or stale data about to be cloned over. `credentials_from_env` is
/// [`credentials_from_env_requested`].
pub fn apply_credential_pin(
    config: &mut Config,
    volume_root: &str,
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
        match write_credential_pin_and_copy(&config.data_dir, volume_root, &from_env) {
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
        match write_credential_pin_and_copy(&config.data_dir, volume_root, &from_env) {
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

    sync_credential_pin_copy(volume_root, &pinned);

    if pinned == from_env {
        return PinOutcome::Matches;
    }

    let overridden = drifted_variables(&pinned, &from_env);
    config.superuser_pass = pinned.superuser_pass;
    config.repl_pass = pinned.repl_pass;
    config.app_pass = pinned.app_pass;

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

/// The variables whose value differs from the pinned password of the same
/// role — the list [`PinOutcome::KeptPinned`] carries.
pub fn drifted_variables(
    pinned: &PinnedCredentials,
    from_env: &PinnedCredentials,
) -> Vec<&'static str> {
    let mut drifted = Vec::new();
    if pinned.superuser_pass != from_env.superuser_pass {
        drifted.push("PATRONI_SUPERUSER_PASSWORD");
    }
    if pinned.repl_pass != from_env.repl_pass {
        drifted.push("PATRONI_REPLICATION_PASSWORD");
    }
    if pinned.app_pass != from_env.app_pass {
        drifted.push("POSTGRES_PASSWORD");
    }
    drifted
}

/// The variables that differ from the pin on this volume, without applying
/// the pin — for boot-time checks that run before [`apply_credential_pin`]
/// (the credential pre-flights run ahead of the reseed handling, which may
/// wipe the data directory the pin lives in). Empty on a fresh volume, when
/// the pin cannot be read, and under `PATRONI_CREDENTIALS_FROM_ENV=true`,
/// where the variables are declared right by the operator. `config` must be
/// pre-pin: its passwords are the variables' values.
pub fn credential_drift(config: &Config, credentials_from_env: bool) -> Vec<&'static str> {
    if credentials_from_env {
        return Vec::new();
    }
    match read_credential_pin(&config.data_dir) {
        Some(pinned) => drifted_variables(&pinned, &PinnedCredentials::from_config(config)),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patroni::RestapiAddressSource;
    use tempfile::tempdir;

    #[test]
    fn drifted_variables_names_each_role_whose_variable_moved() {
        let pinned = PinnedCredentials {
            superuser_pass: "su".into(),
            repl_pass: "repl".into(),
            app_pass: "app".into(),
        };
        let mut from_env = pinned.clone();
        assert!(drifted_variables(&pinned, &from_env).is_empty());
        from_env.superuser_pass = "edited".into();
        from_env.app_pass = "edited".into();
        assert_eq!(
            drifted_variables(&pinned, &from_env),
            vec!["PATRONI_SUPERUSER_PASSWORD", "POSTGRES_PASSWORD"]
        );
        from_env.repl_pass = "edited".into();
        assert_eq!(drifted_variables(&pinned, &from_env).len(), 3);
    }

    #[test]
    fn credential_drift_reads_the_pin_without_applying_it() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap().to_string();
        let config = config_at(&data_dir);

        // Fresh volume: nothing to compare against.
        assert!(credential_drift(&config, false).is_empty());

        write_credential_pin(
            &data_dir,
            &PinnedCredentials {
                superuser_pass: "su-pinned".into(),
                repl_pass: "repl-env".into(),
                app_pass: "app-env".into(),
            },
        )
        .unwrap();
        assert_eq!(
            credential_drift(&config, false),
            vec!["PATRONI_SUPERUSER_PASSWORD"]
        );
        // The config's passwords are untouched: applying the pin is
        // apply_credential_pin's job.
        assert_eq!(config.superuser_pass, "su-env");
        // Break-glass: the operator declared the variables right.
        assert!(credential_drift(&config, true).is_empty());
    }

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
    fn the_volume_copy_is_read_only_when_pgdata_has_no_pin() {
        let pgdata = tempdir().unwrap();
        let volume = tempdir().unwrap();
        let (data_dir, volume_root) = (
            pgdata.path().to_str().unwrap(),
            volume.path().to_str().unwrap(),
        );
        assert_eq!(pin_for_boot(data_dir, volume_root), None);
        write_credential_pin_and_copy(data_dir, volume_root, &pin_of("one")).unwrap();
        assert_eq!(
            pin_for_boot(data_dir, volume_root),
            Some((pin_of("one"), PinSource::Pgdata))
        );
        let mode = std::fs::metadata(pin_copy_path(volume_root))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // A clone brought a newer pin: PGDATA wins over a copy that fell behind.
        write_credential_pin(data_dir, &pin_of("two")).unwrap();
        assert_eq!(
            pin_for_boot(data_dir, volume_root),
            Some((pin_of("two"), PinSource::Pgdata))
        );
        // The reinitialize wiped PGDATA: the copy is what the boot has.
        std::fs::remove_file(pin_path(data_dir)).unwrap();
        assert_eq!(
            pin_for_boot(data_dir, volume_root),
            Some((pin_of("one"), PinSource::VolumeCopy))
        );
    }

    #[test]
    fn a_staged_rotation_journal_survives_a_pgdata_wipe() {
        let pgdata = tempdir().unwrap();
        let volume = tempdir().unwrap();
        let (data_dir, volume_root) = (
            pgdata.path().to_str().unwrap(),
            volume.path().to_str().unwrap(),
        );
        assert_eq!(read_rotation_journal(data_dir, volume_root), None);
        write_rotation_journal(data_dir, volume_root, b"{\"newPassword\":\"n\"}").unwrap();
        assert_eq!(
            read_rotation_journal(data_dir, volume_root).as_deref(),
            Some(&b"{\"newPassword\":\"n\"}"[..])
        );
        for entry in std::fs::read_dir(data_dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        assert_eq!(
            read_rotation_journal(data_dir, volume_root).as_deref(),
            Some(&b"{\"newPassword\":\"n\"}"[..])
        );
        // verify ran: nothing is left anywhere, and clearing twice is fine.
        clear_rotation_journal(data_dir, volume_root).unwrap();
        clear_rotation_journal(data_dir, volume_root).unwrap();
        assert_eq!(read_rotation_journal(data_dir, volume_root), None);
        assert!(!std::path::Path::new(&journal_copy_path(volume_root)).exists());
    }

    #[test]
    fn an_existing_pin_gets_its_volume_copy_on_the_next_boot() {
        // A cluster pinned by an image without the copy: the first boot of this
        // image writes it, so the next wipe already has it.
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let mut config = config_at(data_dir);
        write_credential_pin(data_dir, &PinnedCredentials::from_config(&config)).unwrap();
        assert_eq!(read_credential_pin_copy(data_dir), None);
        assert_eq!(
            apply_credential_pin(&mut config, data_dir, true, false, &telemetry()),
            PinOutcome::Matches
        );
        assert_eq!(
            read_credential_pin_copy(data_dir),
            Some(PinnedCredentials::from_config(&config))
        );
    }

    fn pin_of(password: &str) -> PinnedCredentials {
        PinnedCredentials {
            superuser_pass: password.into(),
            repl_pass: password.into(),
            app_pass: password.into(),
        }
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
            apply_credential_pin(&mut config, data_dir, false, false, &telemetry()),
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
        let outcome = apply_credential_pin(&mut config, data_dir, false, false, &telemetry());
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
    fn volume_adopted_this_boot_pins_the_variables_before_patroni_starts() {
        // PATRONI_ADOPT_EXISTING_DATA=true over vanilla data: the runner just
        // wrote the bootstrap marker, Patroni will adopt (not bootstrap) the
        // data, and post_bootstrap will never run — so the pin has to come
        // from this call, on this boot, or the converted cluster and every
        // replica cloned from it run unpinned until a restart.
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let mut config = config_at(data_dir);
        config.adopt_existing_data = true;
        assert_eq!(read_credential_pin(data_dir), None);
        assert_eq!(
            apply_credential_pin(&mut config, data_dir, true, false, &telemetry()),
            PinOutcome::Adopted
        );
        let pinned = read_credential_pin(data_dir).expect("pin written on the adopting boot");
        assert_eq!(pinned, PinnedCredentials::from_config(&config));
        assert_eq!(pinned.superuser_pass, "su-env");
        assert_eq!(pinned.repl_pass, "repl-env");
        assert_eq!(pinned.app_pass, "app-env");
    }

    #[test]
    fn cluster_without_a_pin_adopts_the_variables() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_str().unwrap();
        let mut config = config_at(data_dir);
        assert_eq!(
            apply_credential_pin(&mut config, data_dir, true, false, &telemetry()),
            PinOutcome::Adopted
        );
        assert_eq!(
            read_credential_pin(data_dir),
            Some(PinnedCredentials::from_config(&config))
        );
        // Second boot with the same variables is a no-op.
        assert_eq!(
            apply_credential_pin(&mut config, data_dir, true, false, &telemetry()),
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
        let outcome = apply_credential_pin(&mut config, data_dir, true, false, &telemetry());
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
        let outcome = apply_credential_pin(&mut config, data_dir, true, true, &telemetry());
        assert_eq!(outcome, PinOutcome::RepinnedFromEnv);
        assert_eq!(config.superuser_pass, "su-env");
        assert_eq!(
            read_credential_pin(data_dir),
            Some(PinnedCredentials::from_config(&config))
        );
    }
}
