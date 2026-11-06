//! The app's persistent stores, as the core holds them (T-033 / D-014).
//!
//! `sluice-state` owns the mechanics; this module owns *where* (the OS dirs)
//! and the IPC surface. Every store degrades rather than blocks startup: no
//! resolvable home directory means nothing persists this run, said plainly,
//! rather than the app refusing to open.

use sluice_core::{guard_driver_call, DriverError, ErrorKind, SessionId};
use sluice_state::AppDirs;

/// UI state is opaque to the core — the webview owns its shape — but it is
/// bounded, because a runaway client must not fill the cache dir.
pub const MAX_UI_STATE_BYTES: usize = 256 * 1024;

pub struct Stores {
    pub dirs: Option<AppDirs>,
}

impl Default for Stores {
    fn default() -> Self {
        Self { dirs: AppDirs::from_os() }
    }
}

impl Stores {
    #[cfg(test)]
    pub fn rooted(root: &std::path::Path) -> Self {
        Self { dirs: Some(AppDirs::rooted(root)) }
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

