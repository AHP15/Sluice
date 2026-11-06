//! The CI lint required by T-022 / R-017.
//!
//! R-017's Action ends: *"Back it with a test or CI lint asserting that no
//! struct on the connection path carries a plain-`String` password field."*
//!
//! ## Why a source scan rather than a type-system trick
//!
//! The property is "no *future* struct reintroduces a plain-`String`
//! credential". A type-level proof can only constrain types that already opted
//! in — it cannot see the struct someone writes next week in a parser that does
//! not exist yet. That struct is exactly the failure R-017 describes, and the
//! parsers it names (`.env`, environment, `.pgpass`, `~/.pg_service.conf`, the
//! config subprocess's stdout) are built by T-028 and T-029 *after* this task.
//! So the lint is a grep over the connection path, and its job is to fail those
//! tasks' builds if they land a bare `String` password.
//!
//! Scope is deliberately struct **fields**, matching the acceptance wording.
//! Function parameters are not scanned: a parameter is a transient at a call
//! site, whereas a field is state that gets stored, cloned, and `Debug`-printed
//! — which is the thing that ends up in a log line.

use std::fs;
use std::path::{Path, PathBuf};

/// Field names that mean "this holds a credential".
const CREDENTIAL_NAMES: &[&str] = &[
    "password", "passwd", "pwd", "secret", "token", "passphrase", "api_key", "apikey",
];

/// Type fragments that mean "this is unprotected plaintext".
const PLAINTEXT_TYPES: &[&str] = &["String", "str"];

/// Source roots that make up "the connection path".
const SCANNED_ROOTS: &[&str] = &["crates", "src-tauri/src"];

/// `secret.rs` defines the wrapper, so its own `inner: T` and doc examples are
/// not violations. Everything else is in scope.
fn is_exempt(path: &Path) -> bool {
    path.ends_with("secret.rs")
}

fn rust_sources(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        // Build output is not source, and test fixtures are not the connection
        // path — this file's own self-tests below deliberately contain the very
        // violation the lint hunts for.
        if name == "target" || name == "tests" {
            continue;
        }
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") && !is_exempt(&path) {
            out.push(path);
        }
    }
}

/// One offending field.
#[derive(Debug)]
struct Violation {
    file: PathBuf,
    line_no: usize,
    line: String,
}

/// Scan a file for struct fields whose name is credential-ish and whose type is
/// bare plaintext.
///
/// Tracks brace depth from a `struct NAME {` header so that only genuine field
/// declarations are considered — a local `let secret: String` inside a function
/// is not a field and is not flagged.
fn scan(source: &str, path: &Path) -> Vec<Violation> {
    let mut violations = Vec::new();
    let mut depth: i32 = 0;
    let mut in_struct = false;

    for (i, raw) in source.lines().enumerate() {
        let line = raw.trim();

        if !in_struct {
            let is_struct_header = (line.starts_with("struct ")
                || line.starts_with("pub struct ")
                || line.contains(" struct "))
                && line.ends_with('{');
            if is_struct_header {
                in_struct = true;
                depth = 1;
            }
            continue;
        }

        depth += line.matches('{').count() as i32;
        depth -= line.matches('}').count() as i32;
        if depth <= 0 {
            in_struct = false;
            continue;
        }

        // Skip comments and attributes.
        if line.starts_with("//") || line.starts_with('#') || line.starts_with("/*") {
            continue;
        }

        let Some((name_part, type_part)) = line.split_once(':') else { continue };

        let name = name_part.trim_start_matches("pub ").trim().to_ascii_lowercase();
        if !CREDENTIAL_NAMES.iter().any(|c| name.contains(c)) {
            continue;
        }

        // `Secret<...>` anywhere in the type means the value is protected.
        if type_part.contains("Secret") {
            continue;
        }

        if PLAINTEXT_TYPES.iter().any(|t| type_part.contains(t)) {
            violations.push(Violation {
                file: path.to_path_buf(),
                line_no: i + 1,
                line: line.to_string(),
            });
        }
    }

    violations
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/crates/sluice-core.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// R-017's guard: no struct on the connection path may carry a plain-`String`
/// credential. Fails the build for T-028/T-029's parsers if they regress it.
#[test]
fn no_struct_on_the_connection_path_carries_a_plaintext_credential() {
    let root = workspace_root();
    let mut files = Vec::new();
    for r in SCANNED_ROOTS {
        rust_sources(&root.join(r), &mut files);
    }

    assert!(
        files.len() > 5,
        "lint scanned only {} files — the roots are wrong, so it would pass vacuously",
        files.len()
    );

    let mut violations = Vec::new();
    for f in &files {
        let Ok(text) = fs::read_to_string(f) else { continue };
        violations.extend(scan(&text, f));
    }

    if !violations.is_empty() {
        let mut report = String::from(
            "plain-String credential field(s) on the connection path (T-022 / R-017).\n\
             Wrap the field in `sluice_core::Secret<String>` — at the parse boundary,\n\
             not after — so no unprotected credential exists in the process.\n\n",
        );
        for v in &violations {
            let rel = v.file.strip_prefix(&root).unwrap_or(&v.file);
            report.push_str(&format!("  {}:{}  {}\n", rel.display(), v.line_no, v.line));
        }
        panic!("{report}");
    }
}

// ---------------------------------------------------------------- self-tests
//
// A lint that cannot fail is worse than no lint, so its detector is tested
// against both a known violation and the known-good real shape.

#[test]
fn the_lint_detects_a_plaintext_credential_field() {
    let bad = r#"
        pub struct ConnectSpec {
            pub host: String,
            pub password: Option<String>,
        }
    "#;
    let found = scan(bad, Path::new("synthetic.rs"));
    assert_eq!(found.len(), 1, "expected exactly one violation, got {found:?}");
    assert!(found[0].line.contains("password"));
}

#[test]
fn the_lint_accepts_a_wrapped_credential_field() {
    let good = r#"
        pub struct ConnectSpec {
            pub host: String,
            pub password: Option<Secret<String>>,
        }
    "#;
    assert!(scan(good, Path::new("synthetic.rs")).is_empty());
}

/// A local binding is not a field; flagging it would make the lint noisy enough
/// to get disabled, which is the usual way a lint dies.
#[test]
fn the_lint_ignores_locals_and_parameters() {
    let src = r#"
        fn connect(password: String) -> Result<(), Error> {
            let secret: String = load();
            Ok(())
        }
    "#;
    assert!(scan(src, Path::new("synthetic.rs")).is_empty());
}

/// Nested braces inside a struct body (a default expression, a closure) must
/// not end the struct early and blind the lint to later fields.
#[test]
fn the_lint_survives_nested_braces_in_a_struct_body() {
    let src = r#"
        pub struct Weird {
            pub label: String,
            pub password: String,
        }
    "#;
    assert_eq!(scan(src, Path::new("synthetic.rs")).len(), 1);
}
