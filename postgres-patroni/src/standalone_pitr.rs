//! WAL archiving + backups for the image's STANDALONE mode (postgres-wrapper
//! with `PATRONI_ENABLED` unset).
//!
//! Standalone is where every HA cluster lands after "revert to standalone",
//! and some services run this image standalone from the start. The platform
//! keeps `WAL_ARCHIVE_*` set on those services and shows PITR as enabled, so
//! standalone has to run the same archive stack Patroni mode runs:
//!
//! - [`prepare`], in the root supervisor before Postgres starts: screen and
//!   translate the `WAL_ARCHIVE_*` contract, size the WAL budgets, render
//!   `/etc/pgbackrest/pgbackrest.conf`, resolve the per-cluster repo-path
//!   marker, and build the `-c` flags that switch archiving on for this
//!   postmaster. Command-line settings outrank both postgresql.conf and
//!   postgresql.auto.conf, so the values a former Patroni leader left in
//!   either are overridden consistently and nothing is persisted.
//! - [`run_sidecar`], in a separate process running as `postgres`: stanza
//!   bootstrap once the server is up and primary, then the backup watcher in
//!   [`WatcherMode::Standalone`]. It is a separate process because the
//!   supervisor reaps with `waitpid(-1)` as PID 1, which would steal the exit
//!   status of every `psql` / `pgbackrest` child the watcher runs.
//!
//! Every step is best-effort: a bad archive setting may cost the service its
//! backups, never its database. Failures are logged and sent as telemetry,
//! and the server boots exactly as it did before.
//!
//! With `WAL_ARCHIVE_BUCKET` unset nothing here runs.

use crate::patroni::{spawn_backup_watcher_with_mode, WatcherMode};
use crate::wal_archive::{
    compute_volume_thresholds, parse_archive_timeout_secs, render_pgbackrest_conf,
    spawn_bootstrap_stanza_create, translate_wal_env_to_pgbackrest, validate_wal_archive_bucket,
    wal_archive_enabled, ARCHIVE_PUSH_COMMAND,
};
use crate::{pgbackrest::derive_pgbackrest_repo_path, Telemetry, TelemetryEvent};
use std::env;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

/// argv[1] that turns postgres-wrapper into the standalone PITR sidecar.
pub const SIDECAR_ARG: &str = "__standalone-pitr-sidecar";

/// Outcome of [`prepare`]: what the supervisor adds to the postgres command
/// line, and whether it starts the sidecar.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StandalonePitr {
    /// Appended after the service's own postgres arguments.
    pub extra_args: Vec<String>,
    /// Start [`run_sidecar`] beside the server.
    pub run_sidecar: bool,
}

/// Why [`plan_archive_args`] declined to wire archiving.
#[derive(Debug, PartialEq, Eq)]
pub enum NotWired {
    /// The start command does not launch postgres through docker-entrypoint
    /// (a custom command), so there is no postmaster to hand flags to.
    NotAPostgresCommand,
    /// `wal_level=minimal` is in effect. `archive_mode=on` would make the
    /// postmaster refuse to start, so archiving stays off.
    WalLevelMinimal,
}

/// The settings standalone mode forces for archiving, in order. Same values
/// Patroni mode puts in its postgresql.parameters (see patroni/yaml.rs).
fn archive_settings(archive_timeout_secs: i64) -> Vec<(&'static str, String)> {
    vec![
        ("archive_mode", "on".to_string()),
        ("archive_command", ARCHIVE_PUSH_COMMAND.to_string()),
        ("archive_timeout", archive_timeout_secs.to_string()),
        // The PITR picker bounds its target by pg_last_committed_xact(),
        // which needs commit timestamps (mirrors Patroni mode).
        ("track_commit_timestamp", "on".to_string()),
    ]
}

/// True when docker-entrypoint.sh would run these arguments as the postgres
/// server: first argument `postgres`, or a leading option (the entrypoint
/// prepends `postgres` itself).
pub fn launches_postgres(args: &[String]) -> bool {
    match args.first() {
        Some(first) => first == "postgres" || first.starts_with('-'),
        None => false,
    }
}

/// Normalize a GUC name the way the server does: case-insensitive, and
/// `--archive-mode` style long options map dashes to underscores.
fn normalize_guc(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('-', "_")
}

