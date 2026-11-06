//! The portable profile and saved-query file (T-036 / D-011, on T-033's store).
//!
//! D-011: local-only, single user — no accounts, no server, no sync, no
//! telemetry. Profiles and saved queries live in one plain, documented,
//! human-editable TOML file in the config dir (`docs/PROFILE-FILE.md`), so the
//! user can diff it, hand-edit it, and move it between machines with whatever
//! they already use for files.
//!
//! Two properties shape every operation here:
//!
//! - **The file is the source of truth, not this process.** Each change
//!   re-reads the file, applies one edit, and writes it back atomically, so a
//!   hand edit made while the app was open is kept rather than clobbered by a
//!   stale in-memory copy.
//! - **A newer file is never rewritten.** T-033's loader refuses it, so every
//!   mutation fails until the user updates — the alternative is an older build
//!   silently dropping fields a newer one wrote.
//!
//! The on-disk entry types are deliberately separate from the IPC type
//! `ProfileMeta`: the file format is versioned and migrated, the webview shape
//! is not, and coupling them would let a UI change silently change the format.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sluice_core::{guard_driver_call, DriverError, ErrorKind, SessionId};
use sluice_postgres::tls::TlsMode;
use sluice_state::config::{self, DocumentSpec, Migration};

use crate::commands::ProfileMeta;
use crate::state::{state_error, Stores};

pub const FILE_NAME: &str = "profiles.toml";
pub const CURRENT_VERSION: u32 = 1;
const MIGRATIONS: &[Migration] = &[];

/// Written above the document on every save. T-037 / R-019 owns the precise
/// wording of what the file does and does not contain.
pub const HEADER: &str = "\
Sluice connection profiles and saved queries.
Format: https://github.com/OWNER/sluice/blob/main/docs/PROFILE-FILE.md
Hand edits are fine; Sluice re-reads this file before every change it makes.";

pub const SPEC: DocumentSpec<'static> =
    DocumentSpec { what: "the profile file", current: CURRENT_VERSION, migrations: MIGRATIONS, header: HEADER };

/// The whole file.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    #[serde(default, rename = "profile", skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ProfileEntry>,
    #[serde(default, rename = "query", skip_serializing_if = "Vec::is_empty")]
    pub queries: Vec<SavedQuery>,
}

/// One `[[profile]]` table. `deny_unknown_fields`, because this file is
/// hand-edited and a misspelled `dbname` should be an error that names it,
/// not a silently ignored line.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProfileEntry {
    /// Stable id — also the keychain account name the password is stored
    /// under, so renaming it orphans the stored password.
    pub id: String,
    pub label: String,
    #[serde(default = "default_engine")]
    pub engine: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    #[serde(default, skip_serializing_if = "is_default_tls")]
    pub tls: TlsMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_timeout_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_budget: Option<u32>,
}

/// One `[[query]]` table.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SavedQuery {
    pub id: String,
    pub label: String,
    /// The profile it was saved against, if any; a dangling id is kept, not
    /// an error — the profile may live in another machine's copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub sql: String,
}

fn default_engine() -> String {
    "postgres".into()
}

fn is_default_tls(t: &TlsMode) -> bool {
    *t == TlsMode::Verify
}

impl ProfileEntry {
    pub fn from_meta(m: &ProfileMeta) -> Self {
        Self {
            id: m.id.clone(),
            label: m.label.clone(),
            engine: default_engine(),
            host: m.host.clone(),
            port: m.port,
            user: m.user.clone(),
            database: m.dbname.clone(),
            tls: m.tls,
            statement_timeout_secs: m.statement_timeout_secs,
            connection_budget: m.connection_budget,
        }
    }

    pub fn to_meta(&self) -> ProfileMeta {
        ProfileMeta {
            id: self.id.clone(),
            label: self.label.clone(),
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            dbname: self.database.clone(),
            tls: self.tls,
            statement_timeout_secs: self.statement_timeout_secs,
            connection_budget: self.connection_budget,
        }
    }
}

// ------------------------------------------------------------------ store

/// Serialises read-modify-write cycles within this process. Edits from
/// outside (an editor, `git pull`) are handled by re-reading, not locking.
#[derive(Default)]
pub struct WorkspaceFile {
    lock: Mutex<()>,
}

impl WorkspaceFile {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(FILE_NAME)
    }

    pub fn load(config_dir: &Path) -> Result<Workspace, DriverError> {
        Ok(config::load::<Workspace>(&Self::path(config_dir), &SPEC)
            .map_err(state_error)?
            .map(|l| l.value)
            .unwrap_or_default())
    }

    /// Re-read, edit, write back atomically.
    pub fn update<R>(
        &self,
        config_dir: &Path,
        edit: impl FnOnce(&mut Workspace) -> Result<R, DriverError>,
    ) -> Result<R, DriverError> {
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut ws = Self::load(config_dir)?;
        let out = edit(&mut ws)?;
        config::save(&Self::path(config_dir), &ws, &SPEC).map_err(state_error)?;
        Ok(out)
    }
}

