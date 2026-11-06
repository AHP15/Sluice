//! Connection-URL parsing (T-028 / D-026).
//!
//! This is a **credential-bearing parser** in T-022/R-017's sense: the password
//! crosses from raw input bytes into process memory here, so it lands directly
//! in `Secret<String>` and never exists as a plain `String`. The raw URL text is
//! consumed and dropped inside this module — in particular, no error returned
//! from here carries the input, because the input contains the password.

use std::collections::BTreeMap;

use percent_encoding::percent_decode_str;
use sluice_core::Secret;

/// Schemes recognised as database connections.
///
/// The list is a allowlist rather than "anything with a host", because config
/// files are full of URLs that are not connection strings — a `homepage`, a
/// `registry`, a webhook. Treating one of those as a connection would put a
/// bogus entry in front of the user at launch.
pub const DATABASE_SCHEMES: &[&str] = &[
    "postgres",
    "postgresql",
    "mysql",
    "mariadb",
    "mongodb",
    "mongodb+srv",
    "sqlserver",
    "mssql",
    "sqlite",
    "libsql",
];

#[derive(Debug, PartialEq, Eq)]
pub enum UrlError {
    /// Not a URL at all. Deliberately carries no detail: the input is a
    /// credential.
    Malformed,
    /// Parsed, but not a database connection (`https`, `file`, ...).
    NotADatabaseScheme,
    /// Percent-encoding in a component was not valid UTF-8.
    UndecodableComponent,
}

impl std::fmt::Display for UrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Never interpolate the input — see the module docs.
            UrlError::Malformed => write!(f, "not a valid connection URL"),
            UrlError::NotADatabaseScheme => write!(f, "not a database connection URL"),
            UrlError::UndecodableComponent => write!(f, "connection URL contains invalid encoding"),
        }
    }
}

impl std::error::Error for UrlError {}

/// A parsed connection URL.
///
/// `Debug` is derived and safe: `password` is a `Secret`, which renders
/// `[redacted]` (T-022).
#[derive(Debug, Clone)]
pub struct ConnectionUrl {
    pub scheme: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<Secret<String>>,
    pub database: Option<String>,
    pub params: BTreeMap<String, String>,
}

impl ConnectionUrl {
    /// Whether the URL carried an inline password.
    ///
    /// T-027 and T-034 both need this without touching the value.
    pub fn has_inline_password(&self) -> bool {
        self.password.is_some()
    }

    /// The URL with the password removed — safe to display, log, or persist.
    ///
    /// Rebuilt from the parsed components rather than by string-substituting the
    /// original, which is R-023's failure mode: substring redaction misses
    /// URL-encoded and split password forms.
    pub fn redacted(&self) -> String {
        let mut s = String::new();
        s.push_str(&self.scheme);
        s.push_str("://");
        if let Some(u) = &self.user {
            s.push_str(u);
            if self.password.is_some() {
                s.push_str(":***");
            }
            s.push('@');
        }
        if let Some(h) = &self.host {
            s.push_str(h);
        }
        if let Some(p) = self.port {
            s.push_str(&format!(":{p}"));
        }
        if let Some(d) = &self.database {
            s.push('/');
            s.push_str(d);
        }
        if !self.params.is_empty() {
            s.push('?');
            let joined: Vec<String> =
                self.params.iter().map(|(k, v)| format!("{k}={v}")).collect();
            s.push_str(&joined.join("&"));
        }
        s
    }
}

fn decode(raw: &str) -> Result<String, UrlError> {
    percent_decode_str(raw)
        .decode_utf8()
        .map(|c| c.into_owned())
        .map_err(|_| UrlError::UndecodableComponent)
}

/// Parse a connection URL.
///
/// Returns `Err` rather than a partially-populated struct, so a caller cannot
/// accidentally treat a failed parse as a connection with no password.
pub fn parse(raw: &str) -> Result<ConnectionUrl, UrlError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(UrlError::Malformed);
    }

    let parsed = url::Url::parse(trimmed).map_err(|_| UrlError::Malformed)?;

    let scheme = parsed.scheme().to_ascii_lowercase();
    if !DATABASE_SCHEMES.contains(&scheme.as_str()) {
        return Err(UrlError::NotADatabaseScheme);
    }

    // `Url::username`/`password` return the still-percent-encoded text, so a
    // password of `p@ss:word` arrives as `p%40ss%3Aword` and MUST be decoded —
    // sending the encoded form to the server is a silent auth failure.
    let user = match parsed.username() {
        "" => None,
        u => Some(decode(u)?),
    };

    let mut password = match parsed.password() {
        None | Some("") => None,
        Some(p) => Some(Secret::new(decode(p)?)),
    };

    let database = {
        let path = parsed.path().trim_start_matches('/');
        if path.is_empty() {
            None
        } else {
            Some(decode(path)?)
        }
    };

    let mut params = BTreeMap::new();
    for (k, v) in parsed.query_pairs() {
        // T-034: libpq also accepts `?password=…`. It is a password like any
        // other, so it goes into the `Secret` — never into `params`, which are
        // plain strings that `redacted()` prints and callers may persist. The
        // userinfo password wins if both are given, matching libpq.
        if k.eq_ignore_ascii_case("password") {
            if password.is_none() && !v.is_empty() {
                password = Some(Secret::new(v.into_owned()));
            }
            continue;
        }
        params.insert(k.into_owned(), v.into_owned());
    }

    Ok(ConnectionUrl {
        scheme,
        host: parsed.host_str().map(|h| h.to_string()),
        port: parsed.port(),
        user,
        password,
        database,
        params,
    })
}

