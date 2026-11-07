//! The `Driver` trait.
//!
//! D-003's actual requirement is not "a trait exists" — it is that the trait can
//! be lifted onto a wire protocol later **without redesign**. That is why every
//! method takes and returns owned plain data: no lifetimes, no engine handles,
//! nothing opaque escaping into the interface. `lib.rs` enforces this with
//! compile-time assertions rather than trusting review.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::capability::Capabilities;
use crate::error::DriverError;
use crate::export::SqlLiterals;
use crate::outcome::{ExecBudget, ExecutionResult};
use crate::redact::Redactor;
use crate::request::{BrowseRequest, EditPlan, EditRequest, NavNode, NodeId, RawRequest};
use crate::session::{
    BatchSink, BrowseBatch, BrowseStreamEnd, CancelSlot, EditorSession, ExportBatch, ExportEnd,
    PoolStatsSnapshot,
};
use crate::value::WireValue;

#[async_trait]
pub trait Driver: Send + Sync + 'static {
    /// What this engine can do. Cheap and synchronous — callers may read it per
    /// render, and the UI branches on this instead of on the engine name.
    fn capabilities(&self) -> Capabilities;

    /// Children of a navigator node; `None` means the tree roots (T-008).
    async fn navigator_children(
        &self,
        parent: Option<NodeId>,
    ) -> Result<Vec<NavNode>, DriverError>;

    /// Compile and run a structured browse (T-009).
    async fn browse(&self, request: BrowseRequest) -> Result<ExecutionResult, DriverError>;

    /// The exact statements `apply_edits` would run for `edits`, without
    /// running them (T-057 / R-003). Built by the same code path as apply.
    async fn preview_edits(&self, edits: Vec<EditRequest>) -> Result<EditPlan, DriverError>;

    /// Apply staged edits. D-027 requires all of them in ONE transaction, so
    /// they arrive together rather than one call per row (T-056).
    ///
    /// `expected_digest` is the `EditPlan::digest` the user previewed; when
    /// given, the driver refuses — before opening a transaction — if the edits
    /// no longer compile to exactly that plan (T-057).
    async fn apply_edits(
        &self,
        edits: Vec<EditRequest>,
        expected_digest: Option<String>,
    ) -> Result<ExecutionResult, DriverError>;

    /// The raw editor passthrough. Callers must send the variant this driver
    /// declared in `Capabilities::raw_payload` (T-073 F-5).
    async fn raw(&self, request: RawRequest) -> Result<ExecutionResult, DriverError>;

    // ---- T-076: streamed browse, export, and value fetch -------------------

    /// Structured browse, streamed in batches (T-049 / R-006, D-010).
    ///
    /// Columns first, then row batches of at most `batch_rows`. `emit`
    /// resolves once the consumer has credit; the driver must not read further
    /// ahead than that, and on `false` must stop the query **server-side**.
    async fn browse_batches(
        &self,
        request: BrowseRequest,
        batch_rows: usize,
        emit: BatchSink<BrowseBatch>,
    ) -> Result<BrowseStreamEnd, DriverError>;

    /// Export a browsed relation in full (T-074 / D-024): the request's filter
    /// and sort, every row, every value uncapped, streamed to `emit`.
    async fn export_browse(
        &self,
        request: BrowseRequest,
        batch_rows: usize,
        emit: BatchSink<ExportBatch>,
    ) -> Result<ExportEnd, DriverError>;

    /// Re-run **one** raw statement under a guarantee that it cannot write,
    /// and export its rows (T-074). How "one statement" and "cannot write" are
    /// enforced is the engine's — by its own parser and its own read-only
    /// mode, never by reading the SQL in the app (R-040).
    async fn export_query(
        &self,
        statement: String,
        batch_rows: usize,
        emit: BatchSink<ExportBatch>,
    ) -> Result<ExportEnd, DriverError>;

    /// How this engine spells identifiers and literals for SQL `INSERT`
    /// export into `relation` — or, for a raw query, a placeholder target
    /// named `placeholder`.
    fn export_literals(
        &self,
        relation: Option<NodeId>,
        placeholder: String,
    ) -> Result<Box<dyn SqlLiterals>, DriverError>;

    /// Fetch a value that was replaced by a `WireValue::Oversized`
    /// placeholder (T-018), in full.
    async fn fetch_value(&self, handle: String) -> Result<WireValue, DriverError>;

    /// Stream an oversized value straight to `dest` without holding it whole
    /// (T-018 / R-008). Returns the bytes written.
    async fn stream_value_to_file(&self, handle: String, dest: PathBuf) -> Result<u64, DriverError>;

    // ---- T-077: cancellation, pinned sessions, catalog, pool --------------

    /// A copy of this driver whose calls arm `slot`, so the query they run can
    /// be stopped with [`Driver::cancel`] (T-051). Shares the pool.
    fn with_cancel(self: Arc<Self>, slot: CancelSlot) -> Arc<dyn Driver>;

    /// A copy with a different per-execution result budget (T-050).
    fn with_budget(self: Arc<Self>, budget: ExecBudget) -> Arc<dyn Driver>;

    /// Stop the query registered in `slot`, in the engine's declared way
    /// (`Capabilities::cancel`). Returns whether a request was delivered.
    async fn cancel(&self, slot: &CancelSlot) -> bool {
        slot.cancel().await
    }

    /// T-041 / D-028: pin a server session for an editor tab's lifetime. Only
    /// meaningful when `Capabilities::affinity` is not `none`.
    async fn open_pinned(&self) -> Result<Box<dyn EditorSession>, DriverError>;

    /// T-041: run a raw script on a pinned session, with the same redaction,
    /// cancel slot and catalog invalidation as the pooled raw path.
    async fn run_pinned(
        &self,
        session: &mut dyn EditorSession,
        script: String,
    ) -> Result<ExecutionResult, DriverError>;

    /// T-053 / D-016: drop the navigator cache.
    fn refresh_catalog(&self);

    /// Release counters for this driver's pool (T-040).
    fn pool_stats(&self) -> PoolStatsSnapshot;

    /// T-042 / D-028: the profile's connection budget, pooled and pinned
    /// together.
    fn connection_budget(&self) -> usize;

    /// T-044: the server's own connection ceiling, if it could be read.
    fn max_connections(&self) -> Option<i64>;

    /// The redactor for this connection's credentials (T-024), for callers
    /// holding errors the driver did not produce (a caught panic, say).
    fn redactor(&self) -> &Redactor;
}
