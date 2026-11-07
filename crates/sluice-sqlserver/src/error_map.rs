//! tiberius errors → D-017's layered error model (T-080).
//!
//! The engine's own number, class and text are kept verbatim in the payload;
//! the kind is normalized from the error number. Connection-level failures
//! keep their text, shape-redacted (T-024) — the session's own password is
//! redacted on top by the driver's `Redactor`.

use sluice_core::error::{DriverError, EnginePayload, ErrorKind};
use sluice_core::redact_shapes;

/// SQL Server error numbers that, from one of our fixed catalog queries, mean
/// the catalog differs from what the query was written for (invalid object or
/// column, or a cross-database reference the server does not allow).
pub const DIVERGENCE: &[u32] = &[207, 208, 4121, 40515];

/// Permission-type refusals: rendered as a note in the navigator (D-016).
pub const PERMISSION: &[u32] = &[229, 230, 262, 297, 300, 916, 15247];

fn kind_for(code: u32) -> ErrorKind {
    match code {
        18456 | 18452 | 18486 | 18487 | 18488 => ErrorKind::AuthFailed,
        4060 => ErrorKind::PermissionDenied,
        c if PERMISSION.contains(&c) => ErrorKind::PermissionDenied,
        102 | 103 | 105 | 156 | 170 | 319 | 1038 => ErrorKind::SyntaxError,
        515 | 547 | 2601 | 2627 | 8152 | 2628 => ErrorKind::ConstraintViolation,
        207 | 208 | 2812 | 3701 | 911 => ErrorKind::NotFound,
        1205 => ErrorKind::Conflict,
        1222 => ErrorKind::Timeout,
        3617 | 3980 => ErrorKind::Cancelled,
        _ => ErrorKind::Unknown,
    }
}

pub fn map_error(err: &tiberius::error::Error) -> DriverError {
    use tiberius::error::Error;
    match err {
        Error::Server(token) => {
            let code = token.code();
            DriverError::new(kind_for(code), token.message().to_string()).with_payload(EnginePayload {
                code: Some(code.to_string()),
                message: Some(token.message().to_string()),
                severity: Some(token.class().to_string()),
                extra: vec![
                    ("state".into(), token.state().to_string()),
                    ("server".into(), token.server().to_string()),
                    ("procedure".into(), token.procedure().to_string()),
                    ("line".into(), token.line().to_string()),
                ],
            })
        }
        // T-072: SQL Server's auto-generated certificate is X.509 v1, which
        // rustls refuses as `UnsupportedCertVersion` — a baffling string for
        // the most common case there is, a self-signed dev server. tiberius
        // reports it as an `Io` error, not `Tls`, so match on the text.
        other if is_untrusted_certificate(&other.to_string()) => {
            let msg = redact_shapes(&other.to_string());
            DriverError::new(
                ErrorKind::ConnectionFailed,
                "this server's TLS certificate is self-signed or not trusted by this machine. For a \
                 development server, set this profile's TLS to \"Accept invalid certificates\"; for a \
                 production server, install a certificate from a trusted authority.",
            )
            .with_payload(EnginePayload { message: Some(msg), ..Default::default() })
        }
        Error::Tls(msg) => {
            DriverError::new(ErrorKind::ConnectionFailed, format!("TLS handshake failed: {}", redact_shapes(msg)))
                .with_payload(EnginePayload { message: Some(redact_shapes(msg)), ..Default::default() })
        }
        other => {
            let msg = redact_shapes(&other.to_string());
            DriverError::new(ErrorKind::ConnectionFailed, msg.clone())
                .with_payload(EnginePayload { message: Some(msg), ..Default::default() })
        }
    }
}

fn is_untrusted_certificate(msg: &str) -> bool {
    msg.contains("invalid peer certificate")
        || msg.contains("UnsupportedCertVersion")
        || msg.contains("UnknownIssuer")
}

/// Whether an error from a fixed catalog query means the catalog differs.
pub fn is_divergence(e: &DriverError) -> bool {
    code(e).is_some_and(|c| DIVERGENCE.contains(&c))
}

pub fn code(e: &DriverError) -> Option<u32> {
    e.payload.code.as_deref().and_then(|c| c.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_numbers_normalize_to_kinds() {
        assert_eq!(kind_for(18456), ErrorKind::AuthFailed);
        assert_eq!(kind_for(229), ErrorKind::PermissionDenied);
        assert_eq!(kind_for(102), ErrorKind::SyntaxError);
        assert_eq!(kind_for(2627), ErrorKind::ConstraintViolation);
        assert_eq!(kind_for(208), ErrorKind::NotFound);
        assert_eq!(kind_for(1205), ErrorKind::Conflict);
        assert_eq!(kind_for(99999), ErrorKind::Unknown);
    }

    #[test]
    fn the_self_signed_certificate_error_says_what_to_do() {
        let e = map_error(&tiberius::error::Error::Tls(
            "invalid peer certificate: Other(UnsupportedCertVersion)".into(),
        ));
        assert_eq!(e.kind, ErrorKind::ConnectionFailed);
        assert!(e.message.contains("Accept invalid certificates"), "{}", e.message);
        assert!(e.payload.message.unwrap().contains("UnsupportedCertVersion"));

        // What tiberius actually returns (measured live): an I/O error.
        let e = map_error(&tiberius::error::Error::Io {
            kind: std::io::ErrorKind::InvalidData,
            message: "invalid peer certificate: Other(UnsupportedCertVersion)".into(),
        });
        assert!(e.message.contains("Accept invalid certificates"), "{}", e.message);
    }
}
