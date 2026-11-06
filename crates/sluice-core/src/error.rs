//! The layered error model (T-066, D-017).
//!
//! Three layers, in order of how much the UI should trust them:
//!
//! 1. A small normalized [`ErrorKind`] — with `Unknown` **first-class**, so a
//!    driver never has to lie about a code it does not recognise.
//! 2. Optional structured [`ErrorHints`] — query position, constraint, column,
//!    retryability. Present when the engine gave them, absent otherwise.
//! 3. The engine's raw payload, **always preserved verbatim**.
//!
//! D-017's reasoning: raw passthrough blocks syntax highlighting and retry
//! logic, while a fully closed taxonomy demands endless per-engine mapping and
//! still loses the detail the user actually needed.

use serde::{Deserialize, Serialize};

/// The normalized failure classes from D-017.
///
/// This list is deliberately exactly D-017's and no longer: every addition is a
/// per-engine mapping obligation, and `Unknown` already covers the tail without
/// forcing a driver to misreport. TLS failures are `ConnectionFailed` with the
/// detail preserved in the payload.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    ConnectionFailed,
    AuthFailed,
    Timeout,
    /// **Not a failure.** A user-initiated cancel surfacing as a red error makes
    /// the app feel broken every time someone stops a slow query (D-017), so
    /// this is a distinct kind and [`DriverError::is_failure`] returns false.
    Cancelled,
    SyntaxError,
    ConstraintViolation,
    PermissionDenied,
    NotFound,
    Conflict,
    /// The driver itself misbehaved — including a caught panic (T-006/R-022).
    DriverBug,
    /// First-class, not a fallback of last resort.
    Unknown,
}

/// Where in the submitted statement the engine says the problem is.
///
/// ## Index units, and why this type is not just a `u32`
///
/// D-017 says to "convert Postgres' byte-offset error position to a character
/// index at the driver boundary". Measured against a live PostgreSQL 17 server
/// (see `crates/sluice-postgres/src/error_map.rs` tests), the protocol's `P`
/// field is **not** a byte offset — the spec defines it as 1-based and measured
/// in characters, and the server behaves that way.
///
/// The conversion is still required, just not the one named: the webview is
/// JavaScript, where string indices are **UTF-16 code units**. A Postgres
/// codepoint index and a UTF-16 index diverge on any astral character (emoji,
/// some CJK extensions), so an unconverted index highlights the wrong character
/// — the exact failure D-017 was guarding against, reached by a different route.
///
/// So this type carries the index it is expressed in, explicitly, rather than an
/// unlabelled integer that every consumer would have to guess about.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryPosition {
    /// 0-based offset in **UTF-16 code units**, ready for the webview and for
    /// CodeMirror without further adjustment.
    pub utf16_offset: u32,
    /// 0-based offset in Unicode scalar values, for non-JS consumers.
    pub char_offset: u32,
}

/// Structured hints. Every field is optional because every engine supplies a
/// different subset, and inventing values to fill the struct would be worse
/// than admitting absence.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ErrorHints {
    pub position: Option<QueryPosition>,
    pub constraint: Option<String>,
    pub column: Option<String>,
    pub table: Option<String>,
    pub schema: Option<String>,
    /// Whether retrying the same statement unchanged could plausibly succeed.
    /// `None` means the driver has no opinion — distinct from `Some(false)`.
    pub retryable: Option<bool>,
}

impl ErrorHints {
    pub fn is_empty(&self) -> bool {
        *self == ErrorHints::default()
    }
}

/// The engine's own error, preserved verbatim (D-017 layer 3).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct EnginePayload {
    /// Engine-native code: Postgres SQLSTATE, SQL Server error number, Mongo
    /// `codeName`.
    pub code: Option<String>,
    /// The engine's own message text, unmodified.
    pub message: Option<String>,
    /// Engine severity string as given.
    pub severity: Option<String>,
    /// Anything else worth keeping, as engine-named key/value pairs. Not
    /// interpreted by the UI; rendered in a details pane.
    pub extra: Vec<(String, String)>,
}

