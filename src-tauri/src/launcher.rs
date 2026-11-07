//! The context-aware launcher (T-026 / D-012).
//!
//! `cd project && sluice` is D-012's headline ergonomic: the app opens and
//! connects to whatever the working directory describes. This module turns an
//! invocation — the first launch, or a second one forwarded by the
//! single-instance plugin — into a [`LaunchOutcome`] the UI opens a tab for.
//!
//! ## Where the password goes
//!
//! A discovered or command-line URL can carry an inline password. D-007 forbids
//! it reaching the webview, so a resolved connection is parked here as a
//! [`PendingLaunch`] under an opaque id, and the UI receives only that id plus a
//! credential-free [`LaunchTarget`]. `connect_launch` (in `commands.rs`) turns
//! the id back into a connection, in the core.
//!
//! ## Delivery
//!
//! Outcomes go into an outbox the webview *pulls* (`take_launches`) — on mount,
//! and whenever the `launch-pending` event fires. Pull rather than push because
//! the first launch resolves before the webview has necessarily subscribed to
//! anything; an emitted event alone could be lost in that window.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use sluice_config::connection_url::ConnectionUrl;
use sluice_config::consent::ConsentStore;
use sluice_config::evaluate::{
    evaluate_with_consent, probe_sandbox, EnvPolicy, EvalOutcome, TargetConfirmation,
    DEFAULT_TIMEOUT,
};
use sluice_config::launch::{parse_args, LaunchRequest};
use sluice_config::{discover, interpreter};
use sluice_core::TlsMode;
use sluice_state::profile::Engine;
use tauri::{Emitter, Manager};

use crate::consent_dialog::{blast_radius_text, NativeConsentPrompt, NativeTargetConfirmation};

/// Event telling the webview the outbox has something in it.
pub const LAUNCH_PENDING_EVENT: &str = "launch-pending";

/// Where a launched connection came from. Carried to the UI because the
/// guardrails depend on provenance (T-027 / R-002).
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchOrigin {
    /// Found by inert discovery: an env var, `.env`, or a JSON/TOML config.
    Discovered { dir: String, source: String },
    /// Produced by running a TS/JS config under consent (D-026).
    Evaluated { dir: String, config: String },
    /// Passed on the command line.
    Url,
}

/// The credential-free description of a launched connection.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct LaunchTarget {
    /// T-075: the engine the URL's scheme names.
    pub engine: Engine,
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    pub tls: TlsMode,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaunchOutcome {
    /// Open a tab and connect to `launch_id`.
    Connect { launch_id: String, origin: LaunchOrigin, target: LaunchTarget },
    /// Nothing to connect to automatically: open a tab on the connect form.
    /// D-026's degradation for a declined or interpreter-less config.
    Choose { reason: String },
    /// The invocation itself was wrong, or a config failed loudly (R-032).
    Error { message: String },
}

/// A resolved connection waiting for the UI to ask for it.
#[derive(Clone, Debug)]
pub struct PendingLaunch {
    pub url: ConnectionUrl,
    pub target: LaunchTarget,
    pub origin: LaunchOrigin,
}

#[derive(Default)]
pub struct Launcher {
    pending: Mutex<HashMap<String, PendingLaunch>>,
    outbox: Mutex<Vec<LaunchOutcome>>,
}

impl Launcher {
    pub fn take_outcomes(&self) -> Vec<LaunchOutcome> {
        std::mem::take(&mut *self.outbox.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub fn pending(&self, launch_id: &str) -> Option<PendingLaunch> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).get(launch_id).cloned()
    }

    pub fn forget(&self, launch_id: &str) {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).remove(launch_id);
    }

    fn park(&self, url: ConnectionUrl, origin: LaunchOrigin) -> LaunchOutcome {
        let target = match target_for(&url) {
            Ok(t) => t,
            Err(message) => return LaunchOutcome::Error { message },
        };
        let launch_id = new_launch_id();
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).insert(
            launch_id.clone(),
            PendingLaunch { url, target: target.clone(), origin: origin.clone() },
        );
        LaunchOutcome::Connect { launch_id, origin, target }
    }

    fn post(&self, outcome: LaunchOutcome) {
        self.outbox.lock().unwrap_or_else(|p| p.into_inner()).push(outcome);
    }
}

/// Resolves `--profile <name>`. Saved profiles arrive with T-036; until then
/// the grammar is parsed and this reports that it cannot be resolved, rather
/// than the flag being silently unknown.
pub trait ProfileLookup {
    fn find(&self, name: &str) -> Result<ConnectionUrl, String>;
}

pub struct NoSavedProfiles;

