//! App-state stores (T-033 / D-014).
//!
//! D-014 splits app state into three stores **by role**, because their
//! requirements differ sharply:
//!
//! | store | where | format | if deleted |
//! |---|---|---|---|
//! | profiles + saved queries | config dir | versioned TOML ([`config`]) | the user's work is gone — durable |
//! | query history | data dir | SQLite with `user_version` ([`history`]) | searchable past is gone — durable |
//! | UI / window state | cache dir | disposable JSON ([`cache`]) | defaults come back — **safe to delete** |
//!
//! Both durable stores are versioned from day one, migrate forward, and
//! **refuse** a file written by a newer build ([`StateError::NewerVersion`]).
//! That refusal matters even for one user: D-011 makes the config file
//! portable, so two machines syncing it through git is the expected case, and
//! an older build silently rewriting a newer file is how profiles get lost.
//!
//! Config writes are atomic — temp file in the same directory, `fsync`, rename
//! ([`write_atomic`]) — so a crash mid-save leaves the old file, never half of
//! the new one.

pub mod cache;
pub mod config;
pub mod history;
pub mod profile;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The app's directory under each OS location: `<os dir>/sluice`.
///
/// The `sluice` leaf matches what diagnostics (`<cache>/sluice/logs`) and the
/// credential file fallback (`<data>/sluice/credentials`) already use, so the
/// app keeps one directory per role rather than two naming schemes.
pub const APP_DIR: &str = "sluice";

/// Where each store lives. Constructed from the OS via [`AppDirs::from_os`]
/// (the `directories` crate), or rooted anywhere for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppDirs {
    /// Profiles and saved queries — portable and diffable. No credentials, but
    /// it does name hosts, ports, users and databases (R-019): portable is not
    /// the same as safe to publish.
    pub config: PathBuf,
    /// Query history — durable, but machine-local by design (D-011's
    /// portability covers profiles and saved queries, not history).
    pub data: PathBuf,
    /// UI and window state — disposable.
    pub cache: PathBuf,
}

impl AppDirs {
    /// The OS locations: e.g. `~/.config/sluice`, `~/.local/share/sluice`,
    /// `~/.cache/sluice` on Linux; `~/Library/Application Support/sluice` and
    /// `~/Library/Caches/sluice` on macOS; `%APPDATA%\sluice` and
    /// `%LOCALAPPDATA%\sluice` on Windows.
    pub fn from_os() -> Option<Self> {
        let base = directories::BaseDirs::new()?;
        Some(Self {
            config: base.config_dir().join(APP_DIR),
            data: base.data_dir().join(APP_DIR),
            cache: base.cache_dir().join(APP_DIR),
        })
    }

    /// Everything under one root — for tests and portable installs.
    pub fn rooted(root: &Path) -> Self {
        Self { config: root.join("config"), data: root.join("data"), cache: root.join("cache") }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The file was written by a newer Sluice. Loading it would mean either
    /// failing on fields this build does not know or — worse — saving it back
    /// without them. Neither is acceptable, so the store is not opened.
    #[error(
        "{what} was written by a newer version of Sluice (format {found}; this build understands up to {supported}) — \
         update Sluice to open it; it has not been modified"
    )]
    NewerVersion { what: String, found: u32, supported: u32 },

    #[error("{what} is not valid: {detail}")]
    Invalid { what: String, detail: String },

    #[error("{what}: {source}")]
    Io {
        what: String,
        #[source]
        source: std::io::Error,
    },

    #[error("query history database: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl StateError {
    pub(crate) fn io(what: impl Into<String>, source: std::io::Error) -> Self {
        StateError::Io { what: what.into(), source }
    }
}

/// Write `bytes` to `path` atomically: a temp file in the same directory (so
/// the rename never crosses a filesystem), flushed to disk, then renamed over
/// the target. A crash at any point leaves either the old file or the new one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StateError> {
    let what = path.display().to_string();
    let dir = path.parent().ok_or_else(|| StateError::Invalid { what: what.clone(), detail: "no parent directory".into() })?;
    fs::create_dir_all(dir).map_err(|e| StateError::io(&what, e))?;

    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        sync_dir(dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| StateError::io(what, e))
}

/// Make the rename itself durable. Best-effort: not every platform can open a
/// directory for syncing, and failing here must not fail a write that landed.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_the_file_and_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("profiles.toml");
        write_atomic(&path, b"one").unwrap();
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "two");
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn os_dirs_put_each_store_under_the_app_dir() {
        let Some(d) = AppDirs::from_os() else { return };
        for p in [&d.config, &d.data, &d.cache] {
            assert!(p.ends_with(APP_DIR), "{p:?}");
        }
        assert_ne!(d.config, d.cache, "the cache must be separable from durable state");
    }
}