/// A driver error: normalized kind, optional hints, verbatim payload.
///
/// `message` is the display string and **must already be redacted** — T-024
/// enforces that at the boundary, because connection failures routinely embed
/// the whole connection string including the password (D-017/D-007).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct DriverError {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "ErrorHints::is_empty")]
    pub hints: ErrorHints,
    #[serde(default)]
    pub payload: EnginePayload,
}

impl DriverError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            hints: ErrorHints::default(),
            payload: EnginePayload::default(),
        }
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unknown, message)
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "query cancelled")
    }

    pub fn with_hints(mut self, hints: ErrorHints) -> Self {
        self.hints = hints;
        self
    }

    pub fn with_payload(mut self, payload: EnginePayload) -> Self {
        self.payload = payload;
        self
    }

    /// Convenience for the common "keep the engine text" case.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.payload.message = Some(detail.into());
        self
    }

    /// D-017: a cancel is an outcome, not a failure. The UI must not paint it
    /// red or offer "report a bug".
    pub fn is_failure(&self) -> bool {
        self.kind != ErrorKind::Cancelled
    }
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{:?}] {}", self.kind, self.message)
    }
}

impl std::error::Error for DriverError {}

/// Convert a **1-based Unicode-scalar** position, as PostgreSQL reports it, into
/// the dual-unit [`QueryPosition`] the UI can use directly.
///
/// Returns `None` when the position falls outside the statement, which is the
/// honest answer — a hint that points nowhere is worse than no hint, because the
/// editor would highlight an arbitrary character with full confidence.
pub fn position_from_1based_chars(statement: &str, one_based: u32) -> Option<QueryPosition> {
    if one_based == 0 {
        return None;
    }
    let target = (one_based - 1) as usize;

    let mut chars = 0usize;
    let mut utf16 = 0usize;
    for ch in statement.chars() {
        if chars == target {
            return Some(QueryPosition {
                utf16_offset: utf16 as u32,
                char_offset: chars as u32,
            });
        }
        chars += 1;
        utf16 += ch.len_utf16();
    }

    // A position exactly one past the end is legitimate: "unexpected end of
    // input" points at the terminator.
    if chars == target {
        return Some(QueryPosition {
            utf16_offset: utf16 as u32,
            char_offset: chars as u32,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_positions_are_identical_in_both_units() {
        let p = position_from_1based_chars("select * from t", 8).unwrap();
        assert_eq!(p.char_offset, 7);
        assert_eq!(p.utf16_offset, 7);
    }

    /// The whole reason `QueryPosition` carries two units. An astral character
    /// is one Unicode scalar but TWO UTF-16 code units, so a webview that used
    /// the codepoint index would highlight the wrong character from here on.
    #[test]
    fn astral_characters_shift_the_utf16_offset() {
        // "🐈" is one char, two UTF-16 code units.
        let sql = "select '🐈' , bad from t";
        let p = position_from_1based_chars(sql, 12).unwrap();
        assert_eq!(p.char_offset, 11);
        assert_eq!(p.utf16_offset, 12, "emoji must push the UTF-16 index by one");
    }

    #[test]
    fn multibyte_but_bmp_characters_do_not_shift_utf16() {
        // "é" and "世" are multi-BYTE but single UTF-16 unit.
        let sql = "select 'é世' , bad";
        let p = position_from_1based_chars(sql, 12).unwrap();
        assert_eq!(p.char_offset, 11);
        assert_eq!(p.utf16_offset, 11);
    }

    #[test]
    fn out_of_range_positions_are_dropped_not_clamped() {
        assert!(position_from_1based_chars("select 1", 99).is_none());
        assert!(position_from_1based_chars("select 1", 0).is_none());
        // One past the end is valid: "unexpected end of input".
        assert!(position_from_1based_chars("select 1", 9).is_some());
    }

    #[test]
    fn cancelled_is_not_a_failure() {
        assert!(!DriverError::cancelled().is_failure());
        assert!(DriverError::new(ErrorKind::SyntaxError, "boom").is_failure());
    }

    #[test]
    fn empty_hints_are_omitted_from_the_wire() {
        let e = DriverError::new(ErrorKind::Unknown, "x");
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains("hints"), "empty hints should not be serialized: {json}");

        let e = e.with_hints(ErrorHints { retryable: Some(true), ..Default::default() });
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("retryable"));
    }
}
