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

/// etcdctl reads its persistent flags from `ETCDCTL_<FLAG>` variables
/// (`pkg/flags.SetPflagsFromEnv`, applied in `clientConfigFromCmd` for every
/// subcommand that opens a client). `ETCDCTL_USER` + `ETCDCTL_PASSWORD` are the
/// `--user` / `--password` flags without the password on the command line.
/// Setting the variable AND the flag is fatal in etcdctl ("conflicting
/// environment variable is shadowed by corresponding command-line flag"), so
/// the credential is carried one way only: here.
const ETCDCTL_USER_ENV: &str = "ETCDCTL_USER";
const ETCDCTL_PASSWORD_ENV: &str = "ETCDCTL_PASSWORD";

/// The environment that authenticates an `etcdctl` child as root, if a root
/// password is set. Environment rather than an argument: `/proc/<pid>/cmdline`
/// is readable by every process in the container, the child's environment only
/// by its own user.
pub(crate) fn etcdctl_auth_env(root_password: Option<&str>) -> Vec<(&'static str, String)> {
    match root_password.filter(|p| !p.trim().is_empty()) {
        Some(password) => vec![
            (ETCDCTL_USER_ENV, "root".to_string()),
            (ETCDCTL_PASSWORD_ENV, password.to_string()),
        ],
        None => Vec::new(),
    }
}

