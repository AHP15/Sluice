//! The IPC command surface.
//!
//! Two invariants hold across every command here, and both are enforced by
//! tests at the bottom of this file rather than by review:
//!
//! 1. **T-006 / D-003**: every boundary is wrapped in `catch_unwind` via
//!    `guard_driver_call`, so an in-process driver panic becomes a `driver bug`
//!    error naming the session instead of taking the window down.
//! 2. **T-020 / D-007**: no response type can carry a credential. The webview
//!    holds profile ids and metadata only; the core resolves the id to a secret
//!    at connect time and the secret never travels back out.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sluice_core::{
    guard_driver_call, BrowseRequest, Capabilities, Driver, DriverError, EditPlan, EditRequest, ErrorKind,
    ExecutionResult, NavNode, NodeId, RawRequest, Secret, SessionId, WireValue,
};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};
use tokio::sync::Mutex;

use crate::credentials::{Backend, CredentialStore};
use crate::diagnostics::logged;
use crate::guardrails::{Guardrails, Provenance};
use crate::launcher::{LaunchOrigin, LaunchOutcome, Launcher};
use crate::stream::{CreditGate, Stop, StreamEvent, Streams, BATCH_ROWS, MAX_IN_FLIGHT, STREAM_DEADLINE};
use sluice_config::connection_url::ConnectionUrl;

/// Non-secret connection metadata. This is the ONLY connection shape the
/// webview ever sees or sends.
///
/// There is deliberately no `password` field: D-007's boundary is enforced by
/// the type, not by remembering to strip it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProfileMeta {
    pub id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub dbname: String,
    #[serde(default)]
    pub tls: TlsMode,
    /// T-051 / D-010: this profile's statement timeout in seconds. Unset means
    /// the default (`execution::DEFAULT_STATEMENT_TIMEOUT`); `0` means none.
    #[serde(default)]
    pub statement_timeout_secs: Option<u32>,
    /// T-042 / D-028: this profile's connection budget — pooled and pinned
    /// together. Unset means `DEFAULT_CONNECTION_BUDGET`.
    #[serde(default)]
    pub connection_budget: Option<u32>,
}

/// What the UI learns after connecting.
#[derive(Serialize, Clone, Debug)]
pub struct SessionInfo {
    pub session_id: String,
    pub capabilities: Capabilities,
    /// D-007 ties the TLS opt-out to a *visible* warning, so the warning text
    /// travels with the session rather than being re-derived in the UI.
    pub tls_warning: Option<String>,
    /// T-027 / R-002: every session carries the full guardrail field set,
    /// whatever opened it. The authoritative copy is the core's (see
    /// `SessionRecord`); this is the UI's view of it.
    pub guardrails: Guardrails,
}

/// What the core remembers about a live session beyond its driver.
struct SessionRecord {
    guardrails: Guardrails,
    /// For a launched (unsaved) session: its URL, kept so "save as profile"
    /// can move the password into the keychain. Dropped once saved.
    ephemeral_url: Option<ConnectionUrl>,
    target: ProfileMeta,
    /// T-051: the session's statement timeout, for the client-side watchdog
    /// that backs up the server's.
    statement_timeout: Option<std::time::Duration>,
    /// T-042: the profile whose driver (and so budget) this session shares.
    profile_key: String,
}

#[derive(Default)]
pub struct AppState {
    drivers: Mutex<HashMap<String, Arc<PgDriver>>>,
    /// T-042 / D-028: one driver — one pool, one connection budget — per
    /// profile, shared by every tab connected to it. Weak, so the driver goes
    /// when its last session does.
    profiles: Mutex<HashMap<String, std::sync::Weak<PgDriver>>>,
    sessions: Mutex<HashMap<String, SessionRecord>>,
    creds: CredentialStore,
    queries: crate::execution::Queries,
}

impl AppState {
    /// T-024: redact an error with the session's own credentials. Applied to
    /// what comes out of `guard_driver_call`, so a caught driver panic's text
    /// — which the driver never saw as an error — is checked too.
    async fn redacted<T>(&self, session_id: &str, r: Result<T, DriverError>) -> Result<T, DriverError> {
        match r {
            Ok(v) => Ok(v),
            Err(e) => Err(match self.drivers.lock().await.get(session_id) {
                Some(d) => d.redactor().error(e),
                None => sluice_core::Redactor::default().error(e),
            }),
        }
    }

    async fn profile_key(&self, session_id: &str) -> Option<String> {
        self.sessions.lock().await.get(session_id).map(|r| r.profile_key.clone())
    }

    async fn timeout(&self, session_id: &str) -> Option<std::time::Duration> {
        self.sessions.lock().await.get(session_id).and_then(|r| r.statement_timeout)
    }

    async fn driver(&self, session_id: &str) -> Result<Arc<PgDriver>, DriverError> {
        self.drivers
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| DriverError::new(ErrorKind::NotFound, "no such session"))
    }
}

