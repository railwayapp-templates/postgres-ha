//! Environment variable parsing helpers
//!
//! Provides ergonomic helpers for reading configuration from environment variables.

use anyhow::{Context, Result};
use std::env;
use std::str::FromStr;

/// Extension trait for parsing environment variables.
///
/// Provides convenient methods for reading env vars with defaults, required values,
/// and type parsing.
pub trait ConfigExt {
    /// Get an environment variable with a default value.
    ///
    /// # Example
    /// ```ignore
    /// let port = String::env_or("PORT", "8080");
    /// ```
    fn env_or(name: &str, default: &str) -> String {
        env::var(name).unwrap_or_else(|_| default.to_string())
    }

    /// Get an environment variable with a default value, treating an empty or
    /// whitespace-only value as unset.
    ///
    /// For names such as a role name, where an empty string has no meaning and
    /// would otherwise reach the server as an empty user.
    ///
    /// # Example
    /// ```ignore
    /// let user = String::env_non_empty_or("PGUSER", "postgres");
    /// ```
    fn env_non_empty_or(name: &str, default: &str) -> String {
        non_empty_or(env::var(name).ok(), default)
    }

    /// Get a required environment variable, returning an error if not set.
    ///
    /// # Example
    /// ```ignore
    /// let db_url = String::env_required("DATABASE_URL")?;
    /// ```
    fn env_required(name: &str) -> Result<String> {
        env::var(name).context(format!("{} must be set", name))
    }

    /// Get an environment variable as a boolean.
    ///
    /// Returns `true` if the value is "true" (case-insensitive), otherwise `default`.
    fn env_bool(name: &str, default: bool) -> bool {
        env::var(name)
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(default)
    }

    /// Get an environment variable parsed as a specific type.
    ///
    /// Returns `default` if the variable is not set or fails to parse.
    ///
    /// # Example
    /// ```ignore
    /// let max_conn: u32 = u32::env_parse("MAX_CONNECTIONS", 100);
    /// ```
    fn env_parse<T: FromStr>(name: &str, default: T) -> T {
        env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
}

// Blanket implementation for all types
impl<T> ConfigExt for T {}

/// `value` unless it is missing, empty, or whitespace-only; else `default`.
pub fn non_empty_or(value: Option<String>, default: &str) -> String {
    value
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Railway-specific environment helpers.
///
/// Provides easy access to Railway platform environment variables.
pub struct RailwayEnv;

impl RailwayEnv {
    /// Check if running on Railway platform.
    pub fn is_railway() -> bool {
        env::var("RAILWAY_ENVIRONMENT").is_ok()
    }

    /// Get the Railway project ID.
    pub fn project_id() -> String {
        env::var("RAILWAY_PROJECT_ID").unwrap_or_default()
    }

    /// Get the Railway environment ID.
    pub fn environment_id() -> String {
        env::var("RAILWAY_ENVIRONMENT_ID").unwrap_or_default()
    }

    /// Get the Railway service ID.
    pub fn service_id() -> String {
        env::var("RAILWAY_SERVICE_ID").unwrap_or_default()
    }

    /// Get the private domain for this service.
    pub fn private_domain() -> String {
        env::var("RAILWAY_PRIVATE_DOMAIN").unwrap_or_else(|_| "unknown".to_string())
    }

    /// Get the volume mount path, if any.
    pub fn volume_mount_path() -> Option<String> {
        env::var("RAILWAY_VOLUME_MOUNT_PATH").ok()
    }

    /// Get the GraphQL endpoint for telemetry.
    pub fn graphql_endpoint() -> String {
        env::var("RAILWAY_GRAPHQL_ENDPOINT")
            .unwrap_or_else(|_| "https://backboard.railway.app/graphql/internal".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_empty_or_treats_empty_as_unset() {
        assert_eq!(non_empty_or(Some(String::new()), "postgres"), "postgres");
    }

    #[test]
    fn non_empty_or_treats_whitespace_as_unset() {
        assert_eq!(
            non_empty_or(Some("  \t".to_string()), "postgres"),
            "postgres"
        );
    }

    #[test]
    fn non_empty_or_keeps_a_set_value() {
        assert_eq!(non_empty_or(Some("app".to_string()), "postgres"), "app");
    }

    #[test]
    fn non_empty_or_uses_default_when_missing() {
        assert_eq!(non_empty_or(None, "postgres"), "postgres");
    }

    #[test]
    fn env_non_empty_or_reads_an_empty_variable_as_unset() {
        // A name no other test reads, so no lock is needed.
        let name = "COMMON_CONFIG_TEST_ENV_NON_EMPTY_OR";
        env::set_var(name, "");
        assert_eq!(String::env_non_empty_or(name, "postgres"), "postgres");
        env::set_var(name, "app");
        assert_eq!(String::env_non_empty_or(name, "postgres"), "app");
        env::remove_var(name);
    }
}