impl ProfileLookup for NoSavedProfiles {
    fn find(&self, name: &str) -> Result<ConnectionUrl, String> {
        Err(format!(
            "Saved profiles are not available in this build yet, so `--profile {name}` could \
             not be opened. Pass a connection URL, or run sluice in the project directory."
        ))
    }
}

/// The first launch: this process's own arguments and working directory.
pub fn on_first_launch(app: tauri::AppHandle) {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    spawn_resolve(app, args, cwd);
}

/// A second invocation, forwarded by the single-instance plugin with *its*
/// argv and working directory. D-012: open a tab in this window and focus it.
pub fn on_second_instance(app: &tauri::AppHandle, argv: Vec<String>, cwd: String) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    let args: Vec<OsString> = argv.into_iter().skip(1).map(OsString::from).collect();
    spawn_resolve(app.clone(), args, PathBuf::from(cwd));
}

/// Resolution can block on native dialogs (consent, target confirmation), so
/// it never runs on the main thread.
fn spawn_resolve(app: tauri::AppHandle, args: Vec<OsString>, cwd: PathBuf) {
    std::thread::spawn(move || {
        let outcome = resolve(&app, &args, &cwd);
        if let Some(launcher) = app.try_state::<Launcher>() {
            launcher.post(outcome);
        }
        let _ = app.emit(LAUNCH_PENDING_EVENT, ());
    });
}

fn resolve(app: &tauri::AppHandle, args: &[OsString], cwd: &Path) -> LaunchOutcome {
    let launcher = app.state::<Launcher>();
    let request = match parse_args(args, cwd) {
        Ok(r) => r,
        Err(e) => return LaunchOutcome::Error { message: e.to_string() },
    };

    match request {
        LaunchRequest::Url(url) => launcher.park(url, LaunchOrigin::Url),
        LaunchRequest::Profile { name } => match NoSavedProfiles.find(&name) {
            Ok(url) => launcher.park(url, LaunchOrigin::Url),
            Err(message) => LaunchOutcome::Error { message },
        },
        LaunchRequest::Discover { dir } => discover_in(app, &launcher, &dir),
    }
}

fn discover_in(app: &tauri::AppHandle, launcher: &Launcher, dir: &Path) -> LaunchOutcome {
    let found = discover(dir);
    let dir_s = dir.to_string_lossy().into_owned();

    // Inert sources first: parsed, never executed, so no prompt (D-026).
    if let Some(best) = found.best() {
        return launcher.park(
            best.url.clone(),
            LaunchOrigin::Discovered { dir: dir_s, source: best.source.describe() },
        );
    }

    let Some(config) = found.executable_configs.first() else {
        return LaunchOutcome::Choose {
            reason: format!("No connection details were found in {dir_s}."),
        };
    };

    // An executable config: the D-026 consent path.
    let interp = match interpreter::resolve(dir, None) {
        Ok(i) => i,
        Err(_) => {
            return LaunchOutcome::Choose {
                reason: sluice_config::evaluate::DeclineReason::NoInterpreterAvailable
                    .explain()
                    .to_string(),
            }
        }
    };
    let Some(data_dir) = dirs::data_dir() else {
        return LaunchOutcome::Error { message: "no application data directory".into() };
    };
    let mut store = ConsentStore::open_at(ConsentStore::default_path(&data_dir));

    // T-032 / R-033: one answer drives both what the dialog claims and what
    // the evaluator does. The app's data dir — credential fallback key and
    // consent store — is hidden from the config whenever the sandbox runs.
    let sandbox = probe_sandbox(vec![data_dir.join("sluice")]);
    let file_fallback = crate::credentials::CredentialStore::default().file_fallback_present();
    let blast = blast_radius_text(sandbox.is_active(), file_fallback);
    let policy = EnvPolicy { sandbox, ..EnvPolicy::default() };

    match evaluate_with_consent(
        config,
        &interp,
        &mut store,
        &NativeConsentPrompt::new(app.clone()),
        blast,
        DEFAULT_TIMEOUT,
        &policy,
    ) {
        Ok(EvalOutcome::Evaluated { target, .. }) => {
            // R-032: the resolved destination is shown before any connection.
            let decision = NativeTargetConfirmation::new(app.clone()).confirm_target(&target.summary());
            match target.confirm(decision) {
                Some(url) => launcher.park(
                    url,
                    LaunchOrigin::Evaluated {
                        dir: dir_s,
                        config: config.to_string_lossy().into_owned(),
                    },
                ),
                None => LaunchOutcome::Choose { reason: "Connection cancelled.".into() },
            }
        }
        Ok(EvalOutcome::Declined { reason }) => LaunchOutcome::Choose { reason: reason.explain().into() },
        // R-032: a config that fails is an error the user sees, never a quiet
        // fallback to some other connection.
        // T-024: the config's stderr can print its own connection string.
        Err(e) => LaunchOutcome::Error {
            message: sluice_core::redact_shapes(&format!("The project's config failed: {e}")),
        },
    }
}

