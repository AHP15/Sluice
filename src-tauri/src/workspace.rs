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
use sluice_state::profile::{CredentialProvider, Transport};

use crate::commands::ProfileMeta;
use crate::state::{state_error, Stores};

pub const FILE_NAME: &str = "profiles.toml";
pub const CURRENT_VERSION: u32 = 1;
const MIGRATIONS: &[Migration] = &[];

/// Written above the document on every save.
///
/// T-037 / R-019: the precise claim. "Safe to commit" was established for
/// credentials and then applied to the whole file; what the file still
/// reveals — an inventory of endpoints, valid usernames, and which
/// environments exist — is reconnaissance data, so the header says so.
pub const HEADER: &str = "\
Sluice connection profiles and saved queries.
Format: https://github.com/OWNER/sluice/blob/main/docs/PROFILE-FILE.md

This file contains NO passwords or other credentials (those are in the OS
keychain). It DOES contain hostnames, ports, usernames, database names, and
which environments exist (e.g. that a production database is reachable at a
given host). Treat it as infrastructure inventory: fine for a private repo or
your own sync folder, not for a public or widely shared one.

Hand edits are fine; Sluice re-reads this file before every change it makes.";

/// T-037 / R-019: shown the first time a profile pointing beyond this machine
/// is saved — once, not on every save, so it is read rather than dismissed.
pub const TOPOLOGY_NOTICE: &str = "Saved. Note: the profile file holds no passwords, but it does record this \
host, port, user and database. Keep it out of public or widely shared repositories.";

const NOTICE_MARKER: &str = "topology-notice-shown";

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
    /// T-038 / D-008. Omitted at the default (`direct`).
    #[serde(default, skip_serializing_if = "Transport::is_direct")]
    pub transport: Transport,
    /// T-038 / D-008: a provider, never a password. Omitted at the default
    /// (`keychain`).
    #[serde(default, skip_serializing_if = "CredentialProvider::is_keychain")]
    pub auth: CredentialProvider,
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
            transport: m.transport.clone(),
            auth: m.auth.clone(),
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
            transport: self.transport.clone(),
            auth: self.auth.clone(),
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
    /// T-037: a one-time notice for the user, when a save warrants one.
    pub notice: Option<String>,
}

/// T-037 / R-019: whether this save should carry the topology notice. True
/// at most once per installation — the marker lives in the data dir, beside
/// the other durable machine-local state.
pub fn topology_notice(data_dir: &Path, host: &str) -> Option<String> {
    if crate::guardrails::is_loopback(host) {
        return None;
    }
    let marker = data_dir.join(NOTICE_MARKER);
    if marker.exists() {
        return None;
    }
    // Best effort: failing to record it means warning again, never not at all.
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(&marker, b"R-019 topology notice shown\n");
    Some(TOPOLOGY_NOTICE.to_string())
}

fn sid() -> SessionId {
    SessionId("workspace".into())
}

