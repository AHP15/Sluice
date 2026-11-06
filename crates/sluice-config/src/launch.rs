//! The launcher's argument grammar (T-026 / D-012).
//!
//! D-012 fixes the grammar **now**, before there is anything headless to put
//! in it, so that `sluice query "select 1"` stays available later without
//! breaking `sluice postgres://...`:
//!
//! | invocation                | meaning                                      |
//! |---------------------------|----------------------------------------------|
//! | `sluice`                  | discover from the working directory          |
//! | `sluice <url>`            | connect to that URL                          |
//! | `sluice <path>`           | discover from that directory                 |
//! | `sluice --profile <name>` | open a saved profile                         |
//! | `sluice <reserved word>`  | an error: the word is reserved for later     |
//! | `sluice -- <anything>`    | the argument is a path, even a reserved word |
//!
//! A bare argument is a URL **if it parses as one**, otherwise a path — the
//! rule D-012 words exactly. This module is pure: it parses, and it checks a
//! path exists, but it neither discovers nor connects. It is the one grammar
//! both the first launch and the single-instance callback (a second invocation,
//! with *its* working directory) go through.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::connection_url::{self, ConnectionUrl};

/// Words a bare argument may never mean. D-012 defers headless subcommands but
/// reserves their namespace, so a directory named `query` is reached as
/// `sluice ./query` or `sluice -- query` rather than silently claiming the word.
pub const RESERVED_SUBCOMMANDS: &[&str] = &[
    "query", "dump", "export", "import", "exec", "run", "profile", "profiles", "connect",
    "config", "help", "version", "completions", "update", "serve",
];

/// What an invocation asks for.
#[derive(Debug)]
pub enum LaunchRequest {
    /// Discover connection details from this directory (absolute).
    Discover { dir: PathBuf },
    /// Connect to an explicit URL. Its password, if any, stays a `Secret`.
    Url(ConnectionUrl),
    /// Open a saved profile by name.
    Profile { name: String },
}

#[derive(Debug, PartialEq, Eq)]
pub enum LaunchError {
    Reserved { word: String },
    UnknownFlag { flag: String },
    MissingProfileName,
    TooManyArguments,
    /// Neither a parsable URL nor an existing directory. The text is the
    /// argument with any password removed, so it is safe to show.
    NotUrlOrDirectory { shown: String },
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LaunchError::Reserved { word } => write!(
                f,
                "`{word}` is reserved for a future command. To open a directory with that \
                 name, use `sluice ./{word}` or `sluice -- {word}`."
            ),
            LaunchError::UnknownFlag { flag } => write!(f, "unknown option `{flag}`"),
            LaunchError::MissingProfileName => write!(f, "`--profile` needs a profile name"),
            LaunchError::TooManyArguments => {
                write!(f, "expected at most one connection URL or directory")
            }
            LaunchError::NotUrlOrDirectory { shown } => write!(
                f,
                "`{shown}` is neither a connection URL nor an existing directory"
            ),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Parse the arguments after the program name, relative to `cwd`.
///
/// `cwd` is a parameter, not `std::env::current_dir()`, because a second
/// invocation arrives through the single-instance plugin carrying the *other*
/// process's working directory.
pub fn parse_args(args: &[OsString], cwd: &Path) -> Result<LaunchRequest, LaunchError> {
    let mut positional: Vec<String> = Vec::new();
    let mut profile: Option<String> = None;
    let mut after_separator = false;
    let mut forced_path = false;

    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned()).peekable();
    while let Some(arg) = it.next() {
        if after_separator {
            positional.push(arg);
            continue;
        }
        if arg == "--" {
            after_separator = true;
            forced_path = true;
            continue;
        }
        // macOS appends `-psn_0_<n>` when an app is launched from Finder.
        if arg.starts_with("-psn_") {
            continue;
        }
        if arg == "--profile" {
            match it.next() {
                Some(name) if !name.is_empty() && !name.starts_with('-') => profile = Some(name),
                _ => return Err(LaunchError::MissingProfileName),
            }
            continue;
        }
        if let Some(name) = arg.strip_prefix("--profile=") {
            if name.is_empty() {
                return Err(LaunchError::MissingProfileName);
            }
            profile = Some(name.to_string());
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            return Err(LaunchError::UnknownFlag { flag: redact(&arg) });
        }
        positional.push(arg);
    }

    match (profile, positional.len()) {
        (Some(name), 0) => Ok(LaunchRequest::Profile { name }),
        (Some(_), _) => Err(LaunchError::TooManyArguments),
        (None, 0) => Ok(LaunchRequest::Discover { dir: cwd.to_path_buf() }),
        (None, 1) => bare(&positional[0], cwd, forced_path),
        (None, _) => Err(LaunchError::TooManyArguments),
    }
}

