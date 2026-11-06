//! Inert JSON/TOML config files (T-028 / D-026).
//!
//! D-026: *"Native Rust support for `DATABASE_URL`, `.env`, and plain JSON/TOML
//! config remains automatic and unprompted — those are parsed, never
//! executed."* TOML is deserialized into `serde_json::Value` so both formats
//! traverse one code path.

use std::path::Path;

use serde_json::Value;

/// Config filenames scanned in the working directory, in precedence order.
///
/// A fixed list rather than a glob: globbing `*.json` in an arbitrary
/// repository means reading files that have nothing to do with connections, and
/// the first key named `url` in one of them would be offered to the user as a
/// database.
pub const INERT_CONFIG_FILES: &[&str] = &[
    "sluice.toml",
    "sluice.json",
    ".sluicerc",
    ".sluicerc.json",
    "drizzle.config.json",
];

/// Config filenames that are **code**, not data.
///
/// Discovery records these and refuses to touch them. D-026 routes them through
/// per-directory consent instead; T-029 owns that path.
pub const EXECUTABLE_CONFIG_FILES: &[&str] = &[
    "drizzle.config.ts",
    "drizzle.config.js",
    "drizzle.config.mjs",
    "drizzle.config.cjs",
    "sluice.config.ts",
    "sluice.config.js",
];

/// Dotted key paths searched for a connection URL, in order.
///
/// Covers the shapes drizzle-kit and Prisma-style configs actually use.
const URL_KEY_PATHS: &[&str] = &[
    "url",
    "connectionString",
    "dbCredentials.url",
    "dbCredentials.connectionString",
    "db.url",
    "database.url",
    "datasource.url",
    "connection.url",
];

/// Whether a filename is a config this crate will *execute* rather than parse.
pub fn is_executable_config(name: &str) -> bool {
    EXECUTABLE_CONFIG_FILES.contains(&name)
}

/// Parse a config file's text into a `Value`, choosing the format from the
/// extension. Returns `None` for anything unparseable — a malformed config is
/// skipped, never fatal, because it may be unrelated to this tool.
pub fn parse_document(path: &Path, text: &str) -> Option<Value> {
    let is_toml = path.extension().is_some_and(|e| e == "toml");
    if is_toml {
        toml::from_str::<Value>(text).ok()
    } else {
        serde_json::from_str::<Value>(text).ok()
    }
}

/// Find the first connection-URL-shaped string in a parsed config document.
pub fn find_url(doc: &Value) -> Option<String> {
    for path in URL_KEY_PATHS {
        if let Some(Value::String(s)) = lookup(doc, path) {
            if crate::connection_url::looks_like_connection_url(s) {
                return Some(s.clone());
            }
        }
    }
    None
}

/// Resolve a dotted key path against a document.
fn lookup<'a>(doc: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut current = doc;
    for segment in dotted.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn json(s: &str) -> Value {
        parse_document(&PathBuf::from("x.json"), s).unwrap()
    }

    #[test]
    fn finds_a_top_level_url() {
        let d = json(r#"{"url":"postgres://localhost/app"}"#);
        assert_eq!(find_url(&d).unwrap(), "postgres://localhost/app");
    }

    #[test]
    fn finds_the_drizzle_kit_nested_shape() {
        let d = json(
            r#"{"dialect":"postgresql","dbCredentials":{"url":"postgres://u:p@h/db"}}"#,
        );
        assert_eq!(find_url(&d).unwrap(), "postgres://u:p@h/db");
    }

    #[test]
    fn parses_toml_through_the_same_path() {
        let d = parse_document(
            &PathBuf::from("sluice.toml"),
            "[database]\nurl = \"postgres://localhost/app\"\n",
        )
        .unwrap();
        assert_eq!(find_url(&d).unwrap(), "postgres://localhost/app");
    }

    /// A `url` key that is a homepage is not a connection. Without the scheme
    /// allowlist this would be offered to the user as a database.
    #[test]
    fn non_database_urls_are_not_treated_as_connections() {
        let d = json(r#"{"url":"https://example.com","name":"my-app"}"#);
        assert!(find_url(&d).is_none());
    }

    #[test]
    fn a_malformed_document_is_skipped_rather_than_fatal() {
        assert!(parse_document(&PathBuf::from("x.json"), "{not json").is_none());
        assert!(parse_document(&PathBuf::from("x.toml"), "[[[").is_none());
    }

    #[test]
    fn documents_without_a_url_yield_none() {
        assert!(find_url(&json(r#"{"dialect":"postgresql"}"#)).is_none());
        assert!(find_url(&json("[]")).is_none());
        assert!(find_url(&json("null")).is_none());
    }

    #[test]
    fn typescript_configs_are_classified_as_executable() {
        assert!(is_executable_config("drizzle.config.ts"));
        assert!(is_executable_config("sluice.config.js"));
        assert!(!is_executable_config("drizzle.config.json"));
        assert!(!is_executable_config("sluice.toml"));
    }
}
