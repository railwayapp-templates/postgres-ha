//! Post-bootstrap components
//!
//! This module provides functionality for the post-bootstrap script:
//! - Reading credentials from Patroni config
//! - SQL execution helpers

mod collation;
mod config;
mod extensions;
mod sql;

pub use collation::refresh_collation_versions;
pub use config::{read_credentials, Credentials, PATRONI_CONFIG};
pub use extensions::reconcile_pg_stat_statements;
pub use sql::{
    dollar_quote_tag, quote_ident, quote_literal, run_psql, run_psql_in_db, run_psql_script,
};
