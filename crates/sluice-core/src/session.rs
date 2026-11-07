//! Engine-neutral streaming, cancellation and editor-session contract
//! (T-076, T-077 / D-003, D-028, D-010, D-029).
//!
//! These used to be inherent to the Postgres driver, which meant the app core
//! named `PgDriver` on every streaming, export, cancel and editor path. They
//! live here so a second engine plugs into the same paths through
//! `Arc<dyn Driver>`.
//!
//! ## On D-003's "owned plain data"
//!
//! Every *value* that crosses these interfaces — batches, stream ends, session
//! status, pool counters — is owned and serializable. Two things are not
//! plain data, by nature, and are the in-process stand-ins for what a wire
//! protocol would express as messages:
//!
//! - a [`BatchSink`] is a stream of `Columns`/`Rows` messages with a credit
//!   reply (`bool`) per message — exactly T-049's backpressure protocol;
//! - an [`EditorSession`] is a session handle — on a wire, a session id that
//!   the driver resolves.
//!
//! Neither carries a lifetime or an engine type, which is the property D-003
//! needs to stay liftable.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::DriverError;
use crate::outcome::{ExecBudget, ExecutionResult, PageInfo};
use crate::request::EditGate;
use crate::value::{ColumnDesc, WireValue};

/// An owned, sendable future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Where a streamed call delivers its batches. The returned future resolves
/// once the consumer has credit for the next batch; `false` stops the stream
/// (T-049).
pub type BatchSink<B> = Box<dyn FnMut(B) -> BoxFuture<bool> + Send>;

/// Wrap an async-returning closure as a [`BatchSink`].
pub fn sink<B, F, Fut>(mut f: F) -> BatchSink<B>
where
    F: FnMut(B) -> Fut + Send + 'static,
    Fut: Future<Output = bool> + Send + 'static,
{
    Box::new(move |b| Box::pin(f(b)))
}

/// One item of a streamed browse (T-049).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum BrowseBatch {
    /// Always first: the page's column descriptors (D-006), and whether the
    /// relation's rows may be edited (T-056).
    Columns(Vec<ColumnDesc>, EditGate),
    Rows(Vec<Vec<WireValue>>),
}

/// How a streamed browse ended.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct BrowseStreamEnd {
    pub rows: usize,
    /// The request's `limit` was reached (more rows may exist).
    pub truncated: bool,
    /// The consumer stopped it (cancel, timeout, or a closed channel).
    pub stopped: bool,
    /// T-047: the strategy used and the next page's position (unstopped only).
    pub page: Option<PageInfo>,
}

/// One item of a streamed export (T-074).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ExportBatch {
    /// Always first.
    Columns(Vec<ColumnDesc>),
    Rows(Vec<Vec<WireValue>>),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ExportEnd {
    pub rows: u64,
    /// The consumer stopped it (cancel, or a write failure it reported).
    pub stopped: bool,
}

/// The engine's way of stopping one in-flight query (T-051): Postgres's
/// protocol cancel request, SQL Server's close-and-declare (D-030), and so on.
#[async_trait]
pub trait CancelHandle: Send + Sync {
    /// Ask the server to stop. Returns whether the request was delivered.
    async fn cancel(&self) -> bool;
}

/// T-051 / D-010: the handle that cancels one in-flight query.
///
/// A driver call made with a slot (`Driver::with_cancel`) arms it with its
/// connection's [`CancelHandle`] when the work starts and disarms it at the
/// end. [`CancelSlot::cancel`] stops whatever is running — or, if nothing has
/// started yet, makes the next arm refuse, closing the cancel-before-start
/// race.
#[derive(Clone, Default)]
pub struct CancelSlot {
    inner: Arc<std::sync::Mutex<SlotState>>,
}

#[derive(Default)]
struct SlotState {
    handle: Option<Arc<dyn CancelHandle>>,
    cancelled: bool,
}

impl CancelSlot {
    /// Register the running query's handle. `false` means the slot was
    /// already cancelled and the work must not start.
    pub fn arm(&self, handle: Arc<dyn CancelHandle>) -> bool {
        let mut s = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if s.cancelled {
            return false;
        }
        s.handle = Some(handle);
        true
    }

    pub fn disarm(&self) {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).handle = None;
    }

    /// Mark cancelled and take the handle of the query running now, if any.
    pub fn trip(&self) -> Option<Arc<dyn CancelHandle>> {
        let mut s = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        s.cancelled = true;
        s.handle.take()
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).cancelled
    }

    /// Trip the slot and deliver the cancel. `false` means nothing was
    /// running (and nothing will start on this slot now) or the request
    /// could not be delivered.
    pub async fn cancel(&self) -> bool {
        match self.trip() {
            Some(h) => h.cancel().await,
            None => false,
        }
    }
}

/// Release counters for a driver's pool (T-040: "reopen rate is
/// instrumented").
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStatsSnapshot {
    /// Leases ended, by any path.
    pub releases: u64,
    /// Connections returned after the native reset primitive.
    pub resets: u64,
    /// Connections closed rather than returned — `reopen` capability, a failed
    /// reset, or a lease dropped without release.
    pub reopens: u64,
    /// Connections returned as-is because nothing could have dirtied them.
    pub clean_returns: u64,
}

/// D-028's per-profile connection budget, "~5, configurable" (T-042). One
/// ceiling for pooled and pinned connections together, whatever the engine.
pub const DEFAULT_CONNECTION_BUDGET: usize = 5;

