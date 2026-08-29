//! Configuration parsing for post-bootstrap
//!
//! Reads credentials from Patroni config file since the script
//! is run as a subprocess WITHOUT environment variables.

use anyhow::{Context, Result};
use serde::Deserialize;

/// Path to the Patroni configuration file
pub const PATRONI_CONFIG: &str = "/etc/patroni/patroni.yml";

/// Partial Patroni config - only fields we need for bootstrap
#[derive(Deserialize)]
struct PatroniConfig {
    postgresql: PostgresqlConfig,
}

#[derive(Deserialize)]
struct PostgresqlConfig {
    authentication: Authentication,
    #[serde(default)]
    app_user: AppUser,
    /// PGDATA as Patroni sees it — post-bootstrap runs without the
    /// environment, so this is the only trustworthy source for the data dir
    /// (where the credential pin lives).
    #[serde(default)]
    data_dir: String,
}

#[derive(Deserialize)]
struct Authentication {
    replication: UserCredentials,
    superuser: UserCredentials,
}

#[derive(Deserialize, Default)]
struct UserCredentials {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

#[derive(Deserialize, Default)]
struct AppUser {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    database: String,
}

/// Credentials extracted from Patroni configuration
pub struct Credentials {
    pub repl_user: String,
    pub repl_pass: String,
    pub superuser: String,
    pub superuser_pass: String,
    pub app_user: String,
    pub app_pass: String,
    pub app_db: String,
    /// `postgresql.data_dir` from the rendered config (empty if absent).
    pub data_dir: String,
}

/// Read credentials from the Patroni config file
pub fn read_credentials() -> Result<Credentials> {
    let content =
        std::fs::read_to_string(PATRONI_CONFIG).context("Failed to read Patroni config")?;

    let config: PatroniConfig =
        serde_yaml::from_str(&content).context("Failed to parse Patroni config")?;

    let pg = config.postgresql;
    Ok(Credentials {
        repl_user: pg.authentication.replication.username,
        repl_pass: pg.authentication.replication.password,
        superuser: pg.authentication.superuser.username,
        superuser_pass: pg.authentication.superuser.password,
        app_user: pg.app_user.username,
        app_pass: pg.app_user.password,
        app_db: pg.app_user.database,
        data_dir: pg.data_dir,
    })
}
