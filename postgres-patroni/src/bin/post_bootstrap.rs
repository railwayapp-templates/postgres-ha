//! Patroni post-bootstrap script
//!
//! Runs ONCE after PostgreSQL initialization on the primary node.
//! IMPORTANT: Patroni runs this as a subprocess WITHOUT environment variables.
//! We MUST read credentials from /etc/patroni/patroni.yml

use anyhow::{Context, Result};
use common::{init_logging, Telemetry, TelemetryEvent};
use postgres_patroni::bootstrap::{
    dollar_quote_tag, quote_ident, quote_literal, read_credentials, run_psql, run_psql_in_db,
    run_psql_script, PATRONI_CONFIG,
};
use postgres_patroni::patroni::{
    control_plane_passwords_from_env, write_credential_pin, PinnedCredentials,
};
use postgres_patroni::{pgdata, volume_root};
use std::env;
use std::path::Path;
use std::time::Instant;
use tracing::{error, info, warn};

fn main() -> Result<()> {
    let _guard = init_logging("post-bootstrap");

    let start = Instant::now();
    let telemetry = Telemetry::from_env("postgres-ha");
    let node_name = env::var("PATRONI_NAME").unwrap_or_else(|_| "unknown".to_string());

    info!("Post-bootstrap starting...");

    telemetry.send(TelemetryEvent::BootstrapStarted {
        node: node_name.clone(),
        is_fresh: true,
    });

    if !Path::new(PATRONI_CONFIG).exists() {
        error!(path = PATRONI_CONFIG, "Patroni config not found");
        telemetry.send(TelemetryEvent::BootstrapFailed {
            node: node_name,
            error: "Patroni config not found".to_string(),
            phase: "read_config".to_string(),
        });
        std::process::exit(1);
    }

    let creds = match read_credentials() {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "Failed to read credentials");
            telemetry.send(TelemetryEvent::BootstrapFailed {
                node: node_name,
                error: e.to_string(),
                phase: "read_credentials".to_string(),
            });
            std::process::exit(1);
        }
    };

    if creds.repl_user.is_empty() || creds.repl_pass.is_empty() {
        error!("Missing replication credentials");
        std::process::exit(1);
    }

    if creds.superuser.is_empty() {
        error!("Missing superuser");
        std::process::exit(1);
    }

    info!(superuser = %creds.superuser, "Setting up users");

    // Every credential below is spliced as a properly quoted SQL literal
    // (quote_literal: ' doubled, wrapped in '…'), and the DO bodies use a
    // dollar-quote tag chosen to appear in none of the values. Raw splicing
    // used to mean a password containing a quote (or `$$`) terminated the
    // literal — or the DO body itself — early: bootstrap aborted, and the
    // failing statement, password included, landed in the server log via
    // log_min_error_statement. RAISE NOTICE takes the names as % arguments
    // for the same reason.
    let tag = dollar_quote_tag(&[
        &creds.superuser,
        &creds.superuser_pass,
        &creds.repl_user,
        &creds.repl_pass,
        &creds.app_user,
        &creds.app_pass,
    ]);
    let sql = format!(
        r#"
SET password_encryption = 'scram-sha-256';

DO {tag}
BEGIN
    EXECUTE format('ALTER ROLE %I WITH PASSWORD %L', {superuser}, {superuser_pass});
    RAISE NOTICE 'Set password for superuser: %', {superuser};
END
{tag};

DO {tag}
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = {repl_user}) THEN
        EXECUTE format('CREATE ROLE %I WITH REPLICATION LOGIN PASSWORD %L', {repl_user}, {repl_pass});
        RAISE NOTICE 'Created replication user: %', {repl_user};
    ELSE
        EXECUTE format('ALTER ROLE %I WITH REPLICATION LOGIN PASSWORD %L', {repl_user}, {repl_pass});
        RAISE NOTICE 'Updated replication user: %', {repl_user};
    END IF;
END
{tag};

DO {tag}
BEGIN
    IF {app_user} = {superuser} THEN
        RAISE NOTICE 'App user same as superuser, skipping';
    ELSIF {app_user} = '' OR {app_pass} = '' THEN
        RAISE NOTICE 'App user not configured, skipping';
    ELSIF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = {app_user}) THEN
        EXECUTE format('CREATE ROLE %I WITH LOGIN PASSWORD %L', {app_user}, {app_pass});
        RAISE NOTICE 'Created app user: %', {app_user};
    ELSE
        EXECUTE format('ALTER ROLE %I WITH PASSWORD %L', {app_user}, {app_pass});
        RAISE NOTICE 'Updated app user: %', {app_user};
    END IF;
END
{tag};

DO {tag}
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'postgres') THEN
        EXECUTE format('CREATE ROLE postgres WITH SUPERUSER LOGIN PASSWORD %L', {superuser_pass});
        RAISE NOTICE 'Created postgres superuser for compatibility';
    ELSE
        ALTER ROLE postgres WITH SUPERUSER;
        RAISE NOTICE 'Ensured postgres has superuser privileges';
    END IF;