fn bare(arg: &str, cwd: &Path, forced_path: bool) -> Result<LaunchRequest, LaunchError> {
    if !forced_path {
        if RESERVED_SUBCOMMANDS.contains(&arg) {
            return Err(LaunchError::Reserved { word: arg.to_string() });
        }
        if let Ok(url) = connection_url::parse(arg) {
            return Ok(LaunchRequest::Url(url));
        }
    }
    let p = Path::new(arg);
    let dir = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    if dir.is_dir() {
        Ok(LaunchRequest::Discover { dir: dir.canonicalize().unwrap_or(dir) })
    } else {
        Err(LaunchError::NotUrlOrDirectory { shown: redact(arg) })
    }
}

/// An argument for display: if it is URL-shaped, rebuild it without the
/// password (R-023: never substring-redact). Anything else is shown as is.
fn redact(arg: &str) -> String {
    if connection_url::looks_like_connection_url(arg) || arg.contains("://") {
        match connection_url::parse(arg) {
            Ok(u) => u.redacted(),
            Err(_) => "(an unparsable URL)".to_string(),
        }
    } else {
        arg.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    fn tmp() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join("sluice-launch").join(nanos.to_string());
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn no_arguments_discovers_from_the_working_directory() {
        let cwd = tmp();
        match parse_args(&args(&[]), &cwd).unwrap() {
            LaunchRequest::Discover { dir } => assert_eq!(dir, cwd),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bare_url_is_a_connection_and_keeps_its_password_secret() {
        let r = parse_args(&args(&["postgres://app:hunter2@db:5432/shop"]), &tmp()).unwrap();
        match r {
            LaunchRequest::Url(u) => {
                assert_eq!(u.host.as_deref(), Some("db"));
                assert_eq!(u.database.as_deref(), Some("shop"));
                assert!(!format!("{u:?}").contains("hunter2"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bare_non_url_is_a_directory_resolved_against_the_given_cwd() {
        let cwd = tmp();
        std::fs::create_dir(cwd.join("proj")).unwrap();
        match parse_args(&args(&["proj"]), &cwd).unwrap() {
            LaunchRequest::Discover { dir } => assert_eq!(dir, cwd.join("proj")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn neither_url_nor_directory_is_an_error_that_hides_the_password() {
        let err = parse_args(&args(&["nope-not-here"]), &tmp()).unwrap_err();
        assert!(matches!(err, LaunchError::NotUrlOrDirectory { .. }));

        let err = parse_args(&args(&["--bogus=postgres://u:hunter2@h/db"]), &tmp()).unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
    }

    #[test]
    fn reserved_words_are_refused_and_explain_the_escape() {
        for w in ["query", "dump", "help"] {
            let err = parse_args(&args(&[w]), &tmp()).unwrap_err();
            assert_eq!(err, LaunchError::Reserved { word: w.into() });
            assert!(err.to_string().contains(&format!("sluice -- {w}")), "{err}");
        }
    }

    /// A directory that happens to be called `query` is still reachable.
    #[test]
    fn the_separator_and_a_relative_path_reach_a_directory_named_like_a_subcommand() {
        let cwd = tmp();
        std::fs::create_dir(cwd.join("query")).unwrap();
        for a in [vec!["--", "query"], vec!["./query"]] {
            match parse_args(&args(&a), &cwd).unwrap() {
                LaunchRequest::Discover { dir } => assert_eq!(dir, cwd.join("query")),
                other => panic!("{a:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn profile_forms() {
        for a in [vec!["--profile", "prod-ro"], vec!["--profile=prod-ro"]] {
            match parse_args(&args(&a), &tmp()).unwrap() {
                LaunchRequest::Profile { name } => assert_eq!(name, "prod-ro"),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(
            parse_args(&args(&["--profile"]), &tmp()).unwrap_err(),
            LaunchError::MissingProfileName
        );
        assert_eq!(
            parse_args(&args(&["--profile", "--other"]), &tmp()).unwrap_err(),
            LaunchError::MissingProfileName
        );
        assert_eq!(
            parse_args(&args(&["--profile", "a", "postgres://h/db"]), &tmp()).unwrap_err(),
            LaunchError::TooManyArguments
        );
    }

    #[test]
    fn unknown_flags_and_extra_arguments_are_errors() {
        assert!(matches!(
            parse_args(&args(&["--verbose"]), &tmp()).unwrap_err(),
            LaunchError::UnknownFlag { .. }
        ));
        assert_eq!(
            parse_args(&args(&["postgres://h/a", "postgres://h/b"]), &tmp()).unwrap_err(),
            LaunchError::TooManyArguments
        );
    }

    #[test]
    fn the_finder_process_serial_number_is_ignored() {
        let cwd = tmp();
        assert!(matches!(
            parse_args(&args(&["-psn_0_12345"]), &cwd).unwrap(),
            LaunchRequest::Discover { .. }
        ));
    }
}
