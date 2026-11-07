//! `mongodb` errors → D-017's layered error model (T-087).
//!
//! The server's own `codeName`, numeric code and text are kept verbatim in the
//! payload (`EnginePayload.code` is the `codeName`, as D-017 names it for
//! Mongo); the kind is normalized from the numeric code. Client-side failures
//! (server selection, I/O, TLS) keep their text, shape-redacted (T-024) — the
//! session's own password is redacted on top by the driver's `Redactor`.

use mongodb::error::{Error, ErrorKind as MongoKind, WriteFailure};
use sluice_core::error::{DriverError, EnginePayload, ErrorKind};
use sluice_core::redact_shapes;

/// `Unauthorized` and friends: rendered as a note in the navigator (D-016).
pub const PERMISSION: &[i32] = &[13];

pub fn kind_for(code: i32) -> ErrorKind {
    match code {
        18 => ErrorKind::AuthFailed,                    // AuthenticationFailed
        c if PERMISSION.contains(&c) => ErrorKind::PermissionDenied,
        9 | 2 | 14 => ErrorKind::SyntaxError,           // FailedToParse, BadValue, TypeMismatch
        11000 | 11001 | 121 => ErrorKind::ConstraintViolation, // DuplicateKey, DocumentValidationFailure
        26 | 59 => ErrorKind::NotFound,                 // NamespaceNotFound, CommandNotFound
        112 | 251 => ErrorKind::Conflict,               // WriteConflict, NoSuchTransaction
        50 => ErrorKind::Timeout,                       // MaxTimeMSExpired
        11601 => ErrorKind::Cancelled,                  // Interrupted
        _ => ErrorKind::Unknown,
    }
}

fn server_error(code: i32, code_name: &str, message: &str) -> DriverError {
    DriverError::new(kind_for(code), message.to_string()).with_payload(EnginePayload {
        code: Some(code_name.to_string()),
        message: Some(message.to_string()),
        severity: None,
        extra: vec![("code".into(), code.to_string())],
    })
}

fn client_error(kind: ErrorKind, raw: &str) -> DriverError {
    let msg = redact_shapes(raw);
    DriverError::new(kind, msg.clone()).with_payload(EnginePayload { message: Some(msg), ..Default::default() })
}

pub fn map_error(err: &Error) -> DriverError {
    match err.kind.as_ref() {
        MongoKind::Command(c) => server_error(c.code, &c.code_name, &c.message),
        MongoKind::Write(WriteFailure::WriteError(w)) => {
            server_error(w.code, w.code_name.as_deref().unwrap_or(""), &w.message)
        }
        MongoKind::Authentication { message, .. } => {
            let msg = redact_shapes(message);
            DriverError::new(ErrorKind::AuthFailed, format!("authentication failed: {msg}"))
                .with_payload(EnginePayload { message: Some(msg), ..Default::default() })
        }
        // Server selection wraps whatever stopped every candidate server from
        // being reached, TLS failures included — so the certificate advice is
        // matched on the text, as it is for SQL Server (T-072).
        _ if is_untrusted_certificate(&err.to_string()) => {
            let msg = redact_shapes(&err.to_string());
            DriverError::new(
                ErrorKind::ConnectionFailed,
                "this server's TLS certificate is self-signed or not trusted by this machine. For a \
                 development server, set this profile's TLS to \"Accept invalid certificates\"; for a \
                 production server, install a certificate from a trusted authority.",
            )
            .with_payload(EnginePayload { message: Some(msg), ..Default::default() })
        }
        MongoKind::ServerSelection { .. }
        | MongoKind::Io(_)
        | MongoKind::DnsResolve { .. }
        | MongoKind::InvalidTlsConfig { .. }
        | MongoKind::ConnectionPoolCleared { .. } => client_error(ErrorKind::ConnectionFailed, &err.to_string()),
        MongoKind::SessionsNotSupported | MongoKind::IncompatibleServer { .. } => {
            client_error(ErrorKind::Unknown, &err.to_string())
        }
        MongoKind::InvalidResponse { .. } | MongoKind::BsonDeserialization(_) => {
            client_error(ErrorKind::DriverBug, &err.to_string())
        }
        _ => client_error(ErrorKind::Unknown, &err.to_string()),
    }
}

fn is_untrusted_certificate(msg: &str) -> bool {
    msg.contains("invalid peer certificate") || msg.contains("UnknownIssuer") || msg.contains("UnsupportedCertVersion")
}

/// Whether a refusal is a permission one, so the tree shows a note (D-016).
pub fn is_permission(e: &DriverError) -> bool {
    e.kind == ErrorKind::PermissionDenied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_codes_normalize_to_kinds() {
        assert_eq!(kind_for(18), ErrorKind::AuthFailed);
        assert_eq!(kind_for(13), ErrorKind::PermissionDenied);
        assert_eq!(kind_for(11000), ErrorKind::ConstraintViolation);
        assert_eq!(kind_for(26), ErrorKind::NotFound);
        assert_eq!(kind_for(112), ErrorKind::Conflict);
        assert_eq!(kind_for(50), ErrorKind::Timeout);
        assert_eq!(kind_for(424242), ErrorKind::Unknown);
    }

    #[test]
    fn a_client_error_keeps_its_text_shape_redacted() {
        let e = map_error(&Error::custom("x"));
        assert_eq!(e.kind, ErrorKind::Unknown);
        let e = client_error(ErrorKind::ConnectionFailed, "could not reach mongodb://u:hunter2@h:27017");
        assert!(!e.message.contains("hunter2"), "{}", e.message);
    }
}
