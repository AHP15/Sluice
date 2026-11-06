//! Interpreter resolution (T-029 / D-026, R-001).
//!
//! D-026: *"The interpreter is resolved only from absolute system locations (a
//! user-configured path, or a system install directory) — never from
//! `node_modules/.bin`, never from a relative PATH entry, and never from
//! anything inside the scanned directory."*
//!
//! This is the second half of R-001, and the half that is easy to get wrong.
//! Consenting to run `bun` must not mean consenting to run whatever the
//! repository placed on `PATH` under that name — otherwise the consent dialog
//! names a trustworthy binary while a different one executes.
//!
//! **`PATH` is never consulted.** D-026 enumerates exactly two sources, and
//! neither is `PATH`; filtering `PATH` down to its absolute entries would still
//! honour a `PATH` the shell's own rc files (or a direnv-style tool) had already
//! pointed into the project. The cost is that a version manager installing into
//! an unusual prefix (`nvm` in particular) is not auto-detected, and the user
//! must configure the path explicitly. That is the safe direction to fail:
//! D-026's stated degradation is "choose a connection in the UI", which is a
//! working app, not a broken one.
//!
//! This module resolves *paths only* and never executes anything — which is why
//! it is classified inert in `tests/no_subprocess.rs`.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpreterKind {
    Bun,
    Node,
}

impl InterpreterKind {
    pub fn binary_name(&self) -> &'static str {
        match self {
            InterpreterKind::Bun => {
                if cfg!(windows) {
                    "bun.exe"
                } else {
                    "bun"
                }
            }
            InterpreterKind::Node => {
                if cfg!(windows) {
                    "node.exe"
                } else {
                    "node"
                }
            }
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            InterpreterKind::Bun => "bun",
            InterpreterKind::Node => "node",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInterpreter {
    pub kind: InterpreterKind,
    /// Always absolute and canonicalized — this exact path is what the consent
    /// dialog shows and what is spawned. The two must not be able to differ.
    pub path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InterpreterError {
    /// No interpreter in any allowed location. D-026 degrades to the UI.
    NotFound,
    /// A user-configured path failed validation.
    Rejected { reason: RejectReason },
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RejectReason {
    NotAbsolute,
    InsideScannedDirectory,
    InsideNodeModules,
    NotAFile,
    NotExecutable,
}

impl RejectReason {
    pub fn explain(&self) -> &'static str {
        match self {
            RejectReason::NotAbsolute => "interpreter path must be absolute",
            RejectReason::InsideScannedDirectory => {
                "interpreter must not live inside the project directory being opened"
            }
            RejectReason::InsideNodeModules => {
                "interpreter must not come from node_modules — that is repository-controlled"
            }
            RejectReason::NotAFile => "no file at that path",
            RejectReason::NotExecutable => "file is not executable",
        }
    }
}

impl std::fmt::Display for InterpreterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InterpreterError::NotFound => write!(
                f,
                "no JavaScript interpreter found in a trusted system location"
            ),
            InterpreterError::Rejected { reason } => write!(f, "{}", reason.explain()),
        }
    }
}

impl std::error::Error for InterpreterError {}

