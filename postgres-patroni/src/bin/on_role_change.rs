//! Patroni callback for role changes (failover detection)
//!
//! Called by Patroni with: $1=action $2=role $3=scope
//! Sends telemetry to Railway backboard for monitoring/alerting
//!
//! NOTE: Patroni intentionally strips PATRONI_* env vars from callback processes
//! for security reasons, so we read the node name from the config file instead.

use common::{Telemetry, TelemetryEvent};
use postgres_patroni::pgbackrest::derive_pgbackrest_repo_path;
use postgres_patroni::pgdata;
use std::env;
use std::fs;
use tracing::info;

/// Read the node name from Patroni's YAML config file.
/// Patroni strips PATRONI_* env vars from callback subprocesses,
/// so we must read from the generated config instead.
fn get_node_name() -> Option<String> {
    let config_path = "/etc/patroni/patroni.yml";
    let content = fs::read_to_string(config_path).ok()?;
    let yaml: serde_yaml::Value = serde_yaml::from_str(&content).ok()?;
    yaml.get("name")?.as_str().map(|s| s.to_string())
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let action = args.get(1);
    let role = args.get(2);
    let scope = args.get(3);

    // Only proceed for role changes
    if action.map(|s| s.as_str()) != Some("on_role_change") {
        std::process::exit(0);
    }

    let node_name = get_node_name();
    let telemetry = Telemetry::from_env("postgres-ha");

    let event = match (role.map(|s| s.as_str()), scope, node_name) {
        (Some("master" | "primary"), Some(scope), Some(node)) => {
            // Write .pgbackrest_repo_path immediately on master promotion so the
            // marker is present before the first archive_command fires. Without
            // this, a node that was previously a replica (and whose volume never
            // had the marker written) would probe as ARCHIVE_CATALOG_EMPTY until
            // the first WAL archive wrote it — or until the monitor's
            // pg_control_system() fallback kicked in.
            if env::var("WAL_ARCHIVE_BUCKET")
                .ok()
                .filter(|s| !s.is_empty())
                .is_some()
            {
                let data_dir = pgdata();
                let repo_path = derive_pgbackrest_repo_path(&data_dir);
                info!(repo_path = %repo_path, "pgbackrest: repo-path marker refreshed on master promotion");
            }

            TelemetryEvent::PostgresFailover {
                node,
                new_role: role.unwrap().to_string(),
                scope: scope.to_string(),
            }
        }
        (Some("replica" | "standby"), Some(scope), Some(node)) => {
            TelemetryEvent::PostgresRejoined {
                node,
                role: role.unwrap().to_string(),
                scope: scope.to_string(),
            }
        }
        _ => TelemetryEvent::ComponentError {
            component: "patroni".to_string(),
            error: format!(
                "Unexpected on_role_change state: role={:?}, scope={:?}, node={:?}, args={:?}",
                role,
                scope,
                env::var("PATRONI_NAME"),
                args
            ),
            context: "on_role_change".to_string(),
        },
    };

    // Send telemetry before exiting
    telemetry.send(event);

    // Always exit 0 to not block Patroni
    std::process::exit(0);
}