/// Every `name=value` setting in a postgres argument list, in order:
/// `-c name=value`, `-cname=value` and `--name=value`.
fn settings_in_args(args: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let setting = if arg == "-c" {
            i += 1;
            args.get(i).map(String::as_str)
        } else {
            arg.strip_prefix("-c").or_else(|| arg.strip_prefix("--"))
        };
        if let Some((name, value)) = setting.and_then(|s| s.split_once('=')) {
            out.push((normalize_guc(name), value.trim().to_string()));
        }
        i += 1;
    }
    out
}

/// Last `wal_level` assignment in a postgresql.conf-format file, if any.
fn wal_level_in_conf(contents: &str) -> Option<String> {
    let mut found = None;
    for line in contents.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if normalize_guc(name) == "wal_level" {
            let value = value.trim().trim_matches(['\'', '"']).trim();
            found = Some(value.to_ascii_lowercase());
        }
    }
    found
}

/// The `wal_level` the postmaster will run with, by the server's own
/// precedence: command line over postgresql.auto.conf over postgresql.conf.
/// `None` means none of them sets it (the server default, `replica`).
fn effective_wal_level(args: &[String], conf: &str, auto_conf: &str) -> Option<String> {
    settings_in_args(args)
        .into_iter()
        .rev()
        .find(|(name, _)| name == "wal_level")
        .map(|(_, v)| v.trim_matches(['\'', '"']).to_ascii_lowercase())
        .or_else(|| wal_level_in_conf(auto_conf))
        .or_else(|| wal_level_in_conf(conf))
}

/// Decide the `-c` flags that switch archiving on for this postmaster.
///
/// A setting the service already passes on its own start command is left to
/// the service (its own value wins, as it always has); every other setting
/// is appended. `conf` / `auto_conf` are the contents of postgresql.conf and
/// postgresql.auto.conf (empty on a fresh volume).
pub fn plan_archive_args(
    args: &[String],
    conf: &str,
    auto_conf: &str,
    archive_timeout_secs: i64,
) -> Result<Vec<String>, NotWired> {
    if !launches_postgres(args) {
        return Err(NotWired::NotAPostgresCommand);
    }
    if effective_wal_level(args, conf, auto_conf).as_deref() == Some("minimal") {
        return Err(NotWired::WalLevelMinimal);
    }
    let customer_set: Vec<String> = settings_in_args(args)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let mut extra = Vec::new();
    for (name, value) in archive_settings(archive_timeout_secs) {
        if customer_set.iter().any(|n| n == name) {
            continue;
        }
        extra.push("-c".to_string());
        extra.push(format!("{name}={value}"));
    }
    Ok(extra)
}

fn report(telemetry: &Telemetry, error: String) {
    warn!("standalone PITR: {error}");
    telemetry.send(TelemetryEvent::ComponentError {
        component: "postgres-wrapper".to_string(),
        error,
        context: "standalone PITR setup (non-fatal; the database boots without archiving)"
            .to_string(),
    });
}

/// Hand a file the supervisor (root) wrote to the `postgres` user, which is
/// who runs `archive_command`, the sidecar, and the admin probes that read
/// these files. No-op when not root or the file is absent.
fn chown_to_postgres(path: &str) {
    if !nix::unistd::geteuid().is_root() || !Path::new(path).exists() {
        return;
    }
    let user = match nix::unistd::User::from_name("postgres") {
        Ok(Some(u)) => u,
        _ => {
            warn!(
                path,
                "standalone PITR: no postgres user to hand the file to"
            );
            return;
        }
    };
    if let Err(e) = std::os::unix::fs::chown(path, Some(user.uid.as_raw()), Some(user.gid.as_raw()))
    {
        warn!(path, error = %e, "standalone PITR: chown to postgres failed");
    }
}

