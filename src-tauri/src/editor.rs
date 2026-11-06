//! Pinned editor sessions, as the webview sees them (T-041 / D-028).
//!
//! One editor tab, one pinned server session, keyed by an `editor_id` the core
//! mints. The core owns three behaviours the webview cannot be trusted to
//! perform, because each must happen even when the webview is gone or idle:
//!
//! - **Idle rollback.** A reaper rolls back an open transaction idle past
//!   `IDLE_TRANSACTION_TIMEOUT` — rollback, never commit — and tells the tab.
//! - **Close rolls back.** Closing the tab, disconnecting, or closing the
//!   window rolls back whatever is open and releases the connection.
//! - **Loss is final.** A lost session refuses every later run; the tab must
//!   open a new one (explicit reconnect), never get one silently.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use sluice_core::DriverError;
use sluice_postgres::pinned::{PinnedSession, SessionStatus, TxState, IDLE_TRANSACTION_TIMEOUT};
use tauri::Emitter;
use tokio::sync::Mutex;

/// How often the reaper looks for idle open transactions.
pub const REAP_EVERY: Duration = Duration::from_secs(20);

/// Event the webview listens on for core-initiated session changes.
pub const EDITOR_EVENT: &str = "editor-session";

pub struct Editor {
    pub session_id: String,
    /// T-042: the profile whose connection budget this session draws on.
    pub profile_key: String,
    pub pinned: Arc<Mutex<PinnedSession>>,
}

#[derive(Default)]
pub struct Editors {
    live: Mutex<HashMap<String, Editor>>,
    /// T-042: editors whose session was released to make room, and why. Their
    /// tab is refused until it explicitly reconnects (a new editor) — a
    /// release is never followed by a silent reconnect.
    released: Mutex<HashMap<String, String>>,
}

#[derive(Serialize, Clone, Debug)]
pub struct EditorEvent {
    pub editor_id: String,
    /// `idle_rollback`: the core rolled back an idle open transaction.
    /// `released`: the session was released to make room (T-042).
    pub kind: &'static str,
    pub message: String,
    pub status: SessionStatus,
}

impl Editors {
    pub async fn insert(&self, id: String, e: Editor) {
        self.live.lock().await.insert(id, e);
    }

    pub async fn get(&self, id: &str) -> Option<Arc<Mutex<PinnedSession>>> {
        self.live.lock().await.get(id).map(|e| e.pinned.clone())
    }

    /// Why an editor's session was released, if it was (T-042).
    pub async fn released(&self, id: &str) -> Option<String> {
        self.released.lock().await.get(id).cloned()
    }

