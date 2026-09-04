//! Command execution utilities
//!
//! Provides consistent command execution with proper error handling and logging.

use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, instrument};

/// Environment variable holding the etcd `root` password. When set, every
/// `etcdctl` call authenticates as root — accepted by an etcd cluster with
/// authentication enabled, and tolerated by one without.
pub const ETCD_ROOT_PASSWORD_ENV: &str = "ETCD_ROOT_PASSWORD";

/// The `--user=root:<password>` argument to prepend, if a root password is set.
pub(crate) fn etcdctl_auth_arg(root_password: Option<&str>) -> Option<String> {
    root_password
        .filter(|p| !p.trim().is_empty())
        .map(|p| format!("--user=root:{p}"))
}

fn etcdctl_args(args: &[&str]) -> Vec<String> {
    let password = std::env::var(ETCD_ROOT_PASSWORD_ENV).ok();
    let mut full: Vec<String> = etcdctl_auth_arg(password.as_deref()).into_iter().collect();
    full.extend(args.iter().map(|a| a.to_string()));
    full
}

/// Arguments as logged: any `--user=<name>:<password>` keeps only the name.
pub(crate) fn redact_args(args: &[&str]) -> Vec<String> {
    args.iter()
        .map(|a| match a.strip_prefix("--user=") {
            Some(rest) => match rest.split_once(':') {
                Some((user, _)) => format!("--user={user}:***"),
                None => a.to_string(),
            },
            None => a.to_string(),
        })
        .collect()
}

/// Result of a command execution.
#[derive(Debug)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub code: Option<i32>,
}

/// Run a command and return its output.
///
/// This is a low-level function that returns both stdout and stderr.
/// Use `run_checked` if you want to treat non-zero exit as an error.
#[instrument(skip_all, fields(cmd = %cmd))]
pub async fn run(cmd: &str, args: &[&str]) -> Result<CommandOutput> {
    debug!(args = ?redact_args(args), "Running command");

    let output = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .context(format!("Failed to execute {}", cmd))?;

    Ok(CommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        success: output.status.success(),
        code: output.status.code(),
    })
}

/// Run a command and return stdout if successful, error otherwise.
///
/// # Example
/// ```ignore
/// let version = run_checked("postgres", &["--version"]).await?;
/// ```
pub async fn run_checked(cmd: &str, args: &[&str]) -> Result<String> {
    let output = run(cmd, args).await?;
    if output.success {
        Ok(output.stdout)
    } else {
        let code = output
            .code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        Err(anyhow!("{} failed (exit {}): {}", cmd, code, output.stderr))
    }
}

/// Run a command with sudo.
///
/// # Example
/// ```ignore
/// sudo(&["chown", "postgres:postgres", "/data"]).await?;
/// ```
pub async fn sudo(args: &[&str]) -> Result<String> {
    run_checked("sudo", args).await
}

/// Run an etcdctl command.
///
/// # Example
/// ```ignore
/// let members = etcdctl(&["member", "list"]).await?;
/// ```
pub async fn etcdctl(args: &[&str]) -> Result<String> {
    let full = etcdctl_args(args);
    let refs: Vec<&str> = full.iter().map(String::as_str).collect();
    run_checked("etcdctl", &refs).await
}

/// Probe with etcdctl - returns Ok(true) if healthy, Ok(false) if unhealthy.
///
/// Unlike `etcdctl`, this distinguishes spawn errors (Err) from
/// endpoint-unhealthy errors (Ok(false)).
///
/// Use this for health probing where you want to try multiple endpoints.
pub async fn etcdctl_probe(args: &[&str]) -> Result<bool> {
    let full = etcdctl_args(args);
    let refs: Vec<&str> = full.iter().map(String::as_str).collect();
    let output = run("etcdctl", &refs).await?;
    Ok(output.success)
}

/// `GET /health` on an etcd client endpoint (`host:port` or `http://host:port`).
///
/// etcd serves `/health` outside its RBAC layer, so this keeps answering while
/// authentication is enabled on the cluster — unlike `etcdctl endpoint health`,
/// which reads a key and reports unhealthy without a credential. Ok(false) on
/// any connection or non-healthy answer; Err only if no HTTP client can be built.
pub async fn etcd_http_health(endpoint: &str) -> Result<bool> {
    let base = if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", endpoint.trim_end_matches('/'))
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .context("build etcd health client")?;
    let Ok(resp) = client.get(format!("{base}/health")).send().await else {
        return Ok(false);
    };
    if !resp.status().is_success() {
        return Ok(false);
    }
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return Ok(false);
    };
    Ok(body.get("health").and_then(|h| h.as_str()) == Some("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_arg_only_with_a_non_empty_password() {
        assert_eq!(
            etcdctl_auth_arg(Some("s3cret")),
            Some("--user=root:s3cret".to_string())
        );
        assert_eq!(etcdctl_auth_arg(Some("  ")), None);
        assert_eq!(etcdctl_auth_arg(None), None);
    }

    #[test]
    fn redaction_hides_the_password_but_keeps_the_user() {
        let args = [
            "--user=root:s3cret",
            "member",
            "list",
            "--endpoints=127.0.0.1:2379",
        ];
        assert_eq!(
            redact_args(&args),
            vec![
                "--user=root:***",
                "member",
                "list",
                "--endpoints=127.0.0.1:2379"
            ]
        );
        assert_eq!(redact_args(&["--user=root"]), vec!["--user=root"]);
    }
}

/// Run an openssl command.
///
/// # Example
/// ```ignore
/// let cert_info = openssl(&["x509", "-in", "cert.pem", "-text"]).await?;
/// ```
pub async fn openssl(args: &[&str]) -> Result<String> {
    run_checked("openssl", args).await
}

/// Run a psql command.
///
/// # Example
/// ```ignore
/// let result = psql(&["-c", "SELECT 1"]).await?;
/// ```
pub async fn psql(args: &[&str]) -> Result<String> {
    run_checked("psql", args).await
}