/// Absolute directories searched for an interpreter, in order.
///
/// `~/.bun/bin` and `~/.volta/bin` are included even though they are inside the
/// user's home and user-writable. The threat R-001 describes is a
/// *repository-controlled* binary winning resolution; a path under `$HOME` that
/// the user's own toolchain installer owns is not that, and excluding it would
/// mean bun users — the common case for a TS config — could never consent.
/// `/usr/local/bin` is user-writable on macOS too, so excluding home dirs would
/// buy nothing.
fn system_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/bin"));
    } else if cfg!(target_os = "linux") {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/bin"));
        dirs.push(PathBuf::from("/bin"));
        dirs.push(PathBuf::from("/snap/bin"));
    } else if cfg!(windows) {
        if let Ok(pf) = std::env::var("ProgramFiles") {
            dirs.push(PathBuf::from(pf).join("nodejs"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(PathBuf::from(&local).join("Programs").join("nodejs"));
        }
    }

    if let Some(home) = home_dir() {
        dirs.push(home.join(".bun").join("bin"));
        dirs.push(home.join(".volta").join("bin"));
    }

    dirs
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Validate a candidate interpreter path against R-001's constraints.
///
/// `scan_dir` is the directory whose config is being opened — the untrusted
/// one. Both paths are canonicalized before comparison so `..` segments and
/// symlinks cannot be used to smuggle a repository-local binary past the check.
pub fn validate(candidate: &Path, scan_dir: &Path) -> Result<PathBuf, RejectReason> {
    if !candidate.is_absolute() {
        return Err(RejectReason::NotAbsolute);
    }

    // Reject before canonicalizing as well as after: a symlink *named* into
    // node_modules is as repository-controlled as its target.
    if has_node_modules_component(candidate) {
        return Err(RejectReason::InsideNodeModules);
    }

    let real = candidate.canonicalize().map_err(|_| RejectReason::NotAFile)?;

    if !real.is_file() {
        return Err(RejectReason::NotAFile);
    }

    if has_node_modules_component(&real) {
        return Err(RejectReason::InsideNodeModules);
    }

    // A symlink in a system directory pointing into the project must not pass.
    if let Ok(real_scan) = scan_dir.canonicalize() {
        if real.starts_with(&real_scan) {
            return Err(RejectReason::InsideScannedDirectory);
        }
    }

    if !is_executable(&real) {
        return Err(RejectReason::NotExecutable);
    }

    Ok(real)
}

fn has_node_modules_component(p: &Path) -> bool {
    p.components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("node_modules"))
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// Resolve an interpreter for evaluating a config in `scan_dir`.
///
/// `configured` is the user's explicitly-set absolute path, which wins if it
/// validates. Otherwise the system directories are searched, preferring `bun`
/// because it runs TypeScript without a flag.
pub fn resolve(
    scan_dir: &Path,
    configured: Option<&Path>,
) -> Result<ResolvedInterpreter, InterpreterError> {
    if let Some(c) = configured {
        let path = validate(c, scan_dir).map_err(|reason| InterpreterError::Rejected { reason })?;
        let kind = if path
            .file_name()
            .map(|n| n.to_string_lossy().starts_with("bun"))
            .unwrap_or(false)
        {
            InterpreterKind::Bun
        } else {
            InterpreterKind::Node
        };
        return Ok(ResolvedInterpreter { kind, path });
    }

    for kind in [InterpreterKind::Bun, InterpreterKind::Node] {
        for dir in system_dirs() {
            let candidate = dir.join(kind.binary_name());
            if let Ok(path) = validate(&candidate, scan_dir) {
                return Ok(ResolvedInterpreter { kind, path });
            }
        }
    }

    Err(InterpreterError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join("sluice-interp").join(format!("{tag}-{nanos}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn make_executable(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn relative_paths_are_rejected() {
        let dir = tmp("rel");
        assert_eq!(
            validate(Path::new("bun"), &dir).unwrap_err(),
            RejectReason::NotAbsolute
        );
        assert_eq!(
            validate(Path::new("./node_modules/.bin/bun"), &dir).unwrap_err(),
            RejectReason::NotAbsolute
        );
    }

    /// **The R-001 test.** A repository's own `node_modules/.bin/bun` must never
    /// be resolvable, even by absolute path.
    #[cfg(unix)]
    #[test]
    fn node_modules_binaries_are_rejected_even_when_absolute() {
        let project = tmp("nm");
        let bin = project.join("node_modules").join(".bin");
        fs::create_dir_all(&bin).unwrap();
        let planted = bin.join("bun");
        make_executable(&planted);

        assert_eq!(
            validate(&planted, &project).unwrap_err(),
            RejectReason::InsideNodeModules
        );
    }

    /// The other half of R-001: a binary planted anywhere inside the scanned
    /// project, not just under `node_modules`.
    #[cfg(unix)]
    #[test]
    fn binaries_inside_the_scanned_directory_are_rejected() {
        let project = tmp("inside");
        let planted = project.join("bun");
        make_executable(&planted);

        assert_eq!(
            validate(&planted, &project).unwrap_err(),
            RejectReason::InsideScannedDirectory
        );
    }

    /// A symlink in a trusted-looking location pointing back into the project
    /// must not launder the binary past the check.
    #[cfg(unix)]
    #[test]
    fn a_symlink_into_the_project_is_rejected() {
        let project = tmp("symlink-target");
        let elsewhere = tmp("symlink-home");
        let real = project.join("evil");
        make_executable(&real);

        let link = elsewhere.join("bun");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            validate(&link, &project).unwrap_err(),
            RejectReason::InsideScannedDirectory
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_is_rejected() {
        let dir = tmp("noexec");
        let f = dir.join("bun");
        fs::write(&f, "not executable").unwrap();
        let project = tmp("noexec-project");
        assert_eq!(validate(&f, &project).unwrap_err(), RejectReason::NotExecutable);
    }

    #[test]
    fn a_missing_file_is_rejected() {
        let dir = tmp("missing");
        let project = tmp("missing-project");
        assert_eq!(
            validate(&dir.join("nope"), &project).unwrap_err(),
            RejectReason::NotAFile
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_valid_configured_interpreter_resolves_and_is_canonical() {
        let sysdir = tmp("sys");
        let project = tmp("sys-project");
        let bun = sysdir.join("bun");
        make_executable(&bun);

        let r = resolve(&project, Some(&bun)).unwrap();
        assert_eq!(r.kind, InterpreterKind::Bun);
        assert!(r.path.is_absolute());
        assert_eq!(r.path, bun.canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn a_configured_node_is_classified_as_node() {
        let sysdir = tmp("sysnode");
        let project = tmp("sysnode-project");
        let node = sysdir.join("node");
        make_executable(&node);

        let r = resolve(&project, Some(&node)).unwrap();
        assert_eq!(r.kind, InterpreterKind::Node);
    }

    /// A rejected configured path must surface the rejection, not silently fall
    /// through to a system search — the user asked for a specific interpreter.
    #[cfg(unix)]
    #[test]
    fn a_rejected_configured_path_does_not_fall_through_to_system_search() {
        let project = tmp("nofallthrough");
        let bin = project.join("node_modules").join(".bin");
        fs::create_dir_all(&bin).unwrap();
        let planted = bin.join("bun");
        make_executable(&planted);

        assert_eq!(
            resolve(&project, Some(&planted)).unwrap_err(),
            InterpreterError::Rejected { reason: RejectReason::InsideNodeModules }
        );
    }

    #[test]
    fn system_dirs_are_all_absolute() {
        for d in system_dirs() {
            assert!(d.is_absolute(), "{d:?} is not absolute");
            assert!(!has_node_modules_component(&d), "{d:?}");
        }
    }
}
