//! TLS configuration (T-021 / D-007).
//!
//! D-007: *TLS certificate verification on by default, with an explicit
//! per-profile opt-out that shows a visible warning.* The opt-out is a named,
//! serializable mode rather than a boolean flag buried in a config struct,
//! precisely so the UI can render the warning off the same value the connection
//! used — not off a separate setting that could drift from it.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};

/// T-078: the mode itself is engine-neutral and lives in `sluice-core`.
pub use sluice_core::TlsMode;

/// Build a verifying rustls config from the platform trust store.
pub fn client_config(mode: TlsMode) -> Result<ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    match rustls_native_certs::load_native_certs() {
        certs if !certs.certs.is_empty() => {
            for c in certs.certs {
                let _ = roots.add(c);
            }
        }
        _ => {
            // Fall back to the bundled Mozilla roots rather than silently
            // trusting nothing (which would fail every connection) or silently
            // trusting everything (which would defeat the default).
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
    }

    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    Ok(match mode {
        TlsMode::Verify | TlsMode::Disabled => config,
        TlsMode::AcceptInvalidCerts => {
            let mut c = ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerification))
                .with_no_client_auth();
            c.enable_sni = true;
            c
        }
    })
}

/// Only reachable through [`TlsMode::AcceptInvalidCerts`], which is itself only
/// reachable through an explicit per-profile opt-out that renders a warning.
#[derive(Debug)]
struct NoVerification;

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_is_the_default() {
        assert_eq!(TlsMode::default(), TlsMode::Verify);
        assert!(!TlsMode::Verify.warns());
        assert!(TlsMode::Verify.warning().is_none());
    }

    /// D-007 ties the opt-out to a *visible warning*. Deriving the warning from
    /// the mode itself means a profile cannot be in the unsafe state while the
    /// UI shows nothing.
    #[test]
    fn every_opt_out_carries_a_warning() {
        for mode in [TlsMode::AcceptInvalidCerts, TlsMode::Disabled] {
            assert!(mode.warns(), "{mode:?}");
            assert!(mode.warning().is_some(), "{mode:?}");
        }
    }

    #[test]
    fn modes_round_trip_as_snake_case() {
        let j = serde_json::to_string(&TlsMode::AcceptInvalidCerts).unwrap();
        assert_eq!(j, "\"accept_invalid_certs\"");
        assert_eq!(
            serde_json::from_str::<TlsMode>("\"verify\"").unwrap(),
            TlsMode::Verify
        );
    }
}
