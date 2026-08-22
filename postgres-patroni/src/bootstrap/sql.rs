//! SQL execution helpers for post-bootstrap

use anyhow::{anyhow, Context, Result};
use std::env;
use std::io::Write;
use std::process::{Command, Stdio};

/// Quote `s` as a SQL string literal: every `'` doubled, wrapped in `'…'`.
///
/// Postgres runs with standard-conforming strings (the default since 9.1),
/// so backslashes are ordinary characters inside `'…'` and need no
/// treatment — doubling quotes is the complete escape.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Quote `s` as a SQL identifier: every `"` doubled, wrapped in `"…"`.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Pick a dollar-quote tag guaranteed to appear in none of `values`, so a
/// value spliced into the body of a `DO` block (even one containing `$$` or
/// a would-be tag) can never terminate the body early. Deterministic and
/// dependency-free: starts at `$rw_bootstrap$` and grows until nothing
/// collides. The grown tag stays a valid dollar-quote tag (letters and
/// underscores only).
pub fn dollar_quote_tag(values: &[&str]) -> String {
    let mut inner = String::from("rw_bootstrap");
    loop {
        let tag = format!("${inner}$");
        if !values.iter().any(|v| v.contains(&tag)) {
            return tag;
        }
        inner.push('x');
    }
}

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

#[cfg(test)]
mod tests {
    use super::{dollar_quote_tag, quote_ident, quote_literal};

    #[test]
    fn quote_literal_wraps_and_doubles_quotes() {
        assert_eq!(quote_literal("plain"), "'plain'");
        assert_eq!(quote_literal(""), "''");
        assert_eq!(quote_literal("pa'ss"), "'pa''ss'");
        // A quote-only payload can't close the literal early.
        assert_eq!(quote_literal("'"), "''''");
        assert_eq!(
            quote_literal("'; DROP ROLE postgres; --"),
            "'''; DROP ROLE postgres; --'"
        );
    }

    #[test]
    fn quote_literal_leaves_backslashes_and_dollars_alone() {
        // standard_conforming_strings: backslash is literal inside '…'.
        assert_eq!(quote_literal(r"back\slash"), r"'back\slash'");
        // `$$` inside a quoted literal is inert; the dollar-quote tag of the
        // surrounding DO body is what must avoid it (see dollar_quote_tag).
        assert_eq!(quote_literal("pa$$word"), "'pa$$word'");
    }

    #[test]
    fn quote_ident_wraps_and_doubles_double_quotes() {
        assert_eq!(quote_ident("mydb"), "\"mydb\"");
        assert_eq!(quote_ident("my\"db"), "\"my\"\"db\"");
        // Single quotes are ordinary characters in an identifier.
        assert_eq!(quote_ident("o'db"), "\"o'db\"");
    }

    #[test]
    fn dollar_quote_tag_defaults_when_nothing_collides() {
        assert_eq!(
            dollar_quote_tag(&["password", "pa$$word", "user"]),
            "$rw_bootstrap$"
        );
    }

    #[test]
    fn dollar_quote_tag_grows_past_colliding_values() {
        // A value that embeds the default tag would end the DO body early;
        // the tag must grow until no value contains it.
        let tag = dollar_quote_tag(&["evil$rw_bootstrap$payload"]);
        assert_eq!(tag, "$rw_bootstrapx$");
        let tag = dollar_quote_tag(&["evil$rw_bootstrap$payload", "$rw_bootstrapx$too"]);
        assert_eq!(tag, "$rw_bootstrapxx$");
        // And the result really is absent from every value.
        for v in ["evil$rw_bootstrap$payload", "$rw_bootstrapx$too"] {
            assert!(!v.contains(&tag));
        }
    }
}
