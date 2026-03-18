//! Patroni post-bootstrap script
//!
//! Runs ONCE after PostgreSQL initialization on the primary node.
//! IMPORTANT: Patroni runs this as a subprocess WITHOUT environment variables.
//! We MUST read credentials from /etc/patroni/patroni.yml

use anyhow::{Context, Result};
use common::{init_logging, Telemetry, TelemetryEvent};
use postgres_patroni::bootstrap::{read_credentials, run_psql, run_psql_in_db, run_psql_script, PATRONI_CONFIG};
use postgres_patroni::volume_root;
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

    let sql = format!(
        r#"
SET password_encryption = 'scram-sha-256';

DO $$
BEGIN
    EXECUTE format('ALTER ROLE %I WITH PASSWORD %L', '{superuser}', '{superuser_pass}');
    RAISE NOTICE 'Set password for superuser: {superuser}';
END
$$;

DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{repl_user}') THEN
        EXECUTE format('CREATE ROLE %I WITH REPLICATION LOGIN PASSWORD %L', '{repl_user}', '{repl_pass}');
        RAISE NOTICE 'Created replication user: {repl_user}';
    ELSE
        EXECUTE format('ALTER ROLE %I WITH REPLICATION LOGIN PASSWORD %L', '{repl_user}', '{repl_pass}');
        RAISE NOTICE 'Updated replication user: {repl_user}';
    END IF;
END
$$;

DO $$
BEGIN
    IF '{app_user}' = '{superuser}' THEN
        RAISE NOTICE 'App user same as superuser, skipping';
    ELSIF '{app_user}' = '' OR '{app_pass}' = '' THEN
        RAISE NOTICE 'App user not configured, skipping';
    ELSIF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = '{app_user}') THEN
        EXECUTE format('CREATE ROLE %I WITH LOGIN PASSWORD %L', '{app_user}', '{app_pass}');
        RAISE NOTICE 'Created app user: {app_user}';
    ELSE
        EXECUTE format('ALTER ROLE %I WITH PASSWORD %L', '{app_user}', '{app_pass}');
        RAISE NOTICE 'Updated app user: {app_user}';
    END IF;
END
$$;

DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'postgres') THEN
        EXECUTE format('CREATE ROLE postgres WITH SUPERUSER LOGIN PASSWORD %L', '{superuser_pass}');
        RAISE NOTICE 'Created postgres superuser for compatibility';
    ELSE
        ALTER ROLE postgres WITH SUPERUSER;
        RAISE NOTICE 'Ensured postgres has superuser privileges';
    END IF;
END
$$;
"#,
        superuser = creds.superuser,
        superuser_pass = creds.superuser_pass,
        repl_user = creds.repl_user,
        repl_pass = creds.repl_pass,
        app_user = creds.app_user,
        app_pass = creds.app_pass,
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

    // Create app database if configured
    if !creds.app_db.is_empty() && creds.app_db != "postgres" {
        info!(database = %creds.app_db, "Checking app database");

        let db_exists = run_psql(
            &creds.superuser,
            &format!(
                "SELECT 1 FROM pg_database WHERE datname = '{}'",
                creds.app_db
            ),
        )?;

        if !db_exists.contains('1') {
            info!(database = %creds.app_db, "Creating app database");
            run_psql(
                &creds.superuser,
                &format!("CREATE DATABASE \"{}\"", creds.app_db),
            )?;
        }

        if !creds.app_user.is_empty() && creds.app_user != creds.superuser {
            let grant_sql = format!(
                r#"
DO $$
BEGIN
    EXECUTE format('GRANT ALL PRIVILEGES ON DATABASE %I TO %I', '{db}', '{user}');
END
$$;
"#,
                db = creds.app_db,
                user = creds.app_user,
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
