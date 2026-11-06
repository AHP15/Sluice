//! Working-directory connection discovery (T-028 / D-026, D-012).
//!
//! D-012 makes `cd project && sluice` the headline ergonomic: the app discovers
//! connection details from the working directory and connects. D-026 splits that
//! discovery into two tiers with very different trust properties, and **this
//! crate's discovery path is the safe tier only**:
//!
//! | tier | sources | trust |
//! |---|---|---|
//! | inert (here) | `DATABASE_URL`, `.env`, JSON/TOML | parsed, never executed — automatic and unprompted |
//! | executable (T-029) | `*.config.ts`, `*.config.js` | *code execution*; per-directory consent required |
//!
//! R-001 is the `critical` risk this split exists to close: `cd
//! ~/Downloads/cloned-repo && sluice` must not execute that repository's config
//! in a process holding keychain access to every database the user has ever
//! configured. So discovery here **spawns no process and shows no prompt**. When
//! it encounters an executable config it records the path in
//! [`Discovery::executable_configs`] and stops — handing the decision to T-029's
//! consent gate rather than making it.
//!
//! That "no subprocess" claim is enforced rather than asserted: see
//! `tests/no_subprocess.rs`.

pub mod config_file;
pub mod connection_url;
pub mod consent;
pub mod dotenv;
pub mod evaluate;
pub mod imports;
pub mod interpreter;
pub mod launch;
pub mod sandbox;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use connection_url::{looks_like_connection_url, ConnectionUrl, UrlError};

/// The environment variable D-012 and D-026 both name explicitly.
pub const PRIMARY_ENV_VAR: &str = "DATABASE_URL";

/// Environment variables checked, in order.
const ENV_VARS: &[&str] = &[
    "DATABASE_URL",
    "POSTGRES_URL",
    "PG_CONNECTION_STRING",
    "MONGODB_URI",
    "MYSQL_URL",
];

/// Where a discovered connection came from.
///
/// Carried through to the UI because T-027 makes the guardrails depend on
/// provenance — a connection the user typed and one found lying in a directory
/// are not the same thing, and R-002 is about exactly that conflation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// A process environment variable.
    Environment { var: String },
    /// A key in a `.env` file.
    DotEnv { path: PathBuf, var: String },
    /// A key in an inert JSON/TOML config.
    ConfigFile { path: PathBuf },
}

impl ConfigSource {
    /// A short human description for the launcher and the connection banner.
    pub fn describe(&self) -> String {
        match self {
            ConfigSource::Environment { var } => format!("${var}"),
            ConfigSource::DotEnv { path, var } => {
                format!("{} ({var})", path.file_name().unwrap_or_default().to_string_lossy())
            }
            ConfigSource::ConfigFile { path } => {
                path.file_name().unwrap_or_default().to_string_lossy().into_owned()
            }
        }
    }
}

/// A connection found by discovery.
#[derive(Debug, Clone)]
pub struct DiscoveredConnection {
    pub source: ConfigSource,
    pub url: ConnectionUrl,
}

/// The result of scanning a directory.
#[derive(Debug, Default)]
pub struct Discovery {
    /// Connections found, in precedence order (first is the best candidate).
    pub connections: Vec<DiscoveredConnection>,
    /// Executable configs seen and deliberately **not** evaluated.
    ///
    /// D-026 routes these through per-directory consent. Discovery only reports
    /// them; T-029 decides whether any of them ever runs.
    pub executable_configs: Vec<PathBuf>,
}

impl Discovery {
    /// The connection the launcher should offer by default, if any.
    pub fn best(&self) -> Option<&DiscoveredConnection> {
        self.connections.first()
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty() && self.executable_configs.is_empty()
    }
}

/// Scan `dir` for connections, reading the process environment.
pub fn discover(dir: &Path) -> Discovery {
    discover_with_env(dir, |k| std::env::var(k).ok())
}