    /// T-042 / D-028: make room in `profile_key`'s budget for one more pinned
    /// session.
    ///
    /// Under the budget: nothing to do. At it: release the least-recently-used
    /// pinned session that is **provably stateless** (T-043 / R-037) — idle,
    /// not running, and with the server reporting no session settings, temp
    /// tables, prepared statements, cursors, advisory locks, LISTENs or open
    /// transaction, checked fresh right now. It is returned to the pool, its
    /// tab is told, and it is refused until it reconnects explicitly. A
    /// session holding any state, or running a statement, is never released:
    /// if no session qualifies, the new tab is refused instead, because a
    /// released session would silently be a different one (R-037).
    pub async fn make_room(&self, profile_key: &str, budget: usize) -> Result<Option<EditorEvent>, DriverError> {
        let mut live = self.live.lock().await;
        let mine: Vec<(String, Arc<Mutex<PinnedSession>>)> = live
            .iter()
            .filter(|(_, e)| e.profile_key == profile_key)
            .map(|(k, e)| (k.clone(), e.pinned.clone()))
            .collect();
        if mine.len() < budget {
            return Ok(None);
        }
        // LRU among the eligible.
        let mut best: Option<(String, Duration)> = None;
        let mut holding: Vec<String> = Vec::new();
        let mut running = 0usize;
        for (id, p) in &mine {
            let Ok(mut s) = p.try_lock() else {
                running += 1;
                continue;
            };
            if s.status().lost.is_some() {
                // Already dead: the cheapest thing to release.
                best = Some((id.clone(), Duration::MAX));
                break;
            }
            let idle = s.idle_for();
            match s.held().await {
                Ok(h) if h.is_empty() => {
                    if best.as_ref().is_none_or(|(_, b)| idle > *b) {
                        best = Some((id.clone(), idle));
                    }
                }
                Ok(h) => holding.extend(h.describe()),
                // Cannot show it is stateless, so it is not.
                Err(_) => holding.push("state that could not be checked".into()),
            }
        }
        let Some((victim, _)) = best else {
            holding.sort();
            holding.dedup();
            let what = if holding.is_empty() { String::new() } else { format!(" (held: {})", holding.join(", ")) };
            let busy = if running > 0 { format!(", {running} running a statement") } else { String::new() };
            return Err(DriverError::new(
                sluice_core::ErrorKind::Conflict,
                format!(
                    "This profile's connection budget is {budget}, and every editor session using it holds session state \
                     or is busy{busy}{what}, so none can be released without losing it. Commit or roll back, close an \
                     editor tab, or raise the profile's connection budget, then try again."
                ),
            ));
        };
        let e = live.remove(&victim).expect("found above");
        drop(live);
        // T-043: the banner names what was lost — here, provably nothing but
        // the connection itself.
        let reason = sluice_postgres::pinned::lost_message(
            "This editor's session was released to make room for another tab within the profile's connection budget.",
            &sluice_postgres::pinned::SessionHeld::default(),
        );
        self.released.lock().await.insert(victim.clone(), reason.clone());
        close_pinned(e.pinned).await;
        Ok(Some(EditorEvent {
            editor_id: victim,
            kind: "released",
            message: reason.clone(),
            status: SessionStatus {
                tx: TxState::Idle,
                tx_idle_secs: None,
                idle_timeout_secs: IDLE_TRANSACTION_TIMEOUT.as_secs(),
                lost: Some(reason),
                holds: vec![],
            },
        }))
    }

    pub async fn session_of(&self, id: &str) -> Option<String> {
        self.live.lock().await.get(id).map(|e| e.session_id.clone())
    }

    /// Remove and close one editor: rollback, then release.
    pub async fn close(&self, id: &str) {
        self.released.lock().await.remove(id);
        let e = self.live.lock().await.remove(id);
        if let Some(e) = e {
            close_pinned(e.pinned).await;
        }
    }

    /// Close every editor of a connection session (disconnect, tab close).
    pub async fn close_for_session(&self, session_id: &str) {
        let doomed: Vec<Editor> = {
            let mut m = self.live.lock().await;
            let ids: Vec<String> = m.iter().filter(|(_, e)| e.session_id == session_id).map(|(k, _)| k.clone()).collect();
            ids.into_iter().filter_map(|k| m.remove(&k)).collect()
        };
        for e in doomed {
            close_pinned(e.pinned).await;
        }
    }

    /// Close everything (window close / exit).
    pub async fn close_all(&self) {
        let all: Vec<Editor> = self.live.lock().await.drain().map(|(_, e)| e).collect();
        for e in all {
            close_pinned(e.pinned).await;
        }
    }

    /// One reaper pass: roll back open transactions idle past the timeout.
    /// A session busy running something is skipped (`try_lock`), never waited
    /// on — a long query is not idle.
    pub async fn reap(&self, timeout: Duration) -> Vec<EditorEvent> {
        let editors: Vec<(String, Arc<Mutex<PinnedSession>>)> =
            self.live.lock().await.iter().map(|(k, e)| (k.clone(), e.pinned.clone())).collect();
        let mut events = Vec::new();
        for (id, p) in editors {
            let Ok(mut s) = p.try_lock() else { continue };
            if s.tx() == TxState::Idle || s.idle_for() < timeout || s.status().lost.is_some() {
                continue;
            }
            let mins = timeout.as_secs() / 60;
            let message = match s.rollback().await {
                Ok(_) => format!("The open transaction was idle for over {mins} minutes and was rolled back. Nothing in it was committed."),
                Err(e) => format!("The open transaction was idle for over {mins} minutes; rolling it back failed: {}", e.message),
            };
            events.push(EditorEvent { editor_id: id, kind: "idle_rollback", message, status: s.status() });
        }
        events
    }
}

