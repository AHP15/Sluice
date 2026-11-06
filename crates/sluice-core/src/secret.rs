//! Type-enforced credential redaction (T-022 / D-020, R-017).
//!
//! D-020: *"Wrap secrets in a `Secret<String>` whose `Debug` and `Display`
//! print `[redacted]`."* The point is stated in its rationale — type-level
//! redaction makes password leaks **impossible rather than remembered at every
//! call site**.
//!
//! ## The asymmetry is the design
//!
//! `Secret<T>` deliberately implements `Deserialize` but **not** `Serialize`:
//!
//! - **In** — a parser (a connection URL, `.env`, a config subprocess's stdout)
//!   can deserialize *directly into* `Secret<String>`. This is what closes
//!   R-017: without it, every parser would land the value in a plain `String`
//!   and wrap it a few lines later, and that gap — "the window between secret
//!   enters the process and secret gets wrapped" — is precisely where
//!   connection-failure errors and debug logging happen.
//! - **Out** — there is no `Serialize`, so a `Secret` *cannot* be placed in an
//!   IPC response type at all. D-007's boundary stops being a rule someone has
//!   to remember and becomes a compile error. This composes with the
//!   `no_ipc_type_can_carry_a_credential` test in `commands.rs`: that test
//!   catches a plain-`String` leak, this type catches the wrapped kind.
//!
//! Reading the value back requires the deliberately-ugly [`Secret::expose`], so
//! every place a credential is actually handled is one `rg expose` away.

use std::fmt;

use serde::de::{Deserialize, Deserializer};

/// What every `Debug`/`Display` of a secret renders as.
pub const REDACTED: &str = "[redacted]";

/// A value that must never reach a log line, an error, or the webview.
///
/// Construct with [`Secret::new`] (or `.into()`), read with [`Secret::expose`].
#[derive(Clone)]
pub struct Secret<T> {
    inner: T,
}

impl<T> Secret<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Borrow the protected value.
    ///
    /// Named to be conspicuous at the call site and greppable in review — the
    /// set of places that legitimately need the plaintext is small and should
    /// stay small.
    pub fn expose(&self) -> &T {
        &self.inner
    }

    /// Consume the wrapper and yield the plaintext.
    ///
    /// Prefer [`Secret::expose`]; this exists for the handful of APIs that
    /// demand ownership.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T> From<T> for Secret<T> {
    fn from(inner: T) -> Self {
        Self::new(inner)
    }
}

/// The load-bearing impl. A derived `Debug` anywhere up the struct tree —
/// `ConnectSpec`, an error payload, a `tracing` field — renders this and not
/// the credential.
impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Secrets flow inward only. See the module docs: this is implemented so
/// parsers can land credentials *already wrapped*, closing R-017's window.
impl<'de, T> Deserialize<'de> for Secret<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Secret::new)
    }
}

/// Deserializing an *optional* credential from a source that may omit it.
///
/// `Option<Secret<T>>` already works via the blanket impl; this helper exists
/// for the `.env`/URL parsers (T-028) that represent "no password" as an empty
/// string rather than an absent key, where silently producing `Secret("")`
/// would send an empty password to the server instead of falling through to
/// the keychain.
pub fn deserialize_optional_secret<'de, D, T>(deserializer: D) -> Result<Option<Secret<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + AsRef<str>,
{
    let opt = Option::<T>::deserialize(deserializer)?;
    Ok(match opt {
        Some(v) if v.as_ref().is_empty() => None,
        Some(v) => Some(Secret::new(v)),
        None => None,
    })
}

impl<T> Secret<T>
where
    T: AsRef<str>,
{
    /// Whether the protected value is empty, without exposing it.
    pub fn is_empty(&self) -> bool {
        self.inner.as_ref().is_empty()
    }
}

// Note: `PartialEq` is deliberately not implemented. Equality on credentials
// invites `assert_eq!(secret, "hunter2")` and holding plaintext around to
// compare against. Write `a.expose() == b.expose()` if you truly mean it.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_renders_the_plaintext() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{:?}", s), REDACTED);
        assert!(!format!("{:?}", s).contains("hunter2"));
    }

    #[test]
    fn display_never_renders_the_plaintext() {
        let s = Secret::new("hunter2".to_string());
        assert_eq!(format!("{}", s), REDACTED);
        assert!(!format!("{}", s).contains("hunter2"));
    }

    /// The real-world shape: a derived `Debug` on an enclosing struct must
    /// inherit the redaction rather than needing its own hand-written impl.
    #[test]
    fn a_derived_debug_on_an_enclosing_struct_inherits_redaction() {
        #[derive(Debug)]
        struct Conn {
            user: String,
            password: Option<Secret<String>>,
        }

        let c = Conn {
            user: "postgres".into(),
            password: Some(Secret::new("correct-horse-battery-staple".into())),
        };
        let rendered = format!("{:?}", c);
        assert!(!rendered.contains("correct-horse-battery-staple"), "{rendered}");
        assert!(rendered.contains(REDACTED), "{rendered}");
        assert!(rendered.contains("postgres"), "non-secret fields still render");
    }

    /// R-017's actual mechanism: the parser lands the value already wrapped,
    /// so no plain `String` ever holds it.
    #[test]
    fn a_parser_can_deserialize_straight_into_a_secret() {
        #[derive(serde::Deserialize, Debug)]
        struct Parsed {
            user: String,
            password: Secret<String>,
        }

        let p: Parsed =
            serde_json::from_str(r#"{"user":"pg","password":"hunter2"}"#).expect("deserialize");
        assert_eq!(p.user, "pg");
        assert_eq!(p.password.expose(), "hunter2");
        assert!(!format!("{p:?}").contains("hunter2"));
    }

    #[test]
    fn an_empty_password_deserializes_to_none_rather_than_an_empty_secret() {
        #[derive(serde::Deserialize)]
        struct Parsed {
            #[serde(deserialize_with = "deserialize_optional_secret")]
            password: Option<Secret<String>>,
        }

        let empty: Parsed = serde_json::from_str(r#"{"password":""}"#).unwrap();
        assert!(empty.password.is_none(), "empty string must not become a secret");

        let null: Parsed = serde_json::from_str(r#"{"password":null}"#).unwrap();
        assert!(null.password.is_none());

        let real: Parsed = serde_json::from_str(r#"{"password":"pw"}"#).unwrap();
        assert_eq!(real.password.unwrap().expose(), "pw");
    }

    #[test]
    fn expose_returns_the_original_value() {
        let s: Secret<String> = "s3cret".to_string().into();
        assert_eq!(s.expose(), "s3cret");
        assert_eq!(s.clone().into_inner(), "s3cret");
        assert!(!s.is_empty());
    }
}