/// Scan `dir` for connections, resolving environment variables through `env`.
///
/// The environment is injected so tests are deterministic and so T-031's
/// subprocess environment policy has one place to control what discovery can
/// see. Precedence, highest first:
///
/// 1. a real process environment variable,
/// 2. a `.env` file in `dir`,
/// 3. inert JSON/TOML config files in `dir`, in [`INERT_CONFIG_FILES`] order.
///
/// A real environment variable outranks `.env` because that is what every
/// dotenv loader does, and because the ambient variable is the one the user set
/// most deliberately.
///
/// Only `dir` itself is scanned — there is no walk toward the filesystem root.
/// Walking upward would mean `cd ~/Downloads/repo/sub && sluice` silently
/// reading a config the user did not think they were in, which is the same
/// ambient-authority shape R-001 is about.
///
/// [`INERT_CONFIG_FILES`]: config_file::INERT_CONFIG_FILES
pub fn discover_with_env<F>(dir: &Path, env: F) -> Discovery
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = Discovery::default();
    let mut seen: Vec<String> = Vec::new();

    // 1. Process environment.
    for var in ENV_VARS {
        if let Some(raw) = env(var) {
            push_connection(
                &mut out,
                &mut seen,
                &raw,
                ConfigSource::Environment { var: (*var).to_string() },
            );
        }
    }

    // 2. `.env` in the working directory.
    let dotenv_path = dir.join(".env");
    if let Ok(text) = std::fs::read_to_string(&dotenv_path) {
        let vars: BTreeMap<String, String> = dotenv::parse(&text);
        for var in ENV_VARS {
            if let Some(raw) = vars.get(*var) {
                push_connection(
                    &mut out,
                    &mut seen,
                    raw,
                    ConfigSource::DotEnv {
                        path: dotenv_path.clone(),
                        var: (*var).to_string(),
                    },
                );
            }
        }
    }

    // 3. Inert config files.
    for name in config_file::INERT_CONFIG_FILES {
        let path = dir.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Some(doc) = config_file::parse_document(&path, &text) else { continue };
        if let Some(raw) = config_file::find_url(&doc) {
            push_connection(
                &mut out,
                &mut seen,
                &raw,
                ConfigSource::ConfigFile { path: path.clone() },
            );
        }
    }

    // 4. Note executable configs WITHOUT reading or running them.
    for name in config_file::EXECUTABLE_CONFIG_FILES {
        let path = dir.join(name);
        if path.is_file() {
            out.executable_configs.push(path);
        }
    }

    out
}