async fn close_pinned(p: Arc<Mutex<PinnedSession>>) {
    // Wait for an in-flight run to finish (or be cancelled) before closing,
    // so the rollback is not racing it. The session is then the only owner.
    let lock = p.lock().await;
    drop(lock);
    if let Ok(m) = Arc::try_unwrap(p) {
        m.into_inner().close().await;
    }
    // Another holder (a run that was just cancelled) still has it: dropping
    // the last reference drops the lease unreleased, which closes the socket
    // and the server rolls back. Rollback either way; never a commit.
}

/// Start the idle reaper.
pub fn spawn_reaper(app: tauri::AppHandle, editors: Arc<Editors>) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(REAP_EVERY).await;
            for ev in editors.reap(IDLE_TRANSACTION_TIMEOUT).await {
                let _ = app.emit(EDITOR_EVENT, ev);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use sluice_core::{Driver, RawRequest, Secret, WireValue};
    use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

    async fn driver() -> Option<Arc<PgDriver>> {
        let spec = ConnectSpec {
            host: "localhost".into(),
            port: 55432,
            user: "postgres".into(),
            password: Some(Secret::new("sluice_dev".into())),
            dbname: "sluice_dev".into(),
            tls: TlsMode::Disabled,
            application_name: "sluice-editor-test".into(),
            statement_timeout: None,
        };
        PgDriver::connect(spec).await.map_err(|_| eprintln!("SKIP: no Postgres reachable")).ok()
    }

    async fn count(d: &PgDriver) -> WireValue {
        let r = d.raw(RawRequest::Script("SELECT count(*) FROM public.t041_reap".into())).await.unwrap();
        r.first_rows().unwrap().1[0][0].clone()
    }

    /// The reaper rolls back an idle open transaction — never commits it —
    /// and leaves a merely idle session (no transaction) alone.
    #[tokio::test]
    async fn the_reaper_rolls_back_idle_open_transactions_only() {
        let Some(d) = driver().await else { return };
        d.raw(RawRequest::Script("DROP TABLE IF EXISTS public.t041_reap; CREATE TABLE public.t041_reap (x int)".into()))
            .await
            .unwrap();
        let editors = Editors::default();
        let mut open = d.open_pinned().await.unwrap();
        open.begin().await.unwrap();
        d.run_pinned(&mut open, "INSERT INTO public.t041_reap VALUES (1)").await.unwrap();
        editors.insert("open".into(), Editor { session_id: "s".into(), profile_key: "p".into(), pinned: Arc::new(Mutex::new(open)) }).await;
        let idle = d.open_pinned().await.unwrap();
        editors.insert("idle".into(), Editor { session_id: "s".into(), profile_key: "p".into(), pinned: Arc::new(Mutex::new(idle)) }).await;

        // Not idle long enough: nothing happens.
        assert!(editors.reap(Duration::from_secs(3600)).await.is_empty());

        let events = editors.reap(Duration::ZERO).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].editor_id, "open");
        assert_eq!(events[0].status.tx, TxState::Idle);
        assert!(events[0].message.contains("rolled back") && events[0].message.contains("Nothing in it was committed"));
        assert_eq!(count(&d).await, WireValue::Text("0".into()));

        editors.close_for_session("s").await;
        assert!(editors.get("open").await.is_none() && editors.get("idle").await.is_none());
    }

    fn ed(p: PinnedSession) -> Editor {
        Editor { session_id: "s".into(), profile_key: "p".into(), pinned: Arc::new(Mutex::new(p)) }
    }

    /// T-042 / D-028: at the budget, the least-recently-used idle editor is
    /// released (and told, and refused until it reconnects); one holding a
    /// transaction never is — the new tab is refused instead.
    #[tokio::test]
    async fn the_budget_releases_lru_idle_editors_and_never_an_open_transaction() {
        let Some(d) = driver().await else { return };
        let editors = Editors::default();

        // Under the budget: nothing is released.
        editors.insert("old".into(), ed(d.open_pinned().await.unwrap())).await;
        assert!(editors.make_room("p", 3).await.unwrap().is_none());

        tokio::time::sleep(Duration::from_millis(20)).await;
        editors.insert("newer".into(), ed(d.open_pinned().await.unwrap())).await;
        let mut busy = d.open_pinned().await.unwrap();
        busy.begin().await.unwrap();
        editors.insert("tx".into(), ed(busy)).await;

        // At the budget (3 of 3): the LRU idle one goes, not the newer idle
        // one, and never the one holding a transaction.
        let ev = editors.make_room("p", 3).await.unwrap().expect("one released");
        assert_eq!(ev.editor_id, "old");
        assert_eq!(ev.kind, "released");
        assert!(ev.status.lost.as_deref().unwrap().contains("Reconnect"));
        assert!(editors.get("old").await.is_none());
        assert!(editors.released("old").await.is_some());
        assert!(editors.get("tx").await.is_some());

        // Another profile's editors are not this budget's concern.
        assert!(editors.make_room("other", 1).await.unwrap().is_none());

        // Now everything left holds a transaction: refuse, release nothing.
        editors.get("newer").await.unwrap().lock().await.begin().await.unwrap();
        let e = editors.make_room("p", 2).await.unwrap_err();
        assert_eq!(e.kind, sluice_core::ErrorKind::Conflict);
        assert!(e.message.contains("an open transaction"), "{}", e.message);
        assert!(editors.get("newer").await.is_some() && editors.get("tx").await.is_some());

        // An explicit reconnect (close, then a new editor) clears the tombstone.
        editors.close("old").await;
        assert!(editors.released("old").await.is_none());
        editors.close_all().await;
    }

    /// T-043 / R-037: only a provably stateless session is released. One
    /// holding session state — even state set without a `SET` — is not, and
    /// the refusal names what is held.
    #[tokio::test]
    async fn only_provably_stateless_sessions_are_released() {
        let Some(d) = driver().await else { return };
        let editors = Editors::default();
        let mut a = d.open_pinned().await.unwrap();
        d.run_pinned(&mut a, "SELECT set_config('work_mem', '8MB', false)").await.unwrap();
        editors.insert("setcfg".into(), ed(a)).await;
        let mut b = d.open_pinned().await.unwrap();
        d.run_pinned(&mut b, "CREATE TEMP TABLE t043e (x int)").await.unwrap();
        editors.insert("temp".into(), ed(b)).await;

        let e = editors.make_room("p", 2).await.unwrap_err();
        assert!(e.message.contains("SET work_mem") && e.message.contains("1 temp table"), "{}", e.message);
        assert!(editors.get("setcfg").await.is_some() && editors.get("temp").await.is_some());

        // A stateless one alongside them is the one released, and its banner
        // says nothing but the connection was lost.
        editors.insert("plain".into(), ed(d.open_pinned().await.unwrap())).await;
        let ev = editors.make_room("p", 3).await.unwrap().expect("released");
        assert_eq!(ev.editor_id, "plain");
        assert!(ev.message.contains("held no session state"), "{}", ev.message);
        editors.close_all().await;
    }

    /// Closing an editor with an open transaction rolls it back.
    #[tokio::test]
    async fn closing_an_editor_rolls_back() {
        let Some(d) = driver().await else { return };
        d.raw(RawRequest::Script("DROP TABLE IF EXISTS public.t041_close; CREATE TABLE public.t041_close (x int)".into()))
            .await
            .unwrap();
        let editors = Editors::default();
        let mut p = d.open_pinned().await.unwrap();
        p.begin().await.unwrap();
        d.run_pinned(&mut p, "INSERT INTO public.t041_close VALUES (1)").await.unwrap();
        editors.insert("e".into(), Editor { session_id: "s".into(), profile_key: "p".into(), pinned: Arc::new(Mutex::new(p)) }).await;
        editors.close_all().await;
        let r = d.raw(RawRequest::Script("SELECT count(*) FROM public.t041_close".into())).await.unwrap();
        assert_eq!(r.first_rows().unwrap().1[0][0], WireValue::Text("0".into()));
    }
}
