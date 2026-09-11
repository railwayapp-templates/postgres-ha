//! Stop a member whose REST API password would differ from its peers'.
//!
//! On the template `PATRONI_RESTAPI_PASSWORD` and `PATRONI_SUPERUSER_PASSWORD`
//! are both references to `POSTGRES_PASSWORD`. When that variable is edited
//! after the cluster was created, a member that restarts keeps its roles'
//! passwords (the credential pin) but would ENFORCE the edited value on its
//! own REST API and PRESENT it to its peers, which still hold the original.
//! Every REST call between them is then refused both ways — the peers'
//! `POST /failsafe` pings included, so the leader demotes on its next DCS
//! hiccup — while the runner's own calls to the local API keep working and
//! nothing names the cause.
//!
//! [`etcd_preflight`](super::etcd_preflight) stops such a member when etcd
//! refuses the same edited password. This module covers the cluster where etcd
//! does not check it (authentication not enabled, or a dedicated
//! `PATRONI_ETCD3_PASSWORD`): the credential pin has already recorded that the
//! superuser variable moved, and the REST password is that same moved value.
//! No control-plane credential is pinned or looked up; the member stops with
//! the guidance the etcd pre-flight gives, and restoring the variable brings
//! it back.

use super::etcd_preflight::{FIXED_AT_CREATION, RECOVERY};

/// Stable first words of the message, for log matching.
pub const DIVERGENCE_PREFIX: &str =
    "REST API credential differs from the one this cluster's members hold";

/// Whether starting Patroni would enforce a REST password the peers do not
/// hold. `drifted` is the credential pin's list of variables that differ from
/// the pinned passwords (see `credential_pin::credential_drift`);
/// `restapi_password` is the value this member would enforce, `None` when it
/// does not enforce; `superuser_password_env` is the superuser password as the
/// variables say it (pre-pin).
pub fn rest_credential_diverges(
    drifted: &[&str],
    restapi_password: Option<&str>,
    superuser_password_env: &str,
) -> bool {
    drifted.contains(&"PATRONI_SUPERUSER_PASSWORD")
        && restapi_password.is_some_and(|rest| rest == superuser_password_env)
}

/// The message a member logs when it stops for a diverged REST credential.
pub fn divergence_message(drifted_variables: &[&str]) -> String {
    let mut lines = vec![format!(
        "{DIVERGENCE_PREFIX}: PATRONI_RESTAPI_PASSWORD follows the edited PATRONI_SUPERUSER_PASSWORD, so this member would refuse its peers' requests (failsafe, switchover, failover) and they would refuse its own."
    )];
    lines.push(FIXED_AT_CREATION.to_string());
    if !drifted_variables.is_empty() {
        lines.push(format!(
            "Variables that differ from the credentials this cluster runs with: {}.",
            drifted_variables.join(", ")
        ));
    }
    lines.push(RECOVERY.to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRIFTED: &[&str] = &["PATRONI_SUPERUSER_PASSWORD", "POSTGRES_PASSWORD"];

    #[test]
    fn diverges_when_the_rest_password_is_the_moved_superuser_password() {
        assert!(rest_credential_diverges(DRIFTED, Some("edited"), "edited"));
    }

    #[test]
    fn does_not_diverge_without_enforcement() {
        // Unenforced members present the credential and accept any: nothing
        // to refuse on either side.
        assert!(!rest_credential_diverges(DRIFTED, None, "edited"));
    }

    #[test]
    fn does_not_diverge_when_the_rest_password_is_its_own() {
        // A dedicated REST password did not move with POSTGRES_PASSWORD.
        assert!(!rest_credential_diverges(
            DRIFTED,
            Some("rest-own"),
            "edited"
        ));
    }

    #[test]
    fn does_not_diverge_when_the_superuser_variable_did_not_move() {
        assert!(!rest_credential_diverges(
            &["POSTGRES_PASSWORD"],
            Some("same"),
            "same"
        ));
        assert!(!rest_credential_diverges(&[], Some("same"), "same"));
    }

    #[test]
    fn the_message_names_the_variable_the_drift_and_the_fix() {
        let msg = divergence_message(DRIFTED);
        assert!(msg.starts_with(DIVERGENCE_PREFIX));
        assert!(
            msg.contains("PATRONI_RESTAPI_PASSWORD follows the edited PATRONI_SUPERUSER_PASSWORD")
        );
        assert!(msg.contains("Variables that differ from the credentials this cluster runs with: PATRONI_SUPERUSER_PASSWORD, POSTGRES_PASSWORD."));
        assert!(msg.contains("editing a password variable afterwards does not change them"));
        assert!(msg.contains(
            "restore the previous value of the edited variable and redeploy this member"
        ));
    }
}
