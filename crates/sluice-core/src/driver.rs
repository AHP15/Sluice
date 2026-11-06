//! The `Driver` trait.
//!
//! D-003's actual requirement is not "a trait exists" — it is that the trait can
//! be lifted onto a wire protocol later **without redesign**. That is why every
//! method takes and returns owned plain data: no lifetimes, no engine handles,
//! nothing opaque escaping into the interface. `lib.rs` enforces this with
//! compile-time assertions rather than trusting review.

use async_trait::async_trait;

use crate::capability::Capabilities;
use crate::error::DriverError;
use crate::outcome::ExecutionResult;
use crate::request::{BrowseRequest, EditPlan, EditRequest, NavNode, NodeId, RawRequest};

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
}