/// Parse and record a candidate, skipping duplicates.
///
/// Deduplication is on the **redacted** form, so two sources naming the same
/// database collapse into one entry without the password ever being compared or
/// retained as a plain `String` (T-022/R-017).
fn push_connection(
    out: &mut Discovery,
    seen: &mut Vec<String>,
    raw: &str,
    source: ConfigSource,
) {
    let Ok(url) = connection_url::parse(raw) else { return };
    let key = url.redacted();
    if seen.contains(&key) {
        return;
    }
    seen.push(key);
    out.connections.push(DiscoveredConnection { source, url });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Note: deliberately does not use `std::process::id()` for uniqueness.
    /// `tests/no_subprocess.rs` bans every `std::process` reference in the inert
    /// modules, and weakening that ban so a test helper can name a pid would
    /// trade a real safety property for a convenience.
    fn tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir()
            .join("sluice-config-tests")
            .join(format!("{tag}-{nanos}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn finds_database_url_in_the_environment() {
        let dir = tmpdir("env");
        let d = discover_with_env(&dir, |k| {
            (k == "DATABASE_URL").then(|| "postgres://u:p@h/app".to_string())
        });
        assert_eq!(d.connections.len(), 1);
        assert_eq!(d.best().unwrap().url.database.as_deref(), Some("app"));
        assert!(matches!(
            d.best().unwrap().source,
            ConfigSource::Environment { .. }
        ));
    }

    #[test]
    fn finds_database_url_in_a_dotenv_file() {
        let dir = tmpdir("dotenv");
        fs::write(dir.join(".env"), "DATABASE_URL=postgres://u:p@h/from_dotenv\n").unwrap();

        let d = discover_with_env(&dir, no_env);
        assert_eq!(d.connections.len(), 1);
        assert_eq!(d.best().unwrap().url.database.as_deref(), Some("from_dotenv"));
    }

    #[test]
    fn finds_a_url_in_an_inert_json_config() {
        let dir = tmpdir("json");
        fs::write(
            dir.join("drizzle.config.json"),
            r#"{"dbCredentials":{"url":"postgres://u:p@h/from_json"}}"#,
        )
        .unwrap();

        let d = discover_with_env(&dir, no_env);
        assert_eq!(d.best().unwrap().url.database.as_deref(), Some("from_json"));
    }

    #[test]
    fn finds_a_url_in_an_inert_toml_config() {
        let dir = tmpdir("toml");
        fs::write(
            dir.join("sluice.toml"),
            "[database]\nurl = \"postgres://u:p@h/from_toml\"\n",
        )
        .unwrap();

        let d = discover_with_env(&dir, no_env);
        assert_eq!(d.best().unwrap().url.database.as_deref(), Some("from_toml"));
    }

    #[test]
    fn a_real_environment_variable_outranks_dotenv_and_config_files() {
        let dir = tmpdir("precedence");
        fs::write(dir.join(".env"), "DATABASE_URL=postgres://u:p@h/from_dotenv\n").unwrap();
        fs::write(dir.join("sluice.json"), r#"{"url":"postgres://u:p@h/from_json"}"#).unwrap();

        let d = discover_with_env(&dir, |k| {
            (k == "DATABASE_URL").then(|| "postgres://u:p@h/from_env".to_string())
        });

        assert_eq!(d.best().unwrap().url.database.as_deref(), Some("from_env"));
        // All three are still offered, in precedence order.
        assert_eq!(d.connections.len(), 3);
        let dbs: Vec<_> =
            d.connections.iter().map(|c| c.url.database.clone().unwrap()).collect();
        assert_eq!(dbs, vec!["from_env", "from_dotenv", "from_json"]);
    }

    /// **The R-001 test.** A directory containing a TypeScript config must be
    /// reported, never read and never executed.
    #[test]
    fn a_typescript_config_is_reported_but_never_evaluated() {
        let dir = tmpdir("ts");
        fs::write(
            dir.join("drizzle.config.ts"),
            // If this were ever evaluated the process would exit non-zero, and
            // the value below must never appear in a discovered connection.
            r#"process.exit(1); export default { dbCredentials: { url: "postgres://evil@attacker/db" } };"#,
        )
        .unwrap();

        let d = discover_with_env(&dir, no_env);

        assert!(d.connections.is_empty(), "TS config must not yield a connection");
        assert_eq!(d.executable_configs.len(), 1);
        assert!(d.executable_configs[0].ends_with("drizzle.config.ts"));
        assert!(!d.is_empty(), "the directory is not empty — consent is pending");
    }

    #[test]
    fn duplicate_connections_across_sources_collapse() {
        let dir = tmpdir("dupe");
        fs::write(dir.join(".env"), "DATABASE_URL=postgres://u:p@h/same\n").unwrap();

        let d = discover_with_env(&dir, |k| {
            (k == "DATABASE_URL").then(|| "postgres://u:p@h/same".to_string())
        });
        assert_eq!(d.connections.len(), 1);
    }

    #[test]
    fn an_empty_directory_discovers_nothing() {
        let dir = tmpdir("empty");
        let d = discover_with_env(&dir, no_env);
        assert!(d.is_empty());
        assert!(d.best().is_none());
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let d = discover_with_env(Path::new("/nonexistent/definitely/not/here"), no_env);
        assert!(d.is_empty());
    }

    /// Discovery must not walk toward the filesystem root: `cd repo/sub` should
    /// not silently pick up `repo/.env`.
    #[test]
    fn discovery_does_not_walk_up_to_parent_directories() {
        let parent = tmpdir("nowalk");
        fs::write(parent.join(".env"), "DATABASE_URL=postgres://u:p@h/parent\n").unwrap();
        let child = parent.join("sub");
        fs::create_dir_all(&child).unwrap();

        let d = discover_with_env(&child, no_env);
        assert!(d.is_empty(), "must not read the parent's .env");
    }

    #[test]
    fn a_malformed_url_in_a_source_is_skipped_not_fatal() {
        let dir = tmpdir("malformed");
        fs::write(dir.join(".env"), "DATABASE_URL=not-a-url\n").unwrap();
        fs::write(dir.join("sluice.json"), r#"{"url":"postgres://u:p@h/ok"}"#).unwrap();

        let d = discover_with_env(&dir, no_env);
        assert_eq!(d.connections.len(), 1);
        assert_eq!(d.best().unwrap().url.database.as_deref(), Some("ok"));
    }

    #[test]
    fn discovered_passwords_are_wrapped_and_never_debug_printed() {
        let dir = tmpdir("secret");
        fs::write(dir.join(".env"), "DATABASE_URL=postgres://u:hunter2@h/app\n").unwrap();

        let d = discover_with_env(&dir, no_env);
        let rendered = format!("{d:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert_eq!(d.best().unwrap().url.password.as_ref().unwrap().expose(), "hunter2");
    }

    #[test]
    fn source_descriptions_are_human_readable() {
        let dir = tmpdir("describe");
        fs::write(dir.join(".env"), "DATABASE_URL=postgres://h/app\n").unwrap();
        let d = discover_with_env(&dir, no_env);
        assert_eq!(d.best().unwrap().source.describe(), ".env (DATABASE_URL)");
    }
}
