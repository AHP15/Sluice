//! Disposable UI and window state in the cache dir (T-033 / D-014).
//!
//! The contract is the opposite of the durable stores': **deleting this file
//! must always be safe**, so reading it can never fail. A missing, corrupt,
//! or other-version file yields defaults — there is nothing here worth
//! refusing to start over, and nothing a migration needs to preserve.

use std::fs;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{write_atomic, StateError};

pub const FILE_NAME: &str = "ui-state.json";

/// Bumped when the shape changes incompatibly; a mismatch means "defaults",
/// not an error.
pub const CURRENT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Envelope<T> {
    version: u32,
    state: T,
}

/// Load the cached state, or `T::default()` for any reason it cannot be read.
pub fn load<T: DeserializeOwned + Default>(cache_dir: &Path) -> T {
    fs::read(cache_dir.join(FILE_NAME))
        .ok()
        .and_then(|b| serde_json::from_slice::<Envelope<T>>(&b).ok())
        .filter(|e| e.version == CURRENT_VERSION)
        .map(|e| e.state)
        .unwrap_or_default()
}

pub fn save<T: Serialize>(cache_dir: &Path, state: &T) -> Result<(), StateError> {
    let bytes = serde_json::to_vec_pretty(&Envelope { version: CURRENT_VERSION, state })
        .map_err(|e| StateError::Invalid { what: "UI state".into(), detail: e.to_string() })?;
    write_atomic(&cache_dir.join(FILE_NAME), &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
    struct Ui {
        theme: String,
    }

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &Ui { theme: "light".into() }).unwrap();
        assert_eq!(load::<Ui>(dir.path()).theme, "light");
    }

    #[test]
    fn missing_corrupt_or_foreign_state_is_defaults_never_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load::<Ui>(dir.path()), Ui::default());
        fs::write(dir.path().join(FILE_NAME), b"{not json").unwrap();
        assert_eq!(load::<Ui>(dir.path()), Ui::default());
        fs::write(dir.path().join(FILE_NAME), br#"{"version":99,"state":{"theme":"x"}}"#).unwrap();
        assert_eq!(load::<Ui>(dir.path()), Ui::default());
    }
}