/// Standalone archive setup, run by the supervisor before it starts
/// Postgres. `args` is the service's postgres argument list. Never fails:
/// every problem is logged + reported and leaves archiving off.
pub fn prepare(
    pgdata: &str,
    volume_root: &str,
    args: &[String],
    telemetry: &Telemetry,
) -> StandalonePitr {
    if !wal_archive_enabled() {
        return StandalonePitr::default();
    }

    // Same screen Patroni mode runs; a junk bucket unsets WAL_ARCHIVE_* and
    // drops the sentinel the admin monitor reads.
    validate_wal_archive_bucket(volume_root);
    chown_to_postgres(&format!("{volume_root}/.pgbackrest_invalid_bucket"));
    if !wal_archive_enabled() {
        return StandalonePitr::default();
    }

    let read = |name: &str| std::fs::read_to_string(format!("{pgdata}/{name}")).unwrap_or_default();
    let timeout = parse_archive_timeout_secs(env::var("POSTGRES_ARCHIVE_TIMEOUT").ok().as_deref());
    let extra_args = match plan_archive_args(
        args,
        &read("postgresql.conf"),
        &read("postgresql.auto.conf"),
        timeout,
    ) {
        Ok(extra) => extra,
        Err(NotWired::NotAPostgresCommand) => {
            report(
                telemetry,
                "WAL_ARCHIVE_BUCKET is set but the start command does not launch postgres; \
                 archiving stays off"
                    .to_string(),
            );
            return StandalonePitr::default();
        }
        Err(NotWired::WalLevelMinimal) => {
            report(
                telemetry,
                "WAL_ARCHIVE_BUCKET is set but wal_level=minimal is in effect, which cannot \
                 archive; archiving stays off (set wal_level to replica or logical to enable it)"
                    .to_string(),
            );
            return StandalonePitr::default();
        }
    };

    translate_wal_env_to_pgbackrest();

    // The archive_command wrapper reads WAL_DROP_THRESHOLD_MB from its
    // environment, inherited from here through docker-entrypoint.
    let (wal_drop_mib, queue_max_mib) = compute_volume_thresholds(volume_root);
    if env::var("WAL_DROP_THRESHOLD_MB")
        .ok()
        .filter(|s| !s.is_empty())
        .is_none()
    {
        env::set_var("WAL_DROP_THRESHOLD_MB", wal_drop_mib.to_string());
    }

    if let Err(e) = render_pgbackrest_conf(pgdata, queue_max_mib) {
        report(
            telemetry,
            format!("rendering pgbackrest.conf failed: {e:#}"),
        );
        return StandalonePitr::default();
    }
    chown_to_postgres("/etc/pgbackrest/pgbackrest.conf");

    // Per-cluster repo path: an existing marker wins (a reverted leader
    // keeps archiving where it always has), otherwise cluster-<sysid>. On a
    // fresh volume pg_control appears only after initdb; the sidecar's
    // stanza bootstrap writes the marker then.
    if Path::new(&format!("{pgdata}/global/pg_control")).exists() {
        let repo_path = derive_pgbackrest_repo_path(pgdata);
        chown_to_postgres(&format!("{pgdata}/.pgbackrest_repo_path"));
        env::set_var("PGBACKREST_REPO1_PATH", &repo_path);
        info!(repo_path = %repo_path, "standalone PITR: using per-cluster repo1-path");
    }

    info!(
        args = ?extra_args,
        "standalone PITR: archiving enabled for this server"
    );
    StandalonePitr {
        extra_args,
        run_sidecar: true,
    }
}

