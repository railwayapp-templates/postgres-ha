//! SQL execution helpers for post-bootstrap

use anyhow::{anyhow, Context, Result};
use std::env;
use std::io::Write;
use std::process::{Command, Stdio};

/// Run a single SQL command via psql
pub fn run_psql(superuser: &str, sql: &str) -> Result<String> {
    let output = Command::new("env")
        .args(["-i"])
        .env("PATH", env::var("PATH").unwrap_or_default())
        .args([
            "psql",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            "/var/run/postgresql",
            "-U",
            superuser,
            "-d",
            "postgres",
            "-c",
            sql,
        ])
        .stdin(Stdio::null())
        .output()
        .context("Failed to run psql")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "psql failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Run a single SQL command via psql against a specific database
pub fn run_psql_in_db(superuser: &str, database: &str, sql: &str) -> Result<String> {
    let output = Command::new("env")
        .args(["-i"])
        .env("PATH", env::var("PATH").unwrap_or_default())
        .args([
            "psql",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            "/var/run/postgresql",
            "-U",
            superuser,
            "-d",
            database,
            "-c",
            sql,
        ])
        .stdin(Stdio::null())
        .output()
        .context("Failed to run psql")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "psql failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

/// Run a multi-line SQL script via psql
pub fn run_psql_script(superuser: &str, sql: &str) -> Result<String> {
    let mut child = Command::new("env")
        .args(["-i"])
        .env("PATH", env::var("PATH").unwrap_or_default())
        .args([
            "psql",
            "-v",
            "ON_ERROR_STOP=1",
            "-h",
            "/var/run/postgresql",
            "-U",
            superuser,
            "-d",
            "postgres",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn psql")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(sql.as_bytes())?;
    }

    let output = child.wait_with_output()?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "psql failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}