/// T-071 / R-016: an authentication failure is recoverable by re-entering the
/// password, so say so in the error rather than leaving a dead end. When the
/// keychain item was unreadable, the message names that as the likely cause.
fn credential_reentry(mut e: DriverError, unreadable: Option<&str>) -> DriverError {
    if e.kind == ErrorKind::AuthFailed {
        e.hints.credential_reentry = Some(true);
        if let Some(why) = unreadable {
            e.message = format!(
                "{} — the stored password could not be read from the keychain ({why}); \
                 it may have been saved by a differently-signed build. Enter it again to re-store it.",
                e.message
            );
        }
    }
    e
}

fn sid(s: &str) -> SessionId {
    SessionId(s.to_string())
}

/// Store a secret for a profile. The secret travels INWARD only; the response
/// says which backend accepted it and nothing else.
///
/// T-022/R-017: this is the IPC parse boundary — the credential is deserialized
/// from the request payload *directly into* `Secret<String>`, so it is never a
/// plain `String` in this process. `Secret` has no `Serialize` impl, which is
/// what makes putting it in a response a compile error rather than a review
/// catch.
#[tauri::command]
pub async fn store_credential(
    state: tauri::State<'_, AppState>,
    profile_id: String,
    secret: Secret<String>,
) -> Result<Backend, DriverError> {
    guard_driver_call(&sid("credentials"), async {
        state
            .creds
            .store(&profile_id, &secret)
            .map_err(|e| DriverError::new(ErrorKind::PermissionDenied, e.to_string()))
    })
    .await
}

#[tauri::command]
pub async fn connect(
    state: tauri::State<'_, AppState>,
    profile: ProfileMeta,
) -> Result<SessionInfo, DriverError> {
    guard_driver_call(&sid(&profile.id), async {
        // D-007: the id becomes a credential HERE, in the core, at connect time.
        //
        // T-071 / R-016: a keychain item that will not open is not fatal. The
        // connect is attempted without it (trust auth, `.pgpass` and service
        // files may still succeed); if the server then refuses, the error asks
        // the UI to collect the password again, which re-stores it under this
        // build's identity.
        let (password, unreadable) = match state.creds.resolve(&profile.id) {
            Ok(p) => (Some(p), None),
            Err(crate::credentials::CredentialError::Unreadable(why)) => (None, Some(why)),
            Err(_) => (None, None),
        };

        let spec = ConnectSpec {
            host: profile.host.clone(),
            port: profile.port,
            user: profile.user.clone(),
            password,
            dbname: profile.dbname.clone(),
            tls: profile.tls,
            application_name: "sluice".into(),
            statement_timeout: crate::execution::statement_timeout(profile.statement_timeout_secs),
        };
        let timeout = spec.statement_timeout;

        // T-042: a second tab on the same profile shares its driver, so the
        // profile's budget covers both. Keyed by id *and* settings, so an
        // edited profile does not inherit a pool built for the old ones.
        let profile_key = format!("{}::{}", profile.id, serde_json::to_string(&profile).unwrap_or_default());
        let existing = state.profiles.lock().await.get(&profile_key).and_then(std::sync::Weak::upgrade);
        let driver = match existing {
            Some(d) => d,
            None => {
                let budget = profile
                    .connection_budget
                    .map(|b| b.max(1) as usize)
                    .unwrap_or(sluice_postgres::DEFAULT_CONNECTION_BUDGET);
                let redactor = sluice_core::Redactor::new(spec.password.clone());
                let d = logged("connect", &profile.id, None, |_| None, PgDriver::connect_with_pool_size(spec, budget))
                    .await
                    .map_err(|e| credential_reentry(redactor.error(e), unreadable.as_deref()))?;
                state.profiles.lock().await.insert(profile_key.clone(), Arc::downgrade(&d));
                d
            }
        };
        let capabilities = driver.capabilities();

        let session_id = format!("{}::{}", profile.id, short_token());
        state.drivers.lock().await.insert(session_id.clone(), driver);

        let guardrails = Guardrails::manual(&profile.host, &profile.dbname);
        state.sessions.lock().await.insert(
            session_id.clone(),
            SessionRecord {
                guardrails: guardrails.clone(),
                ephemeral_url: None,
                target: profile.clone(),
                statement_timeout: timeout,
                profile_key,
            },
        );

        Ok(SessionInfo {
            session_id,
            capabilities,
            tls_warning: profile.tls.warning().map(str::to_string),
            guardrails,
        })
    })
    .await
}

/// T-026: drain the launcher's outbox. Pulled on mount and on every
/// `launch-pending` event, so a launch resolved before the webview subscribed
/// is not lost.
#[tauri::command]
pub async fn take_launches(
    launcher: tauri::State<'_, Launcher>,
) -> Result<Vec<LaunchOutcome>, DriverError> {
    guard_driver_call(&sid("launcher"), async { Ok(launcher.take_outcomes()) }).await
}