/// Whether a bare string looks like a connection URL.
///
/// D-012 fixes the argument grammar as *"a bare argument is a connection URL if
/// it parses as one, otherwise a directory path"*, so T-026 needs exactly this
/// predicate — and it must not be "contains `://`", which would classify
/// `https://...` as a connection and a Windows path as a URL.
pub fn looks_like_connection_url(s: &str) -> bool {
    parse(s).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-034: a query-string password is a password, not a parameter.
    #[test]
    fn a_query_string_password_is_secret_and_never_a_param() {
        let u = parse("postgres://alice@db.example.com/shop?password=s%3Dcret&sslmode=require").unwrap();
        assert_eq!(u.password.as_ref().unwrap().expose(), "s=cret");
        assert!(!u.params.contains_key("password"));
        assert_eq!(u.params.get("sslmode").map(String::as_str), Some("require"));
        let shown = u.redacted();
        assert!(!shown.contains("cret"), "{shown}");

        // Upper-case key too; and userinfo wins when both are present.
        let u = parse("postgres://alice:inline@h/db?PASSWORD=query").unwrap();
        assert_eq!(u.password.as_ref().unwrap().expose(), "inline");
        assert!(u.params.is_empty(), "{:?}", u.params.keys());
    }

    #[test]
    fn parses_a_full_postgres_url() {
        let u = parse("postgres://alice:hunter2@db.example.com:5433/shop?sslmode=require").unwrap();
        assert_eq!(u.scheme, "postgres");
        assert_eq!(u.host.as_deref(), Some("db.example.com"));
        assert_eq!(u.port, Some(5433));
        assert_eq!(u.user.as_deref(), Some("alice"));
        assert_eq!(u.password.as_ref().unwrap().expose(), "hunter2");
        assert_eq!(u.database.as_deref(), Some("shop"));
        assert_eq!(u.params.get("sslmode").map(String::as_str), Some("require"));
    }

    /// R-023's shape, at the parse boundary rather than the redaction boundary:
    /// a password containing URL-significant characters must be decoded, or we
    /// send the wrong password and blame the server.
    #[test]
    fn percent_encoded_credentials_are_decoded() {
        let u = parse("postgres://a%40corp:p%40ss%3Aword%2F1@h/db").unwrap();
        assert_eq!(u.user.as_deref(), Some("a@corp"));
        assert_eq!(u.password.as_ref().unwrap().expose(), "p@ss:word/1");
    }

    #[test]
    fn a_url_without_a_password_yields_none_not_an_empty_secret() {
        let u = parse("postgres://alice@localhost/shop").unwrap();
        assert!(u.password.is_none());
        assert!(!u.has_inline_password());

        // An explicitly empty password is also None — an empty string would be
        // sent to the server as a real (wrong) password.
        let u = parse("postgres://alice:@localhost/shop").unwrap();
        assert!(u.password.is_none());
    }

    #[test]
    fn non_database_schemes_are_rejected() {
        assert_eq!(parse("https://example.com").unwrap_err(), UrlError::NotADatabaseScheme);
        assert_eq!(parse("file:///etc/passwd").unwrap_err(), UrlError::NotADatabaseScheme);
    }

    #[test]
    fn garbage_is_malformed_rather_than_panicking() {
        assert_eq!(parse("").unwrap_err(), UrlError::Malformed);
        assert_eq!(parse("   ").unwrap_err(), UrlError::Malformed);
        assert_eq!(parse("not a url").unwrap_err(), UrlError::Malformed);
    }

    /// The parser's errors are rendered in the UI and written to logs, and its
    /// input is a credential. R-017: the raw input must not survive into them.
    #[test]
    fn errors_never_carry_the_input() {
        for bad in ["postgres://u:hunter2@", "hunter2", "https://u:hunter2@h/"] {
            if let Err(e) = parse(bad) {
                let text = e.to_string();
                assert!(!text.contains("hunter2"), "leaked via {bad:?}: {text}");
            }
        }
    }

    /// R-023: rebuild from components, never substring-replace the original.
    #[test]
    fn redacted_form_drops_the_password_and_survives_encoding() {
        let u = parse("postgres://alice:p%40ss%3Aword@db:5432/shop?sslmode=require").unwrap();
        let r = u.redacted();
        assert!(!r.contains("p@ss:word"), "{r}");
        assert!(!r.contains("p%40ss"), "{r}");
        assert!(r.contains("alice"));
        assert!(r.contains("db:5432"));
        assert!(r.contains("shop"));
    }

    #[test]
    fn a_debug_render_never_exposes_the_password() {
        let u = parse("postgres://alice:hunter2@h/db").unwrap();
        let d = format!("{u:?}");
        assert!(!d.contains("hunter2"), "{d}");
        assert!(d.contains("[redacted]"), "{d}");
    }

    /// D-012's argument grammar rests on this predicate.
    #[test]
    fn the_bare_argument_predicate_separates_urls_from_paths() {
        assert!(looks_like_connection_url("postgres://localhost/db"));
        assert!(looks_like_connection_url("mongodb+srv://c.example.com/app"));

        assert!(!looks_like_connection_url("./some/project"));
        assert!(!looks_like_connection_url("/Users/pro/project"));
        assert!(!looks_like_connection_url("C:\\Users\\pro\\project"));
        assert!(!looks_like_connection_url("https://example.com"));
    }

    #[test]
    fn urls_without_a_database_or_port_are_still_valid() {
        let u = parse("postgres://localhost").unwrap();
        assert_eq!(u.host.as_deref(), Some("localhost"));
        assert!(u.port.is_none());
        assert!(u.database.is_none());
        assert!(u.user.is_none());
    }
}