fn upsert<T>(items: &mut Vec<T>, item: T, id: impl Fn(&T) -> &str) {
    let key = id(&item).to_string();
    match items.iter_mut().find(|x| id(x) == key) {
        Some(slot) => *slot = item,
        None => items.push(item),
    }
}

fn validate_id(id: &str) -> Result<(), DriverError> {
    if id.trim().is_empty() {
        return Err(DriverError::new(ErrorKind::Conflict, "an id cannot be empty"));
    }
    Ok(())
}

// -------------------------------------------------------------------- IPC

/// What the webview gets: metadata only (D-007), plus where the file is so
/// the user can open it themselves.
#[derive(Serialize, Debug)]
pub struct WorkspaceView {
    pub profiles: Vec<ProfileMeta>,
    pub queries: Vec<SavedQuery>,
    pub path: Option<String>,
}

fn sid() -> SessionId {
    SessionId("workspace".into())
}

fn view(stores: &Stores, ws: Workspace) -> WorkspaceView {
    WorkspaceView {
        profiles: ws.profiles.iter().map(ProfileEntry::to_meta).collect(),
        queries: ws.queries,
        path: stores.dirs.as_ref().map(|d| WorkspaceFile::path(&d.config).display().to_string()),
    }
}

#[tauri::command]
pub async fn load_workspace(stores: tauri::State<'_, Stores>) -> Result<WorkspaceView, DriverError> {
    guard_driver_call(&sid(), async {
        let ws = WorkspaceFile::load(&stores.dirs()?.config)?;
        Ok(view(&stores, ws))
    })
    .await
}

#[tauri::command]
pub async fn save_profile(
    stores: tauri::State<'_, Stores>,
    file: tauri::State<'_, WorkspaceFile>,
    profile: ProfileMeta,
) -> Result<WorkspaceView, DriverError> {
    guard_driver_call(&sid(), async {
        validate_id(&profile.id)?;
        let ws = file.update(&stores.dirs()?.config, |ws| {
            upsert(&mut ws.profiles, ProfileEntry::from_meta(&profile), |p| &p.id);
            Ok(ws.clone())
        })?;
        Ok(view(&stores, ws))
    })
    .await
}

/// Removes the profile **and** its stored password: a deleted profile must
/// not leave a credential behind in the keychain.
#[tauri::command]
pub async fn delete_profile(
    stores: tauri::State<'_, Stores>,
    file: tauri::State<'_, WorkspaceFile>,
    app: tauri::State<'_, crate::commands::AppState>,
    profile_id: String,
) -> Result<WorkspaceView, DriverError> {
    guard_driver_call(&sid(), async {
        let ws = file.update(&stores.dirs()?.config, |ws| {
            ws.profiles.retain(|p| p.id != profile_id);
            Ok(ws.clone())
        })?;
        app.forget_credential(&profile_id);
        Ok(view(&stores, ws))
    })
    .await
}

#[tauri::command]
pub async fn save_query(
    stores: tauri::State<'_, Stores>,
    file: tauri::State<'_, WorkspaceFile>,
    query: SavedQuery,
) -> Result<WorkspaceView, DriverError> {
    guard_driver_call(&sid(), async {
        validate_id(&query.id)?;
        let ws = file.update(&stores.dirs()?.config, |ws| {
            upsert(&mut ws.queries, query, |q| &q.id);
            Ok(ws.clone())
        })?;
        Ok(view(&stores, ws))
    })
    .await
}