fn view(stores: &Stores, ws: Workspace) -> WorkspaceView {
    WorkspaceView {
        profiles: ws.profiles.iter().map(ProfileEntry::to_meta).collect(),
        queries: ws.queries,
        path: stores.dirs.as_ref().map(|d| WorkspaceFile::path(&d.config).display().to_string()),
        notice: None,
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
        let dirs = stores.dirs()?;
        let ws = file.update(&dirs.config, |ws| {
            upsert(&mut ws.profiles, ProfileEntry::from_meta(&profile), |p| &p.id);
            Ok(ws.clone())
        })?;
        let mut v = view(&stores, ws);
        v.notice = topology_notice(&dirs.data, &profile.host);
        Ok(v)
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

/// T-034 / D-014: what an imported URL becomes. The password — inline or
/// `?password=` — is split off for the keychain; everything persisted is the
/// URL's other components. There is deliberately no URL field in the profile
/// file, so there is no string in which a password could survive.
pub struct ImportedUrl {
    pub entry: ProfileEntry,
    pub password: Option<sluice_core::Secret<String>>,
}

pub fn decompose_url(url: &sluice_core::Secret<String>, id: &str, label: &str) -> Result<ImportedUrl, DriverError> {
    let parsed = sluice_config::connection_url::parse(url.expose())
        .map_err(|e| DriverError::new(ErrorKind::Conflict, e.to_string()))?;
    if !matches!(parsed.scheme.as_str(), "postgres" | "postgresql") {
        return Err(DriverError::new(
            ErrorKind::Conflict,
            format!("`{}` URLs cannot be imported yet — this build connects to Postgres only", parsed.scheme),
        ));
    }
    let host = parsed.host.clone().unwrap_or_else(|| "localhost".into());
    let database = parsed.database.clone().unwrap_or_else(|| "postgres".into());
    let entry = ProfileEntry {
        id: id.to_string(),
        label: if label.trim().is_empty() { format!("{database}@{host}") } else { label.trim().to_string() },
        engine: default_engine(),
        host,
        port: parsed.port.unwrap_or(5432),
        user: parsed.user.clone().unwrap_or_else(|| "postgres".into()),
        database,
        tls: crate::launcher::tls_from_params(&parsed),
        statement_timeout_secs: None,
        connection_budget: None,
        transport: Transport::default(),
        auth: CredentialProvider::default(),
    };
    Ok(ImportedUrl { entry, password: parsed.password })
}

/// T-034: import a pasted connection URL as a profile.
///
/// The URL travels inward only and is deserialized straight into
/// `Secret<String>` (T-022) — it is a credential until the password is split
/// off here, in the core. The password goes to the keychain; the profile file
/// gets the rest. If either write fails, neither is left behind.
#[tauri::command]
pub async fn import_connection_url(
    stores: tauri::State<'_, Stores>,
    file: tauri::State<'_, WorkspaceFile>,
    app: tauri::State<'_, crate::commands::AppState>,
    url: sluice_core::Secret<String>,
    label: String,
) -> Result<WorkspaceView, DriverError> {
    guard_driver_call(&sid(), async {
        let id = format!("profile-{}", crate::commands::short_token());
        let imported = decompose_url(&url, &id, &label)?;
        drop(url);
        let dirs = stores.dirs()?;
        if let Some(pw) = &imported.password {
            app.store_credential_for(&id, pw)?;
        }
        let host = imported.entry.host.clone();
        let saved = file.update(&dirs.config, |ws| {
            ws.profiles.push(imported.entry);
            Ok(ws.clone())
        });
        let ws = match saved {
            Ok(ws) => ws,
            Err(e) => {
                app.forget_credential(&id);
                return Err(e);
            }
        };
        let mut v = view(&stores, ws);
        v.notice = topology_notice(&dirs.data, &host);
        Ok(v)
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
            transport: Transport::default(),
            auth: CredentialProvider::default(),
        }
    }

    /// T-038: non-default transports and providers round-trip through the
    /// file; defaults stay out of it.
    #[test]
    fn transport_and_auth_round_trip_and_defaults_are_omitted() {
        let plain = config::render(&Workspace { profiles: vec![ProfileEntry::from_meta(&meta("a"))], queries: vec![] }, &SPEC).unwrap();
        assert!(!plain.contains("transport") && !plain.contains("auth"), "{plain}");

        let mut m = meta("b");
        m.transport = Transport::SshTunnel { host: "bastion".into(), port: Some(22), user: None };
        m.auth = CredentialProvider::Env { var: "STAGING_PW".into() };
        let ws = Workspace { profiles: vec![ProfileEntry::from_meta(&m)], queries: vec![] };
        let text = config::render(&ws, &SPEC).unwrap();
        let back = config::parse::<Workspace>(&text, &SPEC).unwrap().value;
        assert_eq!(back, ws, "{text}");
        assert!(text.contains("kind = \"ssh-tunnel\""), "{text}");
        assert!(text.contains("provider = \"env\""), "{text}");
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
        // The header names passwords to say they are absent; the body must not.
        let body = &text[text.find("version = ").unwrap()..];
        assert!(!body.to_lowercase().contains("password"), "{body}");
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

    /// T-034: the password leaves the URL for the keychain, and nothing that
    /// reaches the profile file can carry it.
    #[test]
    fn an_imported_url_is_persisted_without_its_password() {
        for raw in [
            "postgres://alice:hunter2@db.example.com:5433/shop?sslmode=disable",
            "postgresql://alice@db.example.com:5433/shop?password=hunter2&sslmode=disable",
            "postgres://alice:hunt%65r2@db.example.com:5433/shop?sslmode=disable",
        ] {
            let imported = decompose_url(&sluice_core::Secret::new(raw.into()), "profile-x", "").unwrap();
            assert_eq!(imported.password.as_ref().map(|p| p.expose().as_str()), Some("hunter2"), "{raw}");
            let e = &imported.entry;
            assert_eq!((e.host.as_str(), e.port, e.user.as_str(), e.database.as_str()), ("db.example.com", 5433, "alice", "shop"));
            assert_eq!(e.tls, TlsMode::Disabled);
            assert_eq!(e.label, "shop@db.example.com");

            let text = config::render(&Workspace { profiles: vec![imported.entry], queries: vec![] }, &SPEC).unwrap();
            assert!(!text.contains("hunter2") && !text.contains("hunt%65r2"), "{text}");
        }
    }

    #[test]
    fn importing_a_non_postgres_or_broken_url_says_why_without_echoing_it() {
        let e = decompose_url(&sluice_core::Secret::new("mongodb://u:hunter2@h/db".into()), "p", "").err().unwrap();
        assert!(e.message.contains("Postgres only") && !e.message.contains("hunter2"), "{}", e.message);
        let e = decompose_url(&sluice_core::Secret::new("not a url hunter2".into()), "p", "").err().unwrap();
        assert!(!e.message.contains("hunter2"), "{}", e.message);
    }

    /// T-037 / R-019: the header states exactly what the file reveals.
    #[test]
    fn the_header_is_precise_about_what_the_file_contains() {
        let text = config::render(&Workspace::default(), &SPEC).unwrap();
        for must in ["NO passwords", "hostnames", "ports", "usernames", "database names", "which environments exist"] {
            assert!(text.contains(must), "header must mention `{must}`: {text}");
        }
        assert!(!text.to_lowercase().contains("safe to commit"), "{text}");
    }

    #[test]
    fn the_topology_notice_shows_once_and_never_for_loopback() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(topology_notice(dir.path(), "localhost"), None);
        assert_eq!(topology_notice(dir.path(), "127.0.0.1"), None);
        assert!(topology_notice(dir.path(), "db.staging.internal").is_some());
        assert_eq!(topology_notice(dir.path(), "db.prod.internal"), None, "once, not every save");
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