/// Body of the sidecar process: stanza bootstrap once the server is up and
/// primary, then the standalone backup watcher. Runs until the container
/// stops; both tasks retry on their own.
pub async fn run_sidecar(pgdata: String) {
    info!("standalone PITR sidecar: starting stanza bootstrap and backup watcher");
    spawn_bootstrap_stanza_create();
    spawn_backup_watcher_with_mode(pgdata, WatcherMode::Standalone);
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    const DEFAULT_CMD: &[&str] = &["postgres", "-p", "5432", "-c", "listen_addresses=*"];

    #[test]
    fn default_start_command_gets_the_full_archive_set() {
        let extra = plan_archive_args(&args(DEFAULT_CMD), "", "", 60).unwrap();
        assert_eq!(
            extra,
            args(&[
                "-c",
                "archive_mode=on",
                "-c",
                "archive_command=/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p",
                "-c",
                "archive_timeout=60",
                "-c",
                "track_commit_timestamp=on",
            ])
        );
    }

    #[test]
    fn archive_command_is_the_same_wrapper_patroni_mode_installs() {
        assert_eq!(
            ARCHIVE_PUSH_COMMAND,
            "/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p"
        );
    }

    #[test]
    fn patroni_written_values_in_the_conf_files_do_not_suppress_the_flags() {
        // A reverted leader's postgresql.conf / auto.conf already carry the
        // archive settings (possibly stale). The flags still go on the
        // command line so the running values are exactly ours.
        let conf = "archive_mode = 'on'\narchive_command = '/usr/local/bin/pgbackrest-archive-push-wrapper.sh %p'\n";
        let auto = "archive_timeout = '600'\n";
        let extra = plan_archive_args(&args(DEFAULT_CMD), conf, auto, 60).unwrap();
        assert!(extra.contains(&"archive_mode=on".to_string()));
        assert!(extra.contains(&"archive_timeout=60".to_string()));
    }

    #[test]
    fn archive_timeout_follows_the_configured_value() {
        let extra = plan_archive_args(&args(DEFAULT_CMD), "", "", 300).unwrap();
        assert!(extra.contains(&"archive_timeout=300".to_string()));
    }

    #[test]
    fn a_setting_on_the_services_own_start_command_is_left_alone() {
        for cmd in [
            &["postgres", "-c", "archive_timeout=900"][..],
            &["postgres", "-carchive_timeout=900"][..],
            &["postgres", "--archive-timeout=900"][..],
            &["postgres", "-c", "ARCHIVE_TIMEOUT=900"][..],
        ] {
            let extra = plan_archive_args(&args(cmd), "", "", 60).unwrap();
            assert!(
                !extra.iter().any(|a| a.starts_with("archive_timeout=")),
                "{cmd:?} -> {extra:?}"
            );
            assert!(extra.contains(&"archive_mode=on".to_string()), "{cmd:?}");
        }
    }

    #[test]
    fn leading_option_counts_as_a_postgres_command() {
        // docker-entrypoint prepends `postgres` when the first arg is an option.
        assert!(launches_postgres(&args(&["-c", "max_connections=200"])));
        assert!(plan_archive_args(&args(&["-c", "max_connections=200"]), "", "", 60).is_ok());
    }

    #[test]
    fn a_custom_non_postgres_command_is_not_wired() {
        assert_eq!(
            plan_archive_args(&args(&["bash", "-c", "sleep infinity"]), "", "", 60),
            Err(NotWired::NotAPostgresCommand)
        );
        assert_eq!(
            plan_archive_args(&[], "", "", 60),
            Err(NotWired::NotAPostgresCommand)
        );
    }

    #[test]
    fn wal_level_minimal_anywhere_in_effect_skips_wiring_so_the_boot_cannot_fail() {
        assert_eq!(
            plan_archive_args(&args(DEFAULT_CMD), "wal_level = minimal\n", "", 60),
            Err(NotWired::WalLevelMinimal)
        );
        assert_eq!(
            plan_archive_args(&args(DEFAULT_CMD), "", "wal_level = 'minimal'\n", 60),
            Err(NotWired::WalLevelMinimal)
        );
        let cmd = args(&["postgres", "-c", "wal_level=minimal"]);
        assert_eq!(
            plan_archive_args(&cmd, "", "", 60),
            Err(NotWired::WalLevelMinimal)
        );
    }

    #[test]
    fn wal_level_precedence_is_command_line_then_auto_conf_then_conf() {
        // auto.conf raises a minimal postgresql.conf → wired.
        assert!(plan_archive_args(
            &args(DEFAULT_CMD),
            "wal_level = minimal\n",
            "wal_level = 'replica'\n",
            60
        )
        .is_ok());
        // Command line raises a minimal auto.conf → wired.
        let cmd = args(&["postgres", "-c", "wal_level=logical"]);
        assert!(plan_archive_args(&cmd, "", "wal_level = 'minimal'\n", 60).is_ok());
        // Commented-out and later-overridden lines behave like the server.
        assert!(plan_archive_args(
            &args(DEFAULT_CMD),
            "#wal_level = minimal\nwal_level = minimal # old\nwal_level = replica\n",
            "",
            60
        )
        .is_ok());
    }

    #[test]
    fn archive_bucket_unset_changes_nothing() {
        // Serialized with nothing else touching WAL_ARCHIVE_BUCKET in this
        // test binary; prepare() must return before reading any file.
        let prev = env::var("WAL_ARCHIVE_BUCKET").ok();
        env::remove_var("WAL_ARCHIVE_BUCKET");
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_str().unwrap();
        let out = prepare(d, d, &args(DEFAULT_CMD), &Telemetry::from_env("test"));
        assert_eq!(out, StandalonePitr::default());
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
        if let Some(v) = prev {
            env::set_var("WAL_ARCHIVE_BUCKET", v);
        }
    }
}
