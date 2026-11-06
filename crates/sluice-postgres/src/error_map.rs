//! Postgres → normalized error mapping (T-066 / D-017).
//!
//! This is the "driver boundary" D-017 names: the place where an engine-shaped
//! error becomes the three-layer model, including the position conversion.

use sluice_core::error::{position_from_1based_chars, DriverError, EnginePayload, ErrorHints, ErrorKind};
use sluice_core::redact_shapes;
use tokio_postgres::error::{DbError, ErrorPosition, SqlState};

/// Map a Postgres error, using the submitted statement text to resolve the
/// position hint into indices the webview can use.
pub fn map_error(err: &tokio_postgres::Error, statement: Option<&str>) -> DriverError {
    let Some(db) = err.as_db_error() else {
        // No DbError means the failure happened below SQL — socket, TLS,
        // startup. D-017 folds TLS into connection failure with detail kept.
        let msg = err.to_string();
        let kind = if msg.contains("authentication") || msg.contains("password") {
            ErrorKind::AuthFailed
        } else {
            ErrorKind::ConnectionFailed
        };
        // T-024: the whole `source()` chain is recorded, so the redaction pass
        // (and the user) can see every layer — a TLS or DNS error often names
        // the target only in an inner cause.
        let mut extra = Vec::new();
        let mut cause = std::error::Error::source(err);
        let mut n = 1;
        while let Some(c) = cause {
            extra.push((format!("cause.{n}"), redact_shapes(&c.to_string())));
            cause = c.source();
            n += 1;
        }
        // A TLS-required connection to a server without TLS. Since TLS modes
        // stopped falling back to plaintext, this is what a local `.env` URL
        // without `sslmode` meets first — so say what happened and the fix.
        let msg_display = if extra.iter().any(|(_, v)| v.contains("server does not support TLS")) {
            "the server does not support TLS, and this connection requires it. For a local \
             development server, add `sslmode=disable` to the connection URL."
                .to_string()
        } else {
            redact_shapes(&msg)
        };
        // Shape redaction here; the driver's `Redactor` adds the session's
        // known password, in every encoding, on top (T-024 / R-023).
        return DriverError::new(kind, msg_display).with_payload(EnginePayload {
            message: Some(redact_shapes(&msg)),
            extra,
            ..Default::default()
        });
    };

    let mut kind = kind_for(db.code());
    // T-051: 57014 is both a user cancel and a statement timeout; only the
    // message tells them apart, and the UI words them differently.
    if kind == ErrorKind::Cancelled && db.message().contains("statement timeout") {
        kind = ErrorKind::Timeout;
    }

    let hints = ErrorHints {
        position: statement.and_then(|sql| position_hint(db, sql)),
        constraint: db.constraint().map(str::to_string),
        column: db.column().map(str::to_string),
        table: db.table().map(str::to_string),
        schema: db.schema().map(str::to_string),
        retryable: Some(is_retryable(db.code())),
        credential_reentry: None,
    };

    let mut extra: Vec<(String, String)> = Vec::new();
    if let Some(d) = db.detail() {
        extra.push(("detail".into(), redact_shapes(d)));
    }
    if let Some(h) = db.hint() {
        extra.push(("hint".into(), redact_shapes(h)));
    }
    if let Some(w) = db.where_() {
        extra.push(("where".into(), redact_shapes(w)));
    }
    if let Some(r) = db.routine() {
        extra.push(("routine".into(), r.to_string()));
    }

    let payload = EnginePayload {
        code: Some(db.code().code().to_string()),
        message: Some(redact_shapes(db.message())),
        severity: Some(db.severity().to_string()),
        extra,
    };

    DriverError::new(kind, redact_shapes(db.message()))
        .with_hints(hints)
        .with_payload(payload)
}

fn position_hint(db: &DbError, sql: &str) -> Option<sluice_core::QueryPosition> {
    match db.position()? {
        // PostgreSQL's protocol defines `P` as 1-based and measured in
        // CHARACTERS (see the note on QueryPosition, and the live test below).
        ErrorPosition::Original(p) => position_from_1based_chars(sql, *p),
        // An error inside a function body: the position indexes the internal
        // query, not the user's text, so pointing at the user's script would
        // highlight an unrelated character.
        ErrorPosition::Internal { .. } => None,
    }
}