END
{tag};
"#,
        tag = tag,
        superuser = quote_literal(&creds.superuser),
        superuser_pass = quote_literal(&creds.superuser_pass),
        repl_user = quote_literal(&creds.repl_user),
        repl_pass = quote_literal(&creds.repl_pass),
        app_user = quote_literal(&creds.app_user),
        app_pass = quote_literal(&creds.app_pass),
    );

    if let Err(e) = run_psql_script(&creds.superuser, &sql) {
        error!(error = %e, "Failed to create users");
        telemetry.send(TelemetryEvent::BootstrapFailed {
            node: node_name,
            error: e.to_string(),
            phase: "create_users".to_string(),
        });
        std::process::exit(1);
    }

    // Pin the passwords the roles were just created with, beside the data they
    // protect: later boots keep them when the variables drift (Patroni never
    // re-syncs role passwords), and clones inherit the pin with the data. A
    // failed write is not fatal — the runner adopts the variables as the pin on
    // the next boot, which at that point still equal these values.
    let (etcd_pass, restapi_pass) = control_plane_passwords_from_env();
    let pin_dir = if creds.data_dir.is_empty() {
        pgdata()
    } else {
        creds.data_dir.clone()
    };
    match write_credential_pin(
        &pin_dir,
        &PinnedCredentials {
            superuser_pass: creds.superuser_pass.clone(),
            repl_pass: creds.repl_pass.clone(),
            app_pass: creds.app_pass.clone(),
            // Read from the environment rather than patroni.yml: at bootstrap
            // the variables ARE what the cluster is being created with (etcd's
            // root user is created from the same secret), so this is the one
            // moment they can be trusted.
            etcd_pass,
            restapi_pass,
        },
    ) {
        Ok(()) => info!(data_dir = %pin_dir, "Pinned the bootstrap credentials"),
        Err(e) => warn!(error = %e, data_dir = %pin_dir, "Failed to write the credential pin"),
    }

    // Create app database if configured
    if !creds.app_db.is_empty() && creds.app_db != "postgres" {
        info!(database = %creds.app_db, "Checking app database");

        let db_exists = run_psql(
            &creds.superuser,
            &format!(
                "SELECT 1 FROM pg_database WHERE datname = {}",
                quote_literal(&creds.app_db)
            ),
        )?;

        if !db_exists.contains('1') {
            info!(database = %creds.app_db, "Creating app database");
            run_psql(
                &creds.superuser,
                &format!("CREATE DATABASE {}", quote_ident(&creds.app_db)),
            )?;
        }

        if !creds.app_user.is_empty() && creds.app_user != creds.superuser {
            let grant_tag = dollar_quote_tag(&[&creds.app_db, &creds.app_user]);
            let grant_sql = format!(
                r#"
DO {tag}
BEGIN
    EXECUTE format('GRANT ALL PRIVILEGES ON DATABASE %I TO %I', {db}, {user});
END
{tag};
"#,
                tag = grant_tag,
                db = quote_literal(&creds.app_db),
                user = quote_literal(&creds.app_user),
            );
            run_psql_script(&creds.superuser, &grant_sql)?;
        }
    }

    // Enable pg_stat_statements extension in postgres database and app database
    if let Err(e) = run_psql(
        &creds.superuser,
        "CREATE EXTENSION IF NOT EXISTS pg_stat_statements",
    ) {
        warn!(error = %e, "Failed to create pg_stat_statements in postgres database");
    }

    if !creds.app_db.is_empty() && creds.app_db != "postgres" {
        if let Err(e) = run_psql_in_db(
            &creds.superuser,
            &creds.app_db,
            "CREATE EXTENSION IF NOT EXISTS pg_stat_statements",
        ) {
            warn!(error = %e, database = %creds.app_db, "Failed to create pg_stat_statements in app database");
        }
    }

    let mut users_created = vec![creds.superuser.clone(), creds.repl_user.clone()];
    if !creds.app_user.is_empty() && creds.app_user != creds.superuser {
        users_created.push(creds.app_user.clone());
    }

    info!(
        superuser = %creds.superuser,
        replication = %creds.repl_user,
        app_user = %creds.app_user,
        database = %creds.app_db,
        "Users created"
    );

    // Mark bootstrap complete
    let marker_path = format!("{}/.patroni_bootstrap_complete", volume_root());
    std::fs::write(&marker_path, "").context("Failed to write bootstrap marker")?;

    let duration_ms = start.elapsed().as_millis() as u64;
    telemetry.send(TelemetryEvent::BootstrapCompleted {
        node: node_name,
        duration_ms,
        users_created,
    });

    info!(duration_ms, "Post-bootstrap completed");

    Ok(())
}
