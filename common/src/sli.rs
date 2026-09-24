//! Pieces every `sli` line shares.

/// Where the platform tells a container its region.
pub const REGION_ENV: &str = "RAILWAY_REPLICA_REGION";

/// The ` region=<name>` suffix an `sli` line ends with, or nothing when the
/// region is unknown. The control plane groups lines by region to tell a
/// silent cluster from a log pipeline that stopped delivering a region's lines.
pub fn region_token(region: Option<&str>) -> String {
    match region.map(str::trim) {
        Some(r) if !r.is_empty() && !r.contains(char::is_whitespace) => format!(" region={r}"),
        _ => String::new(),
    }
}

/// This process's region, read once by the caller.
pub fn region_from_env() -> Option<String> {
    std::env::var(REGION_ENV).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_token_is_a_trailing_field_or_nothing() {
        assert_eq!(region_token(Some("us-west2")), " region=us-west2");
        assert_eq!(
            region_token(Some(" europe-west4-drams3a ")),
            " region=europe-west4-drams3a"
        );
        assert_eq!(region_token(Some("")), "");
        assert_eq!(region_token(Some("two words")), "");
        assert_eq!(region_token(None), "");
    }
}