/// T-075: the user an engine's own clients assume when a URL names none.
pub fn default_user(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "postgres",
        Engine::Sqlserver => "sa",
        // MongoDB connects unauthenticated when no user is given.
        Engine::Mongodb => "",
    }
}

/// T-075: the database an engine's own clients open when a URL names none.
pub fn default_database(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgres => "postgres",
        Engine::Sqlserver => "master",
        Engine::Mongodb => "admin",
    }
}

/// Each engine's client defaults, applied in the core so the UI is told the
/// real target (libpq's for Postgres).
///
/// T-075: a scheme naming any engine Sluice has maps to that engine; whether
/// this build can *connect* to it is the engine factory's call at connect
/// time, which names the engine if it cannot. Only a scheme with no engine at
/// all (`mysql`, `sqlite`) is refused here.
pub fn target_for(url: &ConnectionUrl) -> Result<LaunchTarget, String> {
    let engine = Engine::from_scheme(&url.scheme).ok_or_else(|| {
        format!(
            "`{}` connections are not supported; Sluice has engines for Postgres, SQL Server and MongoDB.",
            url.scheme
        )
    })?;
    let user = url
        .user
        .clone()
        .or_else(|| url.params.get("user").cloned())
        .or_else(|| engine.is_postgres().then(|| std::env::var("USER").ok()).flatten())
        .unwrap_or_else(|| default_user(engine).into());
    let database = url
        .database
        .clone()
        .or_else(|| url.params.get(if engine == Engine::Sqlserver { "database" } else { "dbname" }).cloned())
        // libpq defaults the database to the user's name.
        .unwrap_or_else(|| if engine.is_postgres() { user.clone() } else { default_database(engine).into() });
    Ok(LaunchTarget {
        engine,
        scheme: url.scheme.clone(),
        host: url
            .host
            .clone()
            .or_else(|| url.params.get("host").cloned())
            .unwrap_or_else(|| "localhost".into()),
        port: url.port.unwrap_or(engine.default_port()),
        database,
        user,
        tls: tls_from_params(url),
    })
}

/// Map libpq's `sslmode` onto D-007's three modes, never weakening the default.
///
/// - absent, `prefer`, `allow`, `verify-ca`, `verify-full` → `Verify`. libpq's
///   `prefer` silently falls back to plaintext; D-007/T-021 make verification
///   the default instead, so a URL that does not ask for less does not get less.
/// - `require` → `AcceptInvalidCerts`: libpq's `require` encrypts without
///   verifying, which is exactly this mode — and it carries its warning.
/// - `disable` → `Disabled`, likewise with its warning.
///
/// T-075: the other engines' URL spellings map the same way — an explicit
/// "no encryption" to `Disabled`, an explicit "trust any certificate" to
/// `AcceptInvalidCerts`, and everything else (including absence) to `Verify`.
pub fn tls_from_params(url: &ConnectionUrl) -> TlsMode {
    let param = |k: &str| url.params.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.to_ascii_lowercase());
    let is = |k: &str, v: &str| param(k).as_deref() == Some(v);
    match Engine::from_scheme(&url.scheme) {
        Some(Engine::Sqlserver) => {
            if is("encrypt", "false") || is("encrypt", "no") {
                TlsMode::Disabled
            } else if is("trustservercertificate", "true") || is("trustservercertificate", "yes") {
                TlsMode::AcceptInvalidCerts
            } else {
                TlsMode::Verify
            }
        }
        Some(Engine::Mongodb) => {
            if is("tls", "false") || is("ssl", "false") {
                TlsMode::Disabled
            } else if is("tlsallowinvalidcertificates", "true") || is("tlsinsecure", "true") {
                TlsMode::AcceptInvalidCerts
            } else {
                TlsMode::Verify
            }
        }
        _ => match param("sslmode").as_deref() {
            Some("disable") => TlsMode::Disabled,
            Some("require") => TlsMode::AcceptInvalidCerts,
            _ => TlsMode::Verify,
        },
    }
}

