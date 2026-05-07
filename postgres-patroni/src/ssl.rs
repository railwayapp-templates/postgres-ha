//! SSL certificate utilities
//!
//! Functions for validating SSL certificates and checking expiration.
//! Uses the openssl crate for direct certificate parsing without subprocess calls.

use anyhow::{Context, Result};
use openssl::pkey::PKey;
use openssl::x509::{X509StoreContext, X509};
use std::fs;
use std::path::Path;

/// Validate SSL certificate setup in the given directory.
///
/// Checks that:
/// 1. server.crt, server.key, and root.crt all exist and are parseable
/// 2. The server certificate's public key matches the private key
/// 3. The server certificate is signed by the root CA
pub fn is_valid_x509v3_cert(cert_path: &str) -> Result<bool> {
    // Derive ssl_dir from cert_path (expects path like "{ssl_dir}/server.crt")
    let ssl_dir = Path::new(cert_path)
        .parent()
        .context("Invalid cert path: no parent directory")?;

    let server_crt_path = ssl_dir.join("server.crt");
    let server_key_path = ssl_dir.join("server.key");
    let root_crt_path = ssl_dir.join("root.crt");

    // Check all required files exist
    if !server_crt_path.exists() || !server_key_path.exists() || !root_crt_path.exists() {
        return Ok(false);
    }

    // Parse server certificate
    let server_crt_pem = fs::read(&server_crt_path).context("Failed to read server certificate")?;
    let server_cert =
        X509::from_pem(&server_crt_pem).context("Failed to parse server certificate as PEM")?;

    // Parse server private key
    let server_key_pem = fs::read(&server_key_path).context("Failed to read server private key")?;
    let server_key = PKey::private_key_from_pem(&server_key_pem)
        .context("Failed to parse server private key")?;

    // Parse root CA certificate
    let root_crt_pem = fs::read(&root_crt_path).context("Failed to read root CA certificate")?;
    let root_cert =
        X509::from_pem(&root_crt_pem).context("Failed to parse root CA certificate as PEM")?;

    // Verify cert/key pair match by comparing public keys
    let cert_pubkey = server_cert
        .public_key()
        .context("Failed to extract public key from certificate")?;
    if !cert_pubkey.public_eq(&server_key) {
        return Ok(false);
    }

    // Verify server cert is signed by root CA
    let mut store_builder =
        openssl::x509::store::X509StoreBuilder::new().context("Failed to create X509 store")?;
    store_builder
        .add_cert(root_cert)
        .context("Failed to add root CA to store")?;
    let store = store_builder.build();

    let mut store_ctx = X509StoreContext::new().context("Failed to create X509 store context")?;

    let chain = openssl::stack::Stack::new().context("Failed to create certificate chain")?;

    let is_valid = store_ctx
        .init(&store, &server_cert, &chain, |ctx| ctx.verify_cert())
        .context("Failed to verify certificate chain")?;

    Ok(is_valid)
}

/// Check if a certificate will expire within the given seconds
pub fn cert_expires_within(cert_path: &str, seconds: u64) -> Result<bool> {
    if !Path::new(cert_path).exists() {
        return Ok(true); // Treat missing cert as "needs renewal"
    }

    let pem_data = fs::read(cert_path).context("Failed to read certificate file")?;

    let cert = X509::from_pem(&pem_data).context("Failed to parse certificate as PEM")?;

    let not_after = cert.not_after();

    // Asn1TimeRef::diff(other) computes: other - self
    // We want: not_after - now = time remaining until expiry
    let now = openssl::asn1::Asn1Time::days_from_now(0).context("Failed to get current time")?;
    let diff = now
        .diff(&not_after)
        .context("Failed to compute time difference")?;

    // Convert diff to total seconds (time remaining until expiry)
    let total_seconds = (diff.days as i64 * 86400) + diff.secs as i64;

    // If total_seconds is negative, cert is already expired
    // If total_seconds < threshold, cert expires soon
    Ok(total_seconds < seconds as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::bn::BigNum;
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::x509::{X509Builder, X509NameBuilder};
    use std::io::Write;
    use tempfile::NamedTempFile;

    /// Create a self-signed certificate that expires in `days` days
    fn create_test_cert(days: u32) -> NamedTempFile {
        let rsa = Rsa::generate(2048).unwrap();
        let pkey = PKey::from_rsa(rsa).unwrap();

        let mut name_builder = X509NameBuilder::new().unwrap();
        name_builder.append_entry_by_text("CN", "test").unwrap();
        let name = name_builder.build();

        let mut builder = X509Builder::new().unwrap();
        builder.set_version(2).unwrap();
        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&pkey).unwrap();

        let serial = BigNum::from_u32(1).unwrap();
        builder
            .set_serial_number(&serial.to_asn1_integer().unwrap())
            .unwrap();

        let not_before = Asn1Time::days_from_now(0).unwrap();
        let not_after = Asn1Time::days_from_now(days).unwrap();
        builder.set_not_before(&not_before).unwrap();
        builder.set_not_after(&not_after).unwrap();

        builder.sign(&pkey, MessageDigest::sha256()).unwrap();
        let cert = builder.build();

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&cert.to_pem().unwrap()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn test_missing_cert_is_invalid() {
        assert!(!is_valid_x509v3_cert("/nonexistent/cert.pem").unwrap());
    }

    #[test]
    fn test_missing_cert_expires_soon() {
        assert!(cert_expires_within("/nonexistent/cert.pem", 86400).unwrap());
    }

    #[test]
    fn test_cert_expires_within_one_year_not_expiring_soon() {
        // Cert valid for 365 days should NOT be flagged as expiring within 30 days
        let cert_file = create_test_cert(365);
        let thirty_days_secs = 30 * 24 * 60 * 60;

        let result = cert_expires_within(cert_file.path().to_str().unwrap(), thirty_days_secs);
        assert!(
            !result.unwrap(),
            "Cert valid for 365 days should NOT be expiring within 30 days"
        );
    }

    #[test]
    fn test_cert_expires_within_10_days_is_expiring_soon() {
        // Cert valid for only 10 days SHOULD be flagged as expiring within 30 days
        let cert_file = create_test_cert(10);
        let thirty_days_secs = 30 * 24 * 60 * 60;

        let result = cert_expires_within(cert_file.path().to_str().unwrap(), thirty_days_secs);
        assert!(
            result.unwrap(),
            "Cert valid for 10 days SHOULD be expiring within 30 days"
        );
    }

    #[test]
    fn test_cert_expires_within_boundary() {
        // Cert valid for exactly 31 days should NOT be flagged as expiring within 30 days
        let cert_file = create_test_cert(31);
        let thirty_days_secs = 30 * 24 * 60 * 60;

        let result = cert_expires_within(cert_file.path().to_str().unwrap(), thirty_days_secs);
        assert!(
            !result.unwrap(),
            "Cert valid for 31 days should NOT be expiring within 30 days"
        );
    }

    #[test]
    fn test_cert_expires_within_820_days_not_expiring() {
        // Regression test: 820-day cert (our default) must NOT trigger expiry renewal
        let cert_file = create_test_cert(820);
        let thirty_days_secs = 30 * 24 * 60 * 60;

        let result = cert_expires_within(cert_file.path().to_str().unwrap(), thirty_days_secs);
        assert!(
            !result.unwrap(),
            "Cert valid for 820 days (default SSL_CERT_DAYS) should NOT be expiring within 30 days"
        );
    }
}