/// T-026: connect a launched connection by its opaque id.
///
/// D-007: the URL — and any inline password — never left the core; the UI
/// holds only `launch_id`. The pending entry is kept until a connect succeeds,
/// so a failed attempt (wrong network, TLS mismatch) can be retried.
#[tauri::command]
pub async fn connect_launch(
    state: tauri::State<'_, AppState>,
    launcher: tauri::State<'_, Launcher>,
    launch_id: String,
) -> Result<SessionInfo, DriverError> {
    guard_driver_call(&sid(&launch_id), async {
        let pending = launcher
            .pending(&launch_id)
            .ok_or_else(|| DriverError::new(ErrorKind::NotFound, "no such launch"))?;
        let t = &pending.target;
        let spec = ConnectSpec {
            host: t.host.clone(),
            port: t.port,
            user: t.user.clone(),
            password: pending.url.password.clone(),
            dbname: t.database.clone(),
            tls: t.tls,
            application_name: "sluice".into(),
            // A launched connection has no profile yet: D-010's default.
            statement_timeout: crate::execution::statement_timeout(None),
        };
        let timeout = spec.statement_timeout;

        let redactor = sluice_core::Redactor::new(spec.password.clone());
        let driver = logged("connect", &launch_id, None, |_| None, PgDriver::connect(spec))
            .await
            .map_err(|e| redactor.error(e))?;
        let capabilities = driver.capabilities();
        let session_id = format!("{launch_id}::{}", short_token());
        state.drivers.lock().await.insert(session_id.clone(), driver);
        launcher.forget(&launch_id);

        // R-002: a launched connection is not a profile, so it is materialized
        // as an ephemeral one with conservative defaults.
        let provenance = match pending.origin {
            LaunchOrigin::Discovered { .. } => Provenance::Discovered,
            LaunchOrigin::Evaluated { .. } => Provenance::Evaluated,
            LaunchOrigin::Url => Provenance::CommandLine,
        };
        let guardrails = Guardrails::ephemeral(provenance, &t.host, &t.database);
        state.sessions.lock().await.insert(
            session_id.clone(),
            SessionRecord {
                guardrails: guardrails.clone(),
                ephemeral_url: Some(pending.url.clone()),
                target: ProfileMeta {
                    id: launch_id.clone(),
                    label: format!("{}@{}", t.database, t.host),
                    host: t.host.clone(),
                    port: t.port,
                    user: t.user.clone(),
                    dbname: t.database.clone(),
                    tls: t.tls,
                    statement_timeout_secs: None,
                    connection_budget: None,
                },
                statement_timeout: timeout,
                // R-002: a launch is its own ephemeral profile, and its budget.
                profile_key: launch_id.clone(),
            },
        );

        Ok(SessionInfo {
            session_id,
            capabilities,
            tls_warning: t.tls.warning().map(str::to_string),
            guardrails,
        })
    })
    .await
}

/// T-049 / R-006: browse a relation as a credit-gated stream of row batches.
///
/// Returns the stream id at once; batches arrive on `on_event`. The core sends
/// a batch only against credit the webview returns via `grant_stream_credit`
/// after rendering one, so the webview's queue is bounded by `MAX_IN_FLIGHT`
/// batches however fast the server is. `cancel_stream` (and the deadline)
/// stop it even while it is blocked waiting for credit, and the query is then
/// cancelled on the server.
#[tauri::command]
pub async fn open_browse_stream(
    state: tauri::State<'_, AppState>,
    streams: tauri::State<'_, Streams>,
    session_id: String,
    request: BrowseRequest,
    on_event: tauri::ipc::Channel<StreamEvent>,
) -> Result<String, DriverError> {
    guard_driver_call(&sid(&session_id), async {
        let driver = state.driver(&session_id).await?;
        let stream_id = format!("stream-{}-{}", session_id, short_token());
        let gate = Arc::new(CreditGate::new(MAX_IN_FLIGHT));
        streams.insert(stream_id.clone(), gate.clone());

        let registry = streams.inner().clone();
        // T-051: the stream's deadline is the session's statement timeout
        // (plus the cancel grace), not a fixed constant.
        let deadline_after = state
            .timeout(&session_id)
            .await
            .map(|t| t + crate::execution::CANCEL_GRACE)
            .unwrap_or(STREAM_DEADLINE);
        let id = stream_id.clone();
        tauri::async_runtime::spawn(async move {
            let deadline = tokio::time::Instant::now() + deadline_after;
            let seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let stop: Arc<std::sync::Mutex<Option<Stop>>> = Arc::default();
            let emit = {
                let (gate, on_event, seq, stop) = (gate.clone(), on_event.clone(), seq.clone(), stop.clone());
                move |batch: sluice_postgres::BrowseBatch| {
                    let (gate, on_event, seq, stop) = (gate.clone(), on_event.clone(), seq.clone(), stop.clone());
                    async move {
                        match batch {
                            sluice_postgres::BrowseBatch::Columns(columns, edit) => {
                                on_event.send(StreamEvent::Columns { columns, edit }).is_ok()
                            }
                            sluice_postgres::BrowseBatch::Rows(rows) => match gate.acquire(deadline).await {
                                Ok(()) => {
                                    let n = seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                                    on_event.send(StreamEvent::Rows { seq: n, rows }).is_ok()
                                }
                                Err(s) => {
                                    *stop.lock().unwrap_or_else(|p| p.into_inner()) = Some(s);
                                    false
                                }
                            },
                        }
                    }
                }
            };
            let started = std::time::Instant::now();
            let result =
                guard_driver_call(&sid(&id), driver.browse_batches(request, BATCH_ROWS, emit)).await;
            crate::diagnostics::query_event(
                "browse_stream",
                crate::diagnostics::profile_of(&id.trim_start_matches("stream-")),
                started,
                result.as_ref().ok().map(|e| e.rows),
                result.as_ref().err().map(|e| e.kind),
                None,
            );
            let stop = *stop.lock().unwrap_or_else(|p| p.into_inner());

            let last = match (result, stop) {
                (Err(error), _) => StreamEvent::Error { error: driver.redactor().error(error) },
                (Ok(_), Some(Stop::TimedOut)) => StreamEvent::TimedOut,
                (Ok(_), Some(Stop::Cancelled)) => StreamEvent::Cancelled,
                // Stopped without a gate stop: the channel closed (tab gone).
                (Ok(end), None) if end.stopped => StreamEvent::Cancelled,
                (Ok(end), None) => StreamEvent::Done { rows: end.rows, truncated: end.truncated, page: end.page },
            };
            let _ = on_event.send(last);
            registry.remove(&id);
        });

        Ok(stream_id)
    })
    .await
}

