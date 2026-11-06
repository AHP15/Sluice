//! The app's persistent stores, as the core holds them (T-033 / D-014).
//!
//! `sluice-state` owns the mechanics; this module owns *where* (the OS dirs)
//! and the IPC surface. Every store degrades rather than blocks startup: no
//! resolvable home directory means nothing persists this run, said plainly,
//! rather than the app refusing to open.

use std::sync::Mutex;

use sluice_core::{guard_driver_call, DriverError, ErrorKind, SessionId, StoredStatement};
use sluice_state::history::{History, HistoryEntry, NewEntry};
use sluice_state::AppDirs;

/// UI state is opaque to the core — the webview owns its shape — but it is
/// bounded, because a runaway client must not fill the cache dir.
pub const MAX_UI_STATE_BYTES: usize = 256 * 1024;

pub struct Stores {
    pub dirs: Option<AppDirs>,
    /// Opened on first use. `Err` is remembered (as text) so a refused
    /// database — written by a newer build — is reported, not retried on every
    /// statement.
    history: Mutex<Option<Result<History, String>>>,
}

impl Default for Stores {
    fn default() -> Self {
        Self { dirs: AppDirs::from_os(), history: Mutex::new(None) }
    }
}

impl Stores {
    #[cfg(test)]
    pub fn rooted(root: &std::path::Path) -> Self {
        Self { dirs: Some(AppDirs::rooted(root)), history: Mutex::new(None) }
    }

    fn with_history<R>(&self, f: impl FnOnce(&mut History) -> Result<R, DriverError>) -> Result<R, DriverError> {
        let dirs = self.dirs()?;
        let mut slot = self.history.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(History::open(&dirs.data).map_err(|e| e.to_string()));
        }
        match slot.as_mut().expect("just set") {
            Ok(h) => f(h),
            Err(why) => Err(DriverError::new(ErrorKind::Conflict, why.clone())),
        }
    }

    /// T-035: append a statement that has **already** been redacted (T-025).
    /// Best-effort by design — history failing must not fail the query — and
    /// silent by design: the failure is not logged, because the only thing
    /// worth saying about it would be the statement (D-014).
    pub fn record_history(&self, profile_id: &str, stmt: &StoredStatement, outcome: &str, duration_ms: i64, rows: Option<i64>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let _ = self.with_history(|h| {
            h.record(&NewEntry {
                profile_id,
                executed_at: now,
                statement: &stmt.text,
                redacted: stmt.redacted,
                outcome,
                duration_ms: Some(duration_ms),
                rows,
            })
            .map_err(state_error)
        });
    }

    pub fn purge_profile_history(&self, profile_id: &str) {
        let _ = self.with_history(|h| h.purge_profile(profile_id).map_err(state_error));
    }

    pub fn dirs(&self) -> Result<&AppDirs, DriverError> {
        self.dirs
            .as_ref()
            .ok_or_else(|| DriverError::new(ErrorKind::NotFound, "no home directory: nothing can be saved this run"))
    }
}

pub fn state_error(e: sluice_state::StateError) -> DriverError {
    let kind = match &e {
        sluice_state::StateError::NewerVersion { .. } => ErrorKind::Conflict,
        sluice_state::StateError::Invalid { .. } => ErrorKind::Unknown,
        sluice_state::StateError::Io { .. } | sluice_state::StateError::Sqlite(_) => ErrorKind::Unknown,
    };
    DriverError::new(kind, e.to_string())
}

fn sid() -> SessionId {
    SessionId("state".into())
}

/// The webview's last UI state, or `null`. Never fails: the cache is
/// disposable, so an unreadable one is simply absent.
#[tauri::command]
pub async fn load_ui_state(stores: tauri::State<'_, Stores>) -> Result<serde_json::Value, DriverError> {
    guard_driver_call(&sid(), async {
        Ok(match &stores.dirs {
            Some(d) => sluice_state::cache::load::<Option<serde_json::Value>>(&d.cache).unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        })
    })
    .await
}

#[tauri::command]
pub async fn save_ui_state(stores: tauri::State<'_, Stores>, state: serde_json::Value) -> Result<(), DriverError> {
    guard_driver_call(&sid(), async {
        let size = serde_json::to_vec(&state).map(|v| v.len()).unwrap_or(usize::MAX);
        if size > MAX_UI_STATE_BYTES {
            return Err(DriverError::new(ErrorKind::Conflict, format!("UI state too large ({size} bytes)")));
        }
        sluice_state::cache::save(&stores.dirs()?.cache, &Some(state)).map_err(state_error)
    })
    .await
}

/// T-035: history, newest first, optionally for one profile and filtered by a
/// substring. Statements were redacted before they were stored (T-025);
/// `redacted` / `truncated` entries are not replayable as written.
#[tauri::command]
pub async fn list_history(
    stores: tauri::State<'_, Stores>,
    profile_id: Option<String>,
    search: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<HistoryEntry>, DriverError> {
    guard_driver_call(&sid(), async {
        let limit = limit.unwrap_or(200).min(1000);
        stores.with_history(|h| h.list(profile_id.as_deref(), search.as_deref(), limit).map_err(state_error))
    })
    .await
}

/// T-035: the one-click purge. Returns how many entries were removed.
#[tauri::command]
pub async fn clear_history(stores: tauri::State<'_, Stores>) -> Result<u64, DriverError> {
    guard_driver_call(&sid(), async { stores.with_history(|h| h.purge().map_err(state_error)) }).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_history_is_the_redacted_text_and_purges_in_one_call() {
        let dir = tempfile::tempdir().unwrap();
        let stores = Stores::rooted(dir.path());
        let stmt = sluice_core::redact_statement("ALTER ROLE app PASSWORD 'hunter2'");
        stores.record_history("p", &stmt, "ok", 3, None);
        stores.record_history("p", &sluice_core::redact_statement("select 1"), "ok", 1, Some(1));

        let entries = stores.with_history(|h| h.list(None, None, 10).map_err(state_error)).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|e| !e.statement.contains("hunter2")));
        assert_eq!(entries.iter().filter(|e| e.redacted).count(), 1);

        // The raw text is not anywhere in the database file either.
        let raw = std::fs::read(dir.path().join("data").join(sluice_state::history::FILE_NAME)).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("hunter2"));

        assert_eq!(stores.with_history(|h| h.purge().map_err(state_error)).unwrap(), 2);
    }

    #[test]
    fn a_refused_history_database_is_reported_and_never_fails_a_query() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let c = rusqlite::Connection::open(data.join(sluice_state::history::FILE_NAME)).unwrap();
        c.pragma_update(None, "user_version", 99).unwrap();
        drop(c);

        let stores = Stores::rooted(dir.path());
        // Recording is silent and harmless…
        stores.record_history("p", &sluice_core::redact_statement("select 1"), "ok", 1, None);
        // …and listing says why.
        let err = stores.with_history(|h| h.count().map_err(state_error)).unwrap_err();
        assert!(err.message.contains("newer version"), "{}", err.message);
    }
}