fn new_launch_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("launch-{n}-{nanos:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sluice_config::connection_url::parse;

    #[test]
    fn libpq_defaults_fill_the_target() {
        let t = target_for(&parse("postgres://db.internal/shop").unwrap()).unwrap();
        assert_eq!(t.host, "db.internal");
        assert_eq!(t.port, 5432);
        assert_eq!(t.database, "shop");

        let t = target_for(&parse("postgresql://app@h").unwrap()).unwrap();
        assert_eq!(t.database, "app", "libpq: dbname defaults to the user name");
    }

    #[test]
    fn sslmode_never_weakens_the_verify_default() {
        let mode = |q: &str| tls_from_params(&parse(&format!("postgres://h/db{q}")).unwrap());
        assert_eq!(mode(""), TlsMode::Verify);
        assert_eq!(mode("?sslmode=prefer"), TlsMode::Verify);
        assert_eq!(mode("?sslmode=allow"), TlsMode::Verify);
        assert_eq!(mode("?sslmode=verify-full"), TlsMode::Verify);
        assert_eq!(mode("?sslmode=require"), TlsMode::AcceptInvalidCerts);
        assert_eq!(mode("?sslmode=disable"), TlsMode::Disabled);
    }

    /// T-075: every scheme with a Sluice engine maps to it, with that
    /// engine's port and database defaults; one with no engine is refused by
    /// name.
    #[test]
    fn recognised_schemes_map_to_their_engine_and_others_are_refused_by_name() {
        let t = target_for(&parse("postgres://h/db").unwrap()).unwrap();
        assert_eq!(t.engine, Engine::Postgres);

        let t = target_for(&parse("sqlserver://sa@sql.internal").unwrap()).unwrap();
        assert_eq!((t.engine, t.port, t.database.as_str()), (Engine::Sqlserver, 1433, "master"));
        let t = target_for(&parse("mssql://h:14333/app").unwrap()).unwrap();
        assert_eq!((t.engine, t.port, t.database.as_str()), (Engine::Sqlserver, 14333, "app"));

        let t = target_for(&parse("mongodb://h/db").unwrap()).unwrap();
        assert_eq!((t.engine, t.port), (Engine::Mongodb, 27017));
        let t = target_for(&parse("mongodb+srv://cluster.example.net/").unwrap()).unwrap();
        assert_eq!((t.engine, t.database.as_str()), (Engine::Mongodb, "admin"));

        let err = target_for(&parse("mysql://h/db").unwrap()).unwrap_err();
        assert!(err.contains("mysql"), "{err}");
    }

    #[test]
    fn other_engines_tls_params_never_weaken_the_verify_default() {
        let mode = |u: &str| tls_from_params(&parse(u).unwrap());
        assert_eq!(mode("sqlserver://h/db"), TlsMode::Verify);
        assert_eq!(mode("sqlserver://h/db?encrypt=true"), TlsMode::Verify);
        assert_eq!(mode("sqlserver://h/db?TrustServerCertificate=true"), TlsMode::AcceptInvalidCerts);
        assert_eq!(mode("sqlserver://h/db?encrypt=false"), TlsMode::Disabled);
        assert_eq!(mode("mongodb://h/db"), TlsMode::Verify);
        assert_eq!(mode("mongodb://h/db?tlsAllowInvalidCertificates=true"), TlsMode::AcceptInvalidCerts);
        assert_eq!(mode("mongodb://h/db?tls=false"), TlsMode::Disabled);
        // A Postgres-only parameter on another engine's URL means nothing.
        assert_eq!(mode("sqlserver://h/db?sslmode=disable"), TlsMode::Verify);
    }

    /// D-007: what the UI receives about a launch carries no credential.
    #[test]
    fn a_launch_outcome_never_carries_the_password() {
        let launcher = Launcher::default();
        let url = parse("postgres://app:hunter2@db.internal:6432/shop").unwrap();
        let outcome = launcher.park(url, LaunchOrigin::Url);
        let json = serde_json::to_string(&outcome).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
        for banned in ["password", "secret"] {
            assert!(!json.contains(banned), "{json}");
        }
        // The core still has it, keyed by the id the UI holds.
        let LaunchOutcome::Connect { launch_id, .. } = outcome else { panic!() };
        let pending = launcher.pending(&launch_id).unwrap();
        assert_eq!(pending.url.password.as_ref().unwrap().expose(), "hunter2");
    }

    #[test]
    fn profiles_report_that_they_are_not_available_yet() {
        let err = NoSavedProfiles.find("prod").unwrap_err();
        assert!(err.contains("--profile prod"), "{err}");
    }

    #[test]
    fn the_outbox_drains_once() {
        let l = Launcher::default();
        l.post(LaunchOutcome::Choose { reason: "x".into() });
        assert_eq!(l.take_outcomes().len(), 1);
        assert!(l.take_outcomes().is_empty());
    }
}