/// D-028's idle timeout for an open transaction in an editor tab. Past it the
/// transaction is rolled back (never committed) and the tab is told.
pub const IDLE_TRANSACTION_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The transaction state as the server reports it after the last run.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TxState {
    /// No transaction block: each statement commits as it runs.
    Idle,
    /// An explicit transaction is open; nothing in it is durable yet.
    Open,
    /// The open transaction hit an error; the server will accept only a
    /// rollback.
    Failed,
}

/// T-043 / R-037: the session state a pinned connection holds, as the
/// *server* reports it — which is what lets the app *show* a tab is stateless
/// rather than guess it from the statements it ran. Kinds an engine does not
/// have stay zero.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionHeld {
    /// Settings changed for the session.
    pub settings: Vec<String>,
    pub temp_tables: i64,
    /// SQL-level prepared statements (the driver's own are excluded).
    pub prepared: i64,
    /// Named cursors still open.
    pub cursors: i64,
    pub advisory_locks: i64,
    pub listening: i64,
    pub transaction: bool,
}

impl SessionHeld {
    pub fn is_empty(&self) -> bool {
        self == &SessionHeld::default()
    }

    /// Human-readable, one item per kind of state held.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.transaction {
            out.push("an open transaction".to_string());
        }
        if !self.settings.is_empty() {
            out.push(format!("SET {}", self.settings.join(", ")));
        }
        let n = |k: i64, one: &str, many: &str| match k {
            0 => None,
            1 => Some(format!("1 {one}")),
            k => Some(format!("{k} {many}")),
        };
        out.extend(n(self.temp_tables, "temp table", "temp tables"));
        out.extend(n(self.prepared, "prepared statement", "prepared statements"));
        out.extend(n(self.cursors, "open cursor", "open cursors"));
        out.extend(n(self.advisory_locks, "advisory lock", "advisory locks"));
        out.extend(n(self.listening, "LISTEN channel", "LISTEN channels"));
        out
    }
}

/// What the editor tab shows about its session.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SessionStatus {
    pub tx: TxState,
    /// Seconds the open transaction has been idle, when one is open.
    pub tx_idle_secs: Option<u64>,
    /// The idle timeout after which an open transaction is rolled back.
    pub idle_timeout_secs: u64,
    /// The connection is gone; nothing more will run on this session.
    pub lost: Option<String>,
    /// T-043: session state held as of the last run, described.
    pub holds: Vec<String>,
}

/// T-043: a lost or released session's banner text, naming what went with it.
pub fn lost_message(what_happened: &str, held: &SessionHeld) -> String {
    let items = held.describe();
    let gone = if items.is_empty() {
        "It held no session state, so only the connection itself is gone.".to_string()
    } else {
        format!(
            "Gone with it: {}{}.",
            items.join(", "),
            if held.transaction { " (rolled back, never committed)" } else { "" }
        )
    };
    format!("{what_happened} {gone} Reconnect to continue in a new session.")
}

/// T-041 / D-028: one pinned editor session — a server session held for an
/// editor tab's lifetime, on an engine that declares session affinity.
///
/// Obtained from `Driver::open_pinned` and run through `Driver::run_pinned`,
/// which adds the driver's cancel slot, redaction and catalog invalidation
/// around [`EditorSession::run`].
#[async_trait]
pub trait EditorSession: Send {
    fn tx(&self) -> TxState;
    fn status(&self) -> SessionStatus;
    fn idle_for(&self) -> Duration;
    /// What would stop a query running on this session now; `None` once the
    /// session is lost or closed.
    fn cancel_handle(&self) -> Option<Arc<dyn CancelHandle>>;
    /// T-043: ask the server what session state this session holds now.
    async fn held(&mut self) -> Result<SessionHeld, DriverError>;
    /// Run a raw script on the pinned session.
    async fn run(&mut self, script: &str, budget: ExecBudget) -> Result<ExecutionResult, DriverError>;
    /// The opt-in: begin an explicit transaction.
    async fn begin(&mut self) -> Result<SessionStatus, DriverError>;
    async fn commit(&mut self) -> Result<SessionStatus, DriverError>;
    async fn rollback(&mut self) -> Result<SessionStatus, DriverError>;
    /// End the session: roll back anything open (never commit) and return
    /// the connection through the declared reset.
    async fn close(self: Box<Self>);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Flag(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait]
    impl CancelHandle for Flag {
        async fn cancel(&self) -> bool {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            true
        }
    }

    #[tokio::test]
    async fn a_slot_cancelled_before_start_refuses_to_arm() {
        let slot = CancelSlot::default();
        assert!(!slot.cancel().await, "nothing was running");
        let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(!slot.arm(Arc::new(Flag(hit.clone()))));
        assert!(!hit.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn an_armed_slot_delivers_the_cancel_to_the_running_query() {
        let slot = CancelSlot::default();
        let hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(slot.arm(Arc::new(Flag(hit.clone()))));
        assert!(slot.cancel().await);
        assert!(hit.load(std::sync::atomic::Ordering::SeqCst));
        assert!(slot.is_cancelled());
    }

    #[test]
    fn stream_items_are_wire_shaped() {
        fn wire<T: 'static + Send + Sync + Serialize + serde::de::DeserializeOwned>() {}
        wire::<BrowseBatch>();
        wire::<BrowseStreamEnd>();
        wire::<ExportBatch>();
        wire::<ExportEnd>();
        wire::<PoolStatsSnapshot>();
        wire::<SessionStatus>();
        wire::<SessionHeld>();
        wire::<TxState>();
    }
}