/// T-049: the webview rendered `batches` batches; return their credit.
#[tauri::command]
pub async fn grant_stream_credit(
    streams: tauri::State<'_, Streams>,
    stream_id: String,
    batches: u32,
) -> Result<u32, DriverError> {
    guard_driver_call(&sid(&stream_id), async {
        // A finished stream has no gate; returning credit to it is harmless.
        Ok(streams.get(&stream_id).map(|g| g.grant(batches)).unwrap_or(0))
    })
    .await
}

/// T-049: stop a stream, including one blocked on credit.
#[tauri::command]
pub async fn cancel_stream(
    streams: tauri::State<'_, Streams>,
    stream_id: String,
) -> Result<(), DriverError> {
    guard_driver_call(&sid(&stream_id), async {
        if let Some(g) = streams.get(&stream_id) {
            g.cancel();
        }
        Ok(())
    })
    .await
}

/// The result of saving a session as a profile.
#[derive(Serialize, Clone, Debug)]
pub struct SavedProfile {
    pub profile: ProfileMeta,
    pub guardrails: Guardrails,
}

/// T-027: "save as profile" for a discovered or command-line session.
///
/// The inline password, if any, moves into the keychain **here, in the core**
/// (D-007) — the webview receives profile metadata only. The session then gets
/// normal profile handling (D-014), keeping its production flag. Persisting the
/// profile's metadata across restarts is T-036's; within this run the returned
/// `ProfileMeta` connects through the ordinary `connect` command.
#[tauri::command]
pub async fn save_session_as_profile(
    state: tauri::State<'_, AppState>,
    session_id: String,
    label: String,
) -> Result<SavedProfile, DriverError> {
    guard_driver_call(&sid(&session_id), async {
        let mut sessions = state.sessions.lock().await;
        let record = sessions
            .get_mut(&session_id)
            .ok_or_else(|| DriverError::new(ErrorKind::NotFound, "no such session"))?;
        let url = record.ephemeral_url.take().ok_or_else(|| {
            DriverError::new(ErrorKind::Conflict, "this session is already a profile")
        })?;

        let mut profile = record.target.clone();
        profile.id = format!("profile-{}", short_token());
        if !label.trim().is_empty() {
            profile.label = label.trim().to_string();
        }
        if let Some(secret) = &url.password {
            if let Err(e) = state.creds.store(&profile.id, secret) {
                // Put it back: a failed save must leave the session as it was.
                record.ephemeral_url = Some(url);
                return Err(DriverError::new(ErrorKind::PermissionDenied, e.to_string()));
            }
        }

        record.guardrails = record.guardrails.clone().saved();
        record.target = profile.clone();
        Ok(SavedProfile { profile, guardrails: record.guardrails.clone() })
    })
    .await
}

/// T-051 / D-010: cancel a running query by the id the UI gave it. The server
/// stops it; if it has not finished within the grace period, its connection
/// is dropped. `false` means it had already finished.
#[tauri::command]
pub async fn cancel_query(state: tauri::State<'_, AppState>, query_id: String) -> Result<bool, DriverError> {
    guard_driver_call(&sid(&query_id), async { Ok(state.queries.cancel(&query_id).await) }).await
}