fn kind_for(code: &SqlState) -> ErrorKind {
    // Explicit codes first, then class prefixes.
    match *code {
        SqlState::QUERY_CANCELED => return ErrorKind::Cancelled,
        SqlState::INSUFFICIENT_PRIVILEGE => return ErrorKind::PermissionDenied,
        SqlState::UNDEFINED_TABLE
        | SqlState::UNDEFINED_COLUMN
        | SqlState::UNDEFINED_FUNCTION
        | SqlState::UNDEFINED_OBJECT => return ErrorKind::NotFound,
        SqlState::T_R_SERIALIZATION_FAILURE | SqlState::T_R_DEADLOCK_DETECTED => {
            return ErrorKind::Conflict
        }
        SqlState::INVALID_AUTHORIZATION_SPECIFICATION | SqlState::INVALID_PASSWORD => {
            return ErrorKind::AuthFailed
        }
        SqlState::SYNTAX_ERROR => return ErrorKind::SyntaxError,
        _ => {}
    }

    match &code.code()[..2] {
        "08" => ErrorKind::ConnectionFailed,
        "28" => ErrorKind::AuthFailed,
        "23" => ErrorKind::ConstraintViolation,
        "40" => ErrorKind::Conflict,
        "42" => ErrorKind::SyntaxError,
        "3D" | "3F" => ErrorKind::NotFound,
        "57" => ErrorKind::Timeout,
        // `Unknown` is first-class (D-017): a driver never has to lie.
        _ => ErrorKind::Unknown,
    }
}

fn is_retryable(code: &SqlState) -> bool {
    matches!(
        *code,
        SqlState::T_R_SERIALIZATION_FAILURE | SqlState::T_R_DEADLOCK_DETECTED
    ) || &code.code()[..2] == "08"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_maps_to_the_non_failure_kind() {
        assert_eq!(kind_for(&SqlState::QUERY_CANCELED), ErrorKind::Cancelled);
        assert!(!DriverError::new(ErrorKind::Cancelled, "x").is_failure());
    }

    #[test]
    fn class_prefixes_cover_codes_without_explicit_mappings() {
        assert_eq!(kind_for(&SqlState::UNIQUE_VIOLATION), ErrorKind::ConstraintViolation);
        assert_eq!(kind_for(&SqlState::SYNTAX_ERROR), ErrorKind::SyntaxError);
        assert_eq!(kind_for(&SqlState::INSUFFICIENT_PRIVILEGE), ErrorKind::PermissionDenied);
        assert_eq!(kind_for(&SqlState::UNDEFINED_TABLE), ErrorKind::NotFound);
        assert_eq!(kind_for(&SqlState::T_R_DEADLOCK_DETECTED), ErrorKind::Conflict);
    }

    #[test]
    fn unrecognised_codes_are_unknown_not_a_wrong_guess() {
        let odd = SqlState::from_code("XX999");
        assert_eq!(kind_for(&odd), ErrorKind::Unknown);
    }

    #[test]
    fn serialization_failures_are_marked_retryable() {
        assert!(is_retryable(&SqlState::T_R_SERIALIZATION_FAILURE));
        assert!(!is_retryable(&SqlState::SYNTAX_ERROR));
    }

    #[test]
    fn connection_strings_are_redacted_before_leaving_the_driver() {
        let msg = "could not connect to postgres://admin:hunter2@db.internal:5432/prod";
        let out = redact_shapes(msg);
        assert!(!out.contains("hunter2"), "{out}");
        assert!(out.contains("admin:[redacted]"), "{out}");
        assert!(out.contains("db.internal"), "host should survive: {out}");
    }

    #[test]
    fn redaction_leaves_ordinary_messages_alone() {
        let msg = "syntax error at or near \"slect\"";
        assert_eq!(redact_shapes(msg), msg);
    }
}
