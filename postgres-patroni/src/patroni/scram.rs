//! Check a candidate password against the verifier PostgreSQL stores for a
//! role in `pg_authid.rolpassword`.
//!
//! The rotation route must not move a member to a password its roles do not
//! carry yet: it would render that password into patroni.yml, Patroni would
//! copy it into its pgpass, and replication and rewind on this node would
//! break until the roles caught up. A login test cannot tell, because the
//! local socket and loopback are `trust` in pg_hba. The verifier can: it is
//! either `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`
//! (RFC 7677) or `md5<hex>` (md5 of password followed by username), and both
//! are recomputable from the candidate with the primitives the `openssl`
//! crate already provides. PostgreSQL runs SASLprep over the password before
//! hashing; the passwords Railway generates are ASCII, for which SASLprep is
//! the identity, so the candidate is hashed as given.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use openssl::hash::{hash, MessageDigest};
use openssl::pkcs5::pbkdf2_hmac;
use openssl::pkey::PKey;
use openssl::sign::Signer;

/// What the verifier says about the candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifierCheck {
    /// The role carries this password.
    Matches,
    /// The role carries a different password.
    Differs,
    /// A verifier this module cannot recompute (unknown scheme or malformed).
    Unsupported(String),
}

/// Compare `password` with the `pg_authid.rolpassword` value `verifier` of
/// role `username`.
pub fn verify(verifier: &str, username: &str, password: &str) -> VerifierCheck {
    if let Some(rest) = verifier.strip_prefix("SCRAM-SHA-256$") {
        return verify_scram_sha256(rest, password);
    }
    if let Some(hex) = verifier.strip_prefix("md5") {
        return verify_md5(hex, username, password);
    }
    VerifierCheck::Unsupported(format!(
        "verifier scheme {:?} is not SCRAM-SHA-256 or md5",
        verifier.split('$').next().unwrap_or("")
    ))
}

fn verify_scram_sha256(rest: &str, password: &str) -> VerifierCheck {
    let Some((params, keys)) = rest.split_once('$') else {
        return VerifierCheck::Unsupported("SCRAM verifier without a key section".into());
    };
    let Some((iterations, salt_b64)) = params.split_once(':') else {
        return VerifierCheck::Unsupported("SCRAM verifier without a salt".into());
    };
    let Some((stored_key_b64, server_key_b64)) = keys.split_once(':') else {
        return VerifierCheck::Unsupported("SCRAM verifier without a ServerKey".into());
    };
    let Ok(iterations) = iterations.parse::<usize>() else {
        return VerifierCheck::Unsupported("SCRAM iteration count is not a number".into());
    };
    if iterations == 0 {
        return VerifierCheck::Unsupported("SCRAM iteration count is zero".into());
    }
    let (Ok(salt), Ok(stored_key), Ok(server_key)) = (
        BASE64.decode(salt_b64),
        BASE64.decode(stored_key_b64),
        BASE64.decode(server_key_b64),
    ) else {
        return VerifierCheck::Unsupported("SCRAM verifier is not valid base64".into());
    };
    let mut salted = [0u8; 32];
    if pbkdf2_hmac(
        password.as_bytes(),
        &salt,
        iterations,
        MessageDigest::sha256(),
        &mut salted,
    )
    .is_err()
    {
        return VerifierCheck::Unsupported("PBKDF2 failed".into());
    }
    let (Ok(client_key), Ok(candidate_server_key)) = (
        hmac_sha256(&salted, b"Client Key"),
        hmac_sha256(&salted, b"Server Key"),
    ) else {
        return VerifierCheck::Unsupported("HMAC failed".into());
    };
    let Ok(candidate_stored_key) = hash(MessageDigest::sha256(), &client_key) else {
        return VerifierCheck::Unsupported("SHA-256 failed".into());
    };
    if constant_time_eq(&candidate_stored_key, &stored_key)
        && constant_time_eq(&candidate_server_key, &server_key)
    {
        VerifierCheck::Matches
    } else {
        VerifierCheck::Differs
    }
}

fn verify_md5(hex: &str, username: &str, password: &str) -> VerifierCheck {
    let Ok(digest) = hash(
        MessageDigest::md5(),
        format!("{password}{username}").as_bytes(),
    ) else {
        return VerifierCheck::Unsupported("md5 failed".into());
    };
    let candidate: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    if constant_time_eq(candidate.as_bytes(), hex.to_ascii_lowercase().as_bytes()) {
        VerifierCheck::Matches
    } else {
        VerifierCheck::Differs
    }
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>, openssl::error::ErrorStack> {
    let key = PKey::hmac(key)?;
    let mut signer = Signer::new(MessageDigest::sha256(), &key)?;
    signer.update(data)?;
    signer.sign_to_vec()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && openssl::memcmp::eq(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Computed independently with Python's hashlib (pbkdf2_hmac, hmac, sha256)
    // for password "pencil", the RFC 7677 salt and 4096 iterations.
    const PENCIL: &str =
        "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";
    // md5("pencil" + "postgres"), also from Python.
    const PENCIL_MD5: &str = "md54c21b325d75177d5e9b9789a50ab0c9c";

    #[test]
    fn scram_verifier_matches_its_password() {
        assert_eq!(verify(PENCIL, "postgres", "pencil"), VerifierCheck::Matches);
    }

    #[test]
    fn scram_verifier_refuses_another_password() {
        assert_eq!(
            verify(PENCIL, "postgres", "pencils"),
            VerifierCheck::Differs
        );
        assert_eq!(verify(PENCIL, "postgres", ""), VerifierCheck::Differs);
    }

    #[test]
    fn scram_verifier_ignores_the_username() {
        // SCRAM salts per role but does not mix the name into the keys.
        assert_eq!(verify(PENCIL, "someone", "pencil"), VerifierCheck::Matches);
    }

    #[test]
    fn md5_verifier_is_password_then_username() {
        assert_eq!(
            verify(PENCIL_MD5, "postgres", "pencil"),
            VerifierCheck::Matches
        );
        assert_eq!(
            verify(PENCIL_MD5, "replicator", "pencil"),
            VerifierCheck::Differs
        );
        assert_eq!(
            verify(PENCIL_MD5, "postgres", "pen"),
            VerifierCheck::Differs
        );
    }

    #[test]
    fn malformed_verifiers_are_unsupported_never_matches() {
        for bad in [
            "",
            "plain",
            "SCRAM-SHA-256$",
            "SCRAM-SHA-256$4096:notbase64!$a:b",
            "SCRAM-SHA-256$x:W22ZaJ0SNY7soEsUEjb6gQ==$a:b",
            "SCRAM-SHA-256$0:W22ZaJ0SNY7soEsUEjb6gQ==$a:b",
            "SCRAM-SHA-1$4096:W22ZaJ0SNY7soEsUEjb6gQ==$a:b",
        ] {
            assert!(
                matches!(
                    verify(bad, "postgres", "pencil"),
                    VerifierCheck::Unsupported(_)
                ),
                "{bad:?}"
            );
        }
    }
}
