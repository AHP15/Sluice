//! Enforces T-028's acceptance clause "**with no subprocess and no prompt**",
//! which is the concrete expression of D-026's inert tier and the reason R-001
//! (`critical`) does not apply to this path.
//!
//! A comment saying "this module does not spawn processes" is worth nothing the
//! moment someone adds a convenience helper. This test makes the claim
//! mechanical: the inert discovery modules may not reference process spawning or
//! dialog APIs at all.
//!
//! T-029 will add the consent-gated evaluator, which *does* spawn a subprocess.
//! It must land in its own module, and that module must be added to
//! `EXECUTABLE_MODULES` below rather than to the inert set — the split is the
//! point.

use std::fs;
use std::path::{Path, PathBuf};

/// Modules that make up the automatic, unprompted discovery path.
const INERT_MODULES: &[&str] =
    &["lib.rs", "connection_url.rs", "dotenv.rs", "config_file.rs", "consent.rs", "interpreter.rs", "launch.rs", "imports.rs", "sandbox.rs"];

/// Modules permitted to spawn a subprocess, because consent gates them.
const EXECUTABLE_MODULES: &[&str] = &["evaluate.rs"];

/// Symbols that mean "this code can execute something".
const SPAWN_MARKERS: &[&str] = &[
    "std::process",
    "process::Command",
    "Command::new",
    "std::os::unix::process",
    "CommandExt",
];

/// Symbols that mean "this code can block on the user".
///
/// D-026 puts the prompt behind consent, in T-029. Discovery itself must be
/// silent, or `sluice` in a directory becomes a modal dialog before the window
/// even opens.
const PROMPT_MARKERS: &[&str] = &["rfd::", "MessageDialog", "read_line", "stdin()"];

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Strip `//` line comments so the module docs above — which legitimately
/// mention `std::process` while explaining why it is absent — do not trip the
/// scan. This is why the check is not a bare `rg`.
fn code_only(source: &str) -> String {
    source
        .lines()
        .map(|l| {
            let t = l.trim_start();
            if t.starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_inert_discovery_path_cannot_spawn_a_subprocess() {
    let dir = src_dir();
    let mut checked = 0;

    for module in INERT_MODULES {
        let path = dir.join(module);
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("inert module {module} is missing ({e}) — if it was renamed, update INERT_MODULES"));
        let code = code_only(&source);

        for marker in SPAWN_MARKERS {
            assert!(
                !code.contains(marker),
                "{module} references `{marker}`.\n\
                 T-028/D-026: the inert discovery path is parsed, never executed — it is what \
                 makes automatic, unprompted discovery safe (R-001, critical).\n\
                 If this is the consent-gated evaluator, it belongs in its own module listed in \
                 EXECUTABLE_MODULES, behind T-029's consent gate."
            );
        }
        checked += 1;
    }

    assert_eq!(
        checked,
        INERT_MODULES.len(),
        "not every inert module was scanned — the test would pass vacuously"
    );
}

#[test]
fn the_inert_discovery_path_cannot_prompt() {
    let dir = src_dir();
    for module in INERT_MODULES {
        let source = fs::read_to_string(dir.join(module)).expect("inert module");
        let code = code_only(&source);
        for marker in PROMPT_MARKERS {
            assert!(
                !code.contains(marker),
                "{module} references `{marker}`. T-028 requires discovery to be silent; \
                 the consent dialog is T-029's, and it is shown only for executable configs."
            );
        }
    }
}

/// Guards the guard: if the inert modules are renamed or split, this test must
/// fail rather than silently scanning nothing.
#[test]
fn every_source_module_is_classified() {
    let dir = src_dir();
    let mut unclassified = Vec::new();

    for entry in fs::read_dir(&dir).expect("src dir").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".rs") {
            continue;
        }
        if !INERT_MODULES.contains(&name.as_str()) && !EXECUTABLE_MODULES.contains(&name.as_str())
        {
            unclassified.push(name);
        }
    }

    assert!(
        unclassified.is_empty(),
        "module(s) {unclassified:?} are in sluice-config but classified neither inert nor \
         executable. Add them to INERT_MODULES (parsed, never executed) or EXECUTABLE_MODULES \
         (consent-gated) in tests/no_subprocess.rs — D-026's whole safety argument is that \
         split, so a module that belongs to neither side is a gap."
    );
}