#[tauri::command]
pub async fn delete_query(
    stores: tauri::State<'_, Stores>,
    file: tauri::State<'_, WorkspaceFile>,
    query_id: String,
) -> Result<WorkspaceView, DriverError> {
    guard_driver_call(&sid(), async {
        let ws = file.update(&stores.dirs()?.config, |ws| {
            ws.queries.retain(|q| q.id != query_id);
            Ok(ws.clone())
        })?;
        Ok(view(&stores, ws))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(id: &str) -> ProfileMeta {
        ProfileMeta {
            id: id.into(),
            label: "Local".into(),
            host: "localhost".into(),
            port: 5432,
            user: "postgres".into(),
            dbname: "app_dev".into(),
            tls: TlsMode::Disabled,
            statement_timeout_secs: None,
            connection_budget: Some(4),
        }
    }


    #[test]
    fn profiles_and_queries_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = WorkspaceFile::default();
        f.update(dir.path(), |ws| {
            upsert(&mut ws.profiles, ProfileEntry::from_meta(&meta("local")), |p| &p.id);
            upsert(
                &mut ws.queries,
                SavedQuery { id: "q1".into(), label: "Counts".into(), profile: Some("local".into()), sql: "select 1".into() },
                |q| &q.id,
            );
            Ok(())
        })
        .unwrap();

        let ws = WorkspaceFile::load(dir.path()).unwrap();
        assert_eq!(ws.profiles.len(), 1);
        assert_eq!(ws.profiles[0].to_meta().dbname, "app_dev");
        assert_eq!(ws.queries[0].sql, "select 1");
    }

    /// The file is meant to be read and edited by a person, so its rendering
    /// is part of the contract, not an implementation detail.
    #[test]
    fn the_file_reads_like_a_hand_written_config() {
        let ws = Workspace { profiles: vec![ProfileEntry::from_meta(&meta("local"))], queries: vec![] };
        let text = config::render(&ws, &SPEC).unwrap();
        assert!(text.starts_with("# Sluice connection profiles"), "{text}");
        assert!(text.contains("version = 1\n"), "{text}");
        assert!(text.contains("[[profile]]\nid = \"local\""), "{text}");
        assert!(text.contains("database = \"app_dev\""), "{text}");
        assert!(text.contains("tls = \"disabled\""), "{text}");
        assert!(!text.contains("[[query]]"), "empty sections are omitted: {text}");
        assert!(!text.to_lowercase().contains("password"), "{text}");
    }

    #[test]
    fn a_hand_written_minimal_profile_loads_with_defaults() {
        let text = "[[profile]]\nid = \"x\"\nlabel = \"X\"\nhost = \"db\"\nport = 5432\nuser = \"u\"\ndatabase = \"d\"\n";
        let ws = config::parse::<Workspace>(text, &SPEC).unwrap().value;
        assert_eq!(ws.profiles[0].engine, "postgres");
        assert_eq!(ws.profiles[0].tls, TlsMode::Verify, "TLS verification is the default (D-007)");
    }

    #[test]
    fn a_misspelled_field_is_an_error_that_names_it() {
        let text = "[[profile]]\nid = \"x\"\nlabel = \"X\"\nhost = \"db\"\nport = 5432\nuser = \"u\"\ndbname = \"d\"\n";
        let err = config::parse::<Workspace>(text, &SPEC).unwrap_err().to_string();
        assert!(err.contains("dbname"), "{err}");
    }

    #[test]
    fn a_hand_edit_made_while_the_app_is_open_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let f = WorkspaceFile::default();
        f.update(dir.path(), |ws| {
            ws.profiles.push(ProfileEntry::from_meta(&meta("a")));
            Ok(())
        })
        .unwrap();

        // Someone edits the file by hand, outside the app.
        let path = WorkspaceFile::path(dir.path());
        let edited = std::fs::read_to_string(&path).unwrap().replace("label = \"Local\"", "label = \"Renamed\"");
        std::fs::write(&path, edited).unwrap();

        f.update(dir.path(), |ws| {
            ws.profiles.push(ProfileEntry::from_meta(&meta("b")));
            Ok(())
        })
        .unwrap();
        let ws = WorkspaceFile::load(dir.path()).unwrap();
        assert_eq!(ws.profiles[0].label, "Renamed");
        assert_eq!(ws.profiles.len(), 2);
    }

    #[test]
    fn a_newer_file_blocks_every_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = WorkspaceFile::path(dir.path());
        std::fs::write(&path, "version = 2\n").unwrap();
        let f = WorkspaceFile::default();
        let err = f.update(dir.path(), |_| Ok(())).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "version = 2\n");
    }

    /// D-011: no accounts, no server, no sync, no telemetry — anywhere. This
    /// fails if a dependency or capability that could phone home is added.
    #[test]
    fn nothing_in_the_app_can_phone_home() {
        let manifest = include_str!("../Cargo.toml");
        let deps = manifest.split("[dependencies]").nth(1).unwrap_or("");
        let deps = deps.split("\n[").next().unwrap_or("");
        // Dependency names — the key before `=` — not comment text.
        let names: Vec<&str> = deps
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#') && l.contains('='))
            .filter_map(|l| l.split('=').next().map(str::trim))
            .collect();
        assert!(names.contains(&"tauri"), "dependency scan saw nothing: {names:?}");
        for name in &names {
            for banned in ["reqwest", "ureq", "hyper", "isahc", "surf", "sentry", "updater", "tauri-plugin-http", "analytics", "telemetry"] {
                assert!(!name.contains(banned), "D-011: `{name}` in src-tauri dependencies");
            }
        }
        let conf = include_str!("../tauri.conf.json");
        assert!(!conf.contains("updater"), "D-011/D-019: no updater in v1");
        let caps = include_str!("../capabilities/default.json");
        assert!(!caps.contains("http:"), "no http capability for the webview");

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../src");
        let mut stack = vec![src];
        let mut scanned = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "ts" || x == "tsx") {
                    scanned += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    for banned in ["fetch(", "XMLHttpRequest", "sendBeacon", "new WebSocket", "EventSource("] {
                        assert!(!text.contains(banned), "D-011: `{banned}` in {}", p.display());
                    }
                }
            }
        }
        assert!(scanned > 5, "scan saw only {scanned} files — would pass vacuously");
    }
}