/// T-053 / D-016: drop the session's navigator cache. External DDL cannot be
/// detected without polling, so this is the contract, not a fallback.
#[tauri::command]
pub async fn refresh_navigator(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<(), DriverError> {
    guard_driver_call(&sid(&session_id), async {
        state.driver(&session_id).await?.refresh_catalog();
        Ok(())
    })
    .await
}

/// Rows across every `rows` outcome of a result, for the shape log.
fn rows_in(r: &ExecutionResult) -> Option<usize> {
    Some(
        r.outcomes
            .iter()
            .map(|o| match &o.body {
                sluice_core::OutcomeBody::Rows { rows, .. } => rows.len(),
                _ => 0,
            })
            .sum(),
    )
}

/// T-023: the exact text "copy diagnostics" will copy. The UI shows all of it
/// before anything leaves the app; nothing is ever sent anywhere by the core.
#[tauri::command]
pub async fn diagnostics_preview() -> Result<String, DriverError> {
    guard_driver_call(&sid("diagnostics"), async { Ok(crate::diagnostics::preview()) }).await
}

/// T-023: the run-scoped verbose toggle. Off at every launch (D-020).
#[tauri::command]
pub async fn set_verbose_diagnostics(enabled: bool) -> Result<bool, DriverError> {
    guard_driver_call(&sid("diagnostics"), async {
        crate::diagnostics::set_verbose(enabled);
        Ok(crate::diagnostics::verbose())
    })
    .await
}

#[tauri::command]
pub async fn verbose_diagnostics() -> Result<bool, DriverError> {
    guard_driver_call(&sid("diagnostics"), async { Ok(crate::diagnostics::verbose()) }).await
}

#[tauri::command]
pub async fn disconnect(
    state: tauri::State<'_, AppState>,
    editors: tauri::State<'_, Arc<crate::editor::Editors>>,
    session_id: String,
) -> Result<(), DriverError> {
    guard_driver_call(&sid(&session_id), async {
        // T-041: the session's editors roll back and release first.
        editors.close_for_session(&session_id).await;
        state.drivers.lock().await.remove(&session_id);
        state.sessions.lock().await.remove(&session_id);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn navigator_children(
    state: tauri::State<'_, AppState>,
    session_id: String,
    parent: Option<NodeId>,
) -> Result<Vec<NavNode>, DriverError> {
    let r = guard_driver_call(&sid(&session_id), async {
        let d = state.driver(&session_id).await?;
        logged("navigator", &session_id, None, |v: &Vec<NavNode>| Some(v.len()), d.navigator_children(parent)).await
    })
    .await;
    state.redacted(&session_id, r).await
}

#[tauri::command]
pub async fn browse(
    state: tauri::State<'_, AppState>,
    session_id: String,
    request: BrowseRequest,
    query_id: Option<String>,
) -> Result<ExecutionResult, DriverError> {
    let r = guard_driver_call(&sid(&session_id), async {
        let d = state.driver(&session_id).await?;
        let limit = state.timeout(&session_id).await;
        let call = crate::execution::run(&state.queries, query_id, d, limit, |d| async move {
            d.browse(request).await
        });
        logged("browse", &session_id, None, rows_in, call).await
    })
    .await;
    state.redacted(&session_id, r).await
}

/// T-057 / R-003: the exact statements staged edits compile to, for review.
///
/// Built by the same driver code path as `apply_edits`; the returned
/// `digest` is what apply must present.
#[tauri::command]
pub async fn preview_edits(
    state: tauri::State<'_, AppState>,
    session_id: String,
    edits: Vec<EditRequest>,
) -> Result<EditPlan, DriverError> {
    let r = guard_driver_call(&sid(&session_id), async {
        let d = state.driver(&session_id).await?;
        logged("preview_edits", &session_id, None, |p: &EditPlan| Some(p.statements.len()), d.preview_edits(edits)).await
    })
    .await;
    state.redacted(&session_id, r).await
}

/// T-056 / D-027: apply the grid's staged edits, all in one transaction.
///
/// The webview sends structured edits, never SQL (D-029). The driver
/// re-derives each relation's unique-key gate before writing, so a webview
/// that offered an edit it should not have cannot make it land.
///
/// T-057: `expected_digest` is required here. The webview can only apply what
/// it previewed: the driver recompiles the edits and refuses unless they
/// produce exactly the previewed statements and parameters.
#[tauri::command]
pub async fn apply_edits(
    state: tauri::State<'_, AppState>,
    session_id: String,
    edits: Vec<EditRequest>,
    expected_digest: String,
    query_id: Option<String>,
) -> Result<ScriptRun, DriverError> {
    let r = guard_driver_call(&sid(&session_id), async {
        let d = state.driver(&session_id).await?;
        let limit = state.timeout(&session_id).await;
        let call = crate::execution::run(&state.queries, query_id, d, limit, |d| async move {
            d.apply_edits(edits, Some(expected_digest)).await
        });
        let result = logged("apply_edits", &session_id, None, |r: &ExecutionResult| Some(r.outcomes.len()), call).await?;
        let summary = result.summary();
        Ok(ScriptRun { result, summary })
    })
    .await;
    state.redacted(&session_id, r).await
}

/// T-041 / D-028: what opening an editor session returns.
#[derive(Serialize, Clone, Debug)]
pub struct EditorOpened {
    pub editor_id: String,
    pub status: sluice_postgres::pinned::SessionStatus,
}

/// T-041: pin a connection for one editor tab, for its lifetime.
///
/// Refused on an engine whose declared affinity is not `connection`: the UI
/// decides from `editorSessionPolicy`, but the core does not trust it to.
#[tauri::command]
pub async fn open_editor(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    editors: tauri::State<'_, Arc<crate::editor::Editors>>,
    session_id: String,
) -> Result<EditorOpened, DriverError> {
    let r = guard_driver_call(&sid(&session_id), async {
        let d = state.driver(&session_id).await?;
        if d.capabilities().affinity != sluice_core::SessionAffinity::Connection {
            return Err(DriverError::new(ErrorKind::Unknown, "this engine cannot pin a connection for an editor"));
        }
        // T-042 / D-028: within the profile's one budget — release an idle
        // editor (LRU) or refuse, never evict an open transaction.
        let profile_key = state.profile_key(&session_id).await.unwrap_or_else(|| session_id.clone());
        if let Some(ev) = editors.make_room(&profile_key, d.connection_budget()).await? {
            use tauri::Emitter;
            let _ = app.emit(crate::editor::EDITOR_EVENT, ev);
        }
        let pinned = d.open_pinned().await?;
        let status = pinned.status();
        let editor_id = format!("editor-{}-{}", session_id, short_token());
        editors
            .insert(
                editor_id.clone(),
                crate::editor::Editor { session_id: session_id.clone(), profile_key, pinned: Arc::new(Mutex::new(pinned)) },
            )
            .await;
        Ok(EditorOpened { editor_id, status })
    })
    .await;
    state.redacted(&session_id, r).await
}

/// T-041: what one editor run returns — the run, and the session afterwards.
#[derive(Serialize, Clone, Debug)]
pub struct EditorRun {
    pub run: ScriptRun,
    pub status: sluice_postgres::pinned::SessionStatus,
}

/// T-041: run a script on the editor's pinned session. Cancellable by
/// `query_id` and bounded by the profile timeout, like `run_script` (T-051).
#[tauri::command]
pub async fn run_in_editor(
    state: tauri::State<'_, AppState>,
    editors: tauri::State<'_, Arc<crate::editor::Editors>>,
    editor_id: String,
    script: String,
    query_id: Option<String>,
) -> Result<EditorRun, DriverError> {
    // T-042: a released editor is refused until it reconnects explicitly.
    if let Some(why) = editors.released(&editor_id).await {
        return Err(DriverError::new(ErrorKind::ConnectionFailed, why));
    }
    let session_id = editors
        .session_of(&editor_id)
        .await
        .ok_or_else(|| DriverError::new(ErrorKind::NotFound, "no such editor session"))?;
    let r = guard_driver_call(&sid(&session_id), async {
        let pinned = editors.get(&editor_id).await.ok_or_else(|| DriverError::new(ErrorKind::NotFound, "no such editor session"))?;
        let d = state.driver(&session_id).await?;
        let limit = state.timeout(&session_id).await;
        let text = script.clone();
        let p = pinned.clone();
        let call = crate::execution::run(&state.queries, query_id, d, limit, |d| async move {
            let mut s = p.lock().await;
            d.run_pinned(&mut s, &script).await
        });
        let result = logged("editor_script", &session_id, Some(&text), rows_in, call).await;
        let status = pinned.lock().await.status();
        let result = result?;
        let summary = result.summary();
        Ok(EditorRun { run: ScriptRun { result, summary }, status })
    })
    .await;
    state.redacted(&session_id, r).await
}

/// T-041: the opt-in transaction controls.
#[tauri::command]
pub async fn editor_transaction(
    state: tauri::State<'_, AppState>,
    editors: tauri::State<'_, Arc<crate::editor::Editors>>,
    editor_id: String,
    action: String,
) -> Result<sluice_postgres::pinned::SessionStatus, DriverError> {
    // T-042: a released editor is refused until it reconnects explicitly.
    if let Some(why) = editors.released(&editor_id).await {
        return Err(DriverError::new(ErrorKind::ConnectionFailed, why));
    }
    let session_id = editors
        .session_of(&editor_id)
        .await
        .ok_or_else(|| DriverError::new(ErrorKind::NotFound, "no such editor session"))?;
    let r = guard_driver_call(&sid(&session_id), async {
        let pinned = editors.get(&editor_id).await.ok_or_else(|| DriverError::new(ErrorKind::NotFound, "no such editor session"))?;
        let mut s = pinned.lock().await;
        match action.as_str() {
            "begin" => s.begin().await,
            "commit" => s.commit().await,
            "rollback" => s.rollback().await,
            other => Err(DriverError::new(ErrorKind::Unknown, format!("unknown transaction action {other}"))),
        }
    })
    .await;
    state.redacted(&session_id, r).await
}

/// T-041: the tab is closing. Rolls back anything open — never commits.
#[tauri::command]
pub async fn close_editor(
    editors: tauri::State<'_, Arc<crate::editor::Editors>>,
    editor_id: String,
) -> Result<(), DriverError> {
    guard_driver_call(&sid(&editor_id), async {
        editors.close(&editor_id).await;
        Ok(())
    })
    .await
}

/// What the editor gets back.
///
/// `summary` is computed by `ExecutionResult::summary()` in the core, not in the
/// webview. R-041 requires "statements 1-2 rolled back" to be distinguishable
/// from "statements 1-2 committed", and that logic is tested in Rust — a second
/// implementation in TypeScript would be a second thing to keep correct.
#[derive(Serialize, Clone, Debug)]
pub struct ScriptRun {
    pub result: ExecutionResult,
    pub summary: String,
}

#[tauri::command]
pub async fn run_script(
    state: tauri::State<'_, AppState>,
    session_id: String,
    script: String,
    query_id: Option<String>,
) -> Result<ScriptRun, DriverError> {
    let r = guard_driver_call(&sid(&session_id), async {
        let d = state.driver(&session_id).await?;
        let limit = state.timeout(&session_id).await;
        // The script text is passed for verbose diagnostics only; `logged`
        // drops it unless the user switched that on for this run (D-020).
        let text = script.clone();
        // T-051: cancellable by `query_id`, bounded by the profile timeout.
        let call = crate::execution::run(&state.queries, query_id, d, limit, |d| async move {
            d.raw(RawRequest::Script(script)).await
        });
        let result = logged("script", &session_id, Some(&text), rows_in, call).await?;
        let summary = result.summary();
        Ok(ScriptRun { result, summary })
    })
    .await;
    state.redacted(&session_id, r).await
}

/// T-018: pull one oversized value on demand.
#[tauri::command]
pub async fn fetch_value(
    state: tauri::State<'_, AppState>,
    session_id: String,
    handle: String,
) -> Result<WireValue, DriverError> {
    let r = guard_driver_call(&sid(&session_id), async {
        let d = state.driver(&session_id).await?;
        logged("fetch_value", &session_id, None, |_| Some(1), d.fetch_value(&handle)).await
    })
    .await;
    state.redacted(&session_id, r).await
}

/// T-018: stream an oversized value to a file without it passing through the
/// webview.
///
/// The destination is derived, not supplied: accepting a path from the webview
/// would hand hostile database content a write primitive, and picking one
/// interactively would need the `dialog` plugin that T-021 leaves out of the
/// capability set. Files land in the user's downloads directory under a
/// sanitized name, and the resolved path is returned so the UI can show it.
#[tauri::command]
pub async fn save_value_to_file(
    state: tauri::State<'_, AppState>,
    session_id: String,
    handle: String,
    suggested_name: String,
) -> Result<SavedFile, DriverError> {
    guard_driver_call(&sid(&session_id), async {
        let driver = state.driver(&session_id).await?;

        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .ok_or_else(|| DriverError::new(ErrorKind::Unknown, "no downloads directory"))?;

        let safe: String = suggested_name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
            .take(80)
            .collect();
        let name = if safe.is_empty() { "sluice-value".to_string() } else { safe };
        let dest = dir.join(format!("{name}-{}", short_token()));

        let bytes = driver.stream_value_to_file(&handle, &dest).await?;
        Ok(SavedFile { path: dest.to_string_lossy().to_string(), bytes })
    })
    .await
}

#[derive(Serialize, Clone, Debug)]
pub struct SavedFile {
    pub path: String,
    pub bytes: u64,
}

#[tauri::command]
pub async fn core_version() -> Result<String, DriverError> {
    guard_driver_call(&sid("core"), async {
        Ok(format!("sluice-core {}", env!("CARGO_PKG_VERSION")))
    })
    .await
}

/// Deliberate fault injection, debug builds only (T-006).
#[cfg(debug_assertions)]
#[tauri::command]
pub async fn debug_panicking_driver_call() -> Result<String, DriverError> {
    guard_driver_call(&sid("debug-fault-injection"), async {
        panic!("simulated driver panic (todo!() in a decode path)");
    })
    .await
}

fn short_token() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{n:08x}")
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    /// This file's own source, read at compile time.
    const SRC: &str = include_str!("commands.rs");

    /// Only the real command code — everything above this test module. Without
    /// this the scan matches the marker inside its own string literals and
    /// reports a phantom unguarded command.
    fn command_source() -> &'static str {
        match SRC.find("#[cfg(test)]") {
            Some(i) => &SRC[..i],
            None => SRC,
        }
    }

    /// T-071 / R-016: an auth failure asks for re-entry instead of dead-ending,
    /// and names the unreadable keychain item when that was the cause.
    #[test]
    fn auth_failures_ask_for_credential_reentry() {
        let e = credential_reentry(DriverError::new(ErrorKind::AuthFailed, "password authentication failed"), None);
        assert_eq!(e.hints.credential_reentry, Some(true));

        let e = credential_reentry(
            DriverError::new(ErrorKind::AuthFailed, "password authentication failed"),
            Some("keychain platform error"),
        );
        assert!(e.message.contains("could not be read from the keychain"), "{}", e.message);

        let other = credential_reentry(DriverError::new(ErrorKind::ConnectionFailed, "refused"), Some("x"));
        assert_eq!(other.hints.credential_reentry, None);
        assert_eq!(other.message, "refused");
    }

    /// T-006: the acceptance says *every* IPC command boundary is wrapped.
    /// Reviewing for that decays; checking it does not.
    #[test]
    fn every_tauri_command_is_panic_guarded() {
        const MARKER: &str = "#[tauri::command]";
        // Every module that defines IPC commands, not just this one.
        let mut src = command_source().to_string();
        for (name, other) in [("state.rs", include_str!("state.rs"))] {
            // Cut at the test *module*, not the first `#[cfg(test)]` — a
            // test-only helper above the commands must not hide them.
            let other = &other[..other.find("#[cfg(test)]\nmod tests").unwrap_or(other.len())];
            assert!(other.contains(MARKER), "{name} commands are not being scanned");
            src.push('\n');
            src.push_str(other);
        }
        let segments: Vec<&str> = src.split(MARKER).skip(1).collect();
        assert!(
            !segments.is_empty(),
            "no #[tauri::command] found — this test would pass vacuously"
        );

        for segment in &segments {
            let name = segment
                .lines()
                .find(|l| l.contains("pub async fn") || l.contains("pub fn"))
                .unwrap_or("<unknown>")
                .trim();
            assert!(
                segment.contains("guard_driver_call"),
                "IPC command is not wrapped in catch_unwind (T-006): {name}"
            );
        }
    }

    #[test]
    fn scan_actually_sees_the_real_commands() {
        let src = command_source();
        assert!(src.matches("#[tauri::command]").count() >= 6);
        assert!(src.contains("pub async fn browse"));
    }

    /// The scan must be able to FAIL, not just pass.
    #[test]
    fn scan_rejects_an_unguarded_command() {
        let bad = "#[tauri::command]\npub async fn leaky() -> Result<(), ()> { Ok(()) }\n";
        let unguarded = bad
            .split("#[tauri::command]")
            .skip(1)
            .any(|seg| !seg.contains("guard_driver_call"));
        assert!(unguarded, "the guard predicate must reject an unwrapped command");
    }

    /// D-007 / T-020: the webview must never receive a credential.
    ///
    /// Enforced structurally: `ProfileMeta` is the only connection shape that
    /// crosses IPC, and it has no secret field. Serializing one with every
    /// field populated must produce no password-shaped key.
    #[test]
    fn no_ipc_type_can_carry_a_credential() {
        let p = ProfileMeta {
            id: "prod".into(),
            label: "Production".into(),
            host: "db.internal".into(),
            port: 5432,
            user: "app".into(),
            dbname: "main".into(),
            tls: TlsMode::Verify,
            statement_timeout_secs: None,
            connection_budget: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        for banned in ["password", "secret", "credential", "passwd", "pwd"] {
            assert!(!json.contains(banned), "ProfileMeta leaks `{banned}`: {json}");
        }
    }

    /// T-027: "save as profile" moves a password into the keychain; its
    /// response must carry only metadata and guardrails.
    #[test]
    fn saving_a_session_as_a_profile_returns_no_credential() {
        let saved = SavedProfile {
            profile: ProfileMeta {
                id: "profile-1".into(),
                label: "Ops".into(),
                host: "db.prod.internal".into(),
                port: 5432,
                user: "app".into(),
                dbname: "shop".into(),
                tls: TlsMode::Verify,
                statement_timeout_secs: None,
                connection_budget: None,
            },
            guardrails: Guardrails::ephemeral(Provenance::Discovered, "db.prod.internal", "shop").saved(),
        };
        let json = serde_json::to_string(&saved).unwrap();
        for banned in ["password", "secret", "credential", "passwd", "pwd"] {
            assert!(!json.contains(banned), "SavedProfile leaks `{banned}`: {json}");
        }
        assert!(json.contains("\"armed\":true"), "{json}");
    }

    /// `store_credential` is the one command that accepts a secret. It must not
    /// hand one back — its success type carries only which backend was used.
    #[test]
    fn the_only_secret_accepting_command_returns_no_secret() {
        let json = serde_json::to_string(&Backend::EncryptedFile).unwrap();
        assert_eq!(json, "\"encrypted_file\"");
        assert!(!json.contains("hunter2"));
    }

    /// A TLS opt-out must arrive at the UI already carrying its warning, so the
    /// unsafe state cannot be displayed without one (D-007).
    #[test]
    fn tls_opt_outs_travel_with_their_warning() {
        for mode in [TlsMode::AcceptInvalidCerts, TlsMode::Disabled] {
            assert!(mode.warning().is_some(), "{mode:?}");
        }
        assert!(TlsMode::Verify.warning().is_none());
    }
}