/// The argument list and environment for one `etcdctl` invocation: the
/// caller's arguments unchanged, the credential (if any) in the environment.
pub(crate) fn etcdctl_invocation<'a>(
    args: &[&'a str],
    root_password: Option<&str>,
) -> (Vec<&'a str>, Vec<(&'static str, String)>) {
    (args.to_vec(), etcdctl_auth_env(root_password))
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
pub async fn run(cmd: &str, args: &[&str]) -> Result<CommandOutput> {
    run_with_env(cmd, args, &[]).await
}

/// Like [`run`], with extra environment variables for the child. Their values
/// are never logged.
#[instrument(skip_all, fields(cmd = %cmd))]
pub async fn run_with_env(
    cmd: &str,
    args: &[&str],
    envs: &[(&str, String)],
) -> Result<CommandOutput> {
    debug!(args = ?redact_args(args), "Running command");

    let mut command = Command::new(cmd);
    command.args(args).stdin(Stdio::null());
    for (name, value) in envs {
        command.env(name, value);
    }
    let output = command
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
    run_checked_with_env(cmd, args, &[]).await
}

/// Like [`run_checked`], with extra environment variables for the child.
pub async fn run_checked_with_env(
    cmd: &str,
    args: &[&str],
    envs: &[(&str, String)],
) -> Result<String> {
    let output = run_with_env(cmd, args, envs).await?;
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

/// Run an etcdctl command, authenticating as root through the environment
/// when `ETCD_ROOT_PASSWORD` is set.
///
/// # Example
/// ```ignore
/// let members = etcdctl(&["member", "list"]).await?;
/// ```
pub async fn etcdctl(args: &[&str]) -> Result<String> {
    let password = std::env::var(ETCD_ROOT_PASSWORD_ENV).ok();
    let (args, envs) = etcdctl_invocation(args, password.as_deref());
    run_checked_with_env("etcdctl", &args, &envs).await
}

/// Probe with etcdctl - returns Ok(true) if healthy, Ok(false) if unhealthy.
///
/// Unlike `etcdctl`, this distinguishes spawn errors (Err) from
/// endpoint-unhealthy errors (Ok(false)).
///
/// Use this for health probing where you want to try multiple endpoints.
pub async fn etcdctl_probe(args: &[&str]) -> Result<bool> {
    let password = std::env::var(ETCD_ROOT_PASSWORD_ENV).ok();
    let (args, envs) = etcdctl_invocation(args, password.as_deref());
    let output = run_with_env("etcdctl", &args, &envs).await?;
    Ok(output.success)
}

/// `http://host:port` for an etcd client endpoint given as `host:port` or
/// already as a URL.
fn etcd_http_base(endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        endpoint.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", endpoint.trim_end_matches('/'))
    }
}

/// `GET /health` on an etcd client endpoint (`host:port` or `http://host:port`).
///
/// etcd serves `/health` outside its RBAC layer, so this keeps answering while
/// authentication is enabled on the cluster — unlike `etcdctl endpoint health`,
/// which reads a key and reports unhealthy without a credential. Ok(false) on
/// any connection or non-healthy answer; Err only if no HTTP client can be built.
pub async fn etcd_http_health(endpoint: &str) -> Result<bool> {
    let base = etcd_http_base(endpoint);
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

/// Body of `POST /v3/auth/user/add` on etcd's gRPC gateway.
pub(crate) fn etcd_user_add_body(name: &str, password: &str) -> serde_json::Value {
    serde_json::json!({ "name": name, "password": password })
}

/// Read a `user/add` answer: created, or already there, is done; anything else
/// (including "user name is empty", which is what the gateway says when
/// authentication got enabled in the meantime and the request carried no
/// token) is an error for the caller to retry.
pub(crate) fn etcd_user_add_outcome(status: u16, body: &str) -> Result<()> {
    if (200..300).contains(&status) || body.to_ascii_lowercase().contains("already exists") {
        return Ok(());
    }
    Err(anyhow!("etcd user add answered HTTP {status}: {body}"))
}

/// Create user `name` with `password` through the gRPC gateway on `endpoint`
/// (`host:port` or `http://host:port`), treating "already exists" as success.
///
/// The password travels in the JSON body only. `etcdctl user add name:password`
/// puts it on the command line; `etcdctl user add name --interactive=false`
/// reads it from stdin with `fmt.Scanf("%s")`, which stops at the first
/// whitespace and accepts an empty read as an empty password (measured on
/// etcdctl 3.6.6: `"a b"` became `"a"`, `"  lead"` became `"lead"`, no input
/// created the user with password `""`). The gateway takes the string as is.
/// Meant for the moment before authentication is enabled; once it is, the call
/// needs a token and the caller should have found the user in place already.
pub async fn etcd_http_user_add(endpoint: &str, name: &str, password: &str) -> Result<()> {
    let base = etcd_http_base(endpoint);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build etcd auth client")?;
    let resp = client
        .post(format!("{base}/v3/auth/user/add"))
        .json(&etcd_user_add_body(name, password))
        .send()
        .await
        .context("etcd user add request")?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    etcd_user_add_outcome(status, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_env_only_with_a_non_empty_password() {
        assert_eq!(
            etcdctl_auth_env(Some("s3cret")),
            vec![
                ("ETCDCTL_USER", "root".to_string()),
                ("ETCDCTL_PASSWORD", "s3cret".to_string())
            ]
        );
        assert!(etcdctl_auth_env(Some("  ")).is_empty());
        assert!(etcdctl_auth_env(None).is_empty());
    }

    #[test]
    fn the_password_never_reaches_the_command_line() {
        let (args, envs) = etcdctl_invocation(
            &["member", "list", "--endpoints=127.0.0.1:2379"],
            Some("s3cret"),
        );
        assert_eq!(args, vec!["member", "list", "--endpoints=127.0.0.1:2379"]);
        assert!(args
            .iter()
            .all(|a| !a.contains("s3cret") && !a.starts_with("--user")));
        assert_eq!(envs.len(), 2);
        assert_eq!(envs[1], ("ETCDCTL_PASSWORD", "s3cret".to_string()));
    }

    #[test]
    fn a_password_with_whitespace_is_carried_verbatim() {
        // Neither `etcdctl user add root:<pw>` nor `--interactive=false` (stdin
        // via Scanf) survives a space; the environment and the JSON body do.
        let (_, envs) = etcdctl_invocation(&["auth", "status"], Some("te st"));
        assert_eq!(envs[1].1, "te st");
        assert_eq!(
            etcd_user_add_body("root", "te st"),
            serde_json::json!({ "name": "root", "password": "te st" })
        );
        assert_eq!(
            etcd_user_add_body("root", "  lead").to_string(),
            r#"{"name":"root","password":"  lead"}"#
        );
    }

    #[test]
    fn user_add_outcome_is_done_when_created_or_already_there() {
        assert!(etcd_user_add_outcome(200, r#"{"header":{"cluster_id":"1"}}"#).is_ok());
        assert!(etcd_user_add_outcome(
            400,
            r#"{"code":9,"message":"etcdserver: user name already exists"}"#
        )
        .is_ok());
        assert!(etcd_user_add_outcome(
            400,
            r#"{"error":"etcdserver: user name already exists","code":9}"#
        )
        .is_ok());
    }

    #[test]
    fn user_add_outcome_fails_on_anything_else() {
        // Authentication was enabled by a peer between our status check and
        // this call: the gateway wants a token now. Retried next cycle.
        let err = etcd_user_add_outcome(
            400,
            r#"{"code":3,"message":"etcdserver: user name is empty"}"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("user name is empty"));
        assert!(etcd_user_add_outcome(503, "").is_err());
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

    #[test]
    fn http_base_accepts_bare_and_url_endpoints() {
        assert_eq!(etcd_http_base("127.0.0.1:2379"), "http://127.0.0.1:2379");
        assert_eq!(etcd_http_base("http://etcd-1:2379/"), "http://etcd-1:2379");
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
