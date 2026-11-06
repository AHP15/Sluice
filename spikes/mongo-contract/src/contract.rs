//! Draft of the cross-paradigm Driver contract, as D-003/D-004 describe it and
//! as T-005 will codify it.
//!
//! This is deliberately written from the *relational* mental model the recorded
//! decisions were made in, and is NOT pre-bent to fit Mongo. R-029's whole point
//! is that a contract shaped to the engine by its own author validates nothing.
//! Every place the Mongo implementation has to fight this file is a finding.
//!
//! Contract rules being tested (D-003): every method takes and returns owned
//! plain data. No lifetimes, no engine handles, nothing opaque in a signature.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- capabilities

/// R-039: three-valued from the start, so Mongo's value can be set on evidence.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionAffinity {
    /// A socket can be pinned for a tab's lifetime (Postgres, SQL Server).
    Connection,
    /// No socket pin, but a logical session carrying causal consistency and
    /// interactive multi-document transactions can be pinned.
    Session,
    /// Nothing pinnable; every statement is independent.
    None,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResetSupport {
    /// A real reset primitive exists (Postgres DISCARD ALL).
    Native,
    /// No primitive; the pool must reopen to guarantee a clean session.
    ReopenOnly,
    /// Session state is not a concept for this engine.
    NotApplicable,
}

/// A node kind the driver declares for the navigator tree (T-008).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NodeKind {
    pub id: String,
    pub label: String,
    pub has_children: bool,
    /// True if nodes of this kind can be the target of a browse request.
    pub is_relation: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FilterOpDecl {
    pub id: String,
    pub label: String,
    /// Type classes this operator applies to (T-013).
    pub type_classes: Vec<String>,
    /// R-009: whether string matching is case-sensitive as executed.
    pub case_sensitive: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Capabilities {
    pub engine: String,
    pub affinity: SessionAffinity,
    pub reset: ResetSupport,
    pub node_kinds: Vec<NodeKind>,
    pub filter_ops: Vec<FilterOpDecl>,
    pub supports_transactions: bool,
    /// Whether the engine can express a multi-statement script at all (T-011).
    pub supports_script: bool,
    /// Whether every relation has a fixed, catalog-declared column set.
    pub fixed_schema: bool,
}

// ---------------------------------------------------------------- navigator

/// Identity of a node in the navigator tree. Owned plain data: a declared kind
/// plus a path of string segments the driver interprets.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct NodeId {
    pub kind: String,
    pub path: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NavNode {
    pub id: NodeId,
    pub label: String,
    pub has_children: bool,
}

// ---------------------------------------------------------------- values

/// Lossless wire value (T-016). Exact numerics travel as strings so no f64
/// rounding happens anywhere in the pipeline.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum WireValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// Exact numeric, preserved as its literal text.
    Numeric(String),
    Text(String),
    Bytes(Vec<u8>),
    /// Timestamp/date, ISO-8601 text.
    Timestamp(String),
    /// T-017: a type the driver could not map, rendered for display only.
    Unsupported { type_name: String, rendered: String },
}

/// Per-column descriptor accompanying a row set (T-016).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ColumnDesc {
    pub name: String,
    /// Engine-native type name, verbatim.
    pub type_name: String,
    /// Coarse class the UI switches on (T-013).
    pub type_class: String,
    pub nullable: bool,
}

// ---------------------------------------------------------------- browse

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Predicate {
    pub column: String,
    pub op: String,
    pub value: WireValue,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum FilterTree {
    And(Vec<FilterTree>),
    Or(Vec<FilterTree>),
    Pred(Predicate),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SortKey {
    pub column: String,
    pub descending: bool,
}

/// Structured browse request compiled per driver (T-009). The UI never emits SQL.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BrowseRequest {
    pub relation: NodeId,
    pub filter: Option<FilterTree>,
    pub sort: Vec<SortKey>,
    pub limit: u32,
    /// Keyset cursor (T-047): the last row's sort-key values, owned.
    pub cursor: Option<Vec<WireValue>>,
}

// ---------------------------------------------------------------- edits

/// A staged edit against one row (T-056/D-027).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EditRequest {
    pub relation: NodeId,
    /// Unique-key columns identifying the row, and their values.
    pub key: Vec<(String, WireValue)>,
    /// Columns being set, and their new values.
    pub set: Vec<(String, WireValue)>,
    /// Optimistic-concurrency originals (T-058): every comparable column's
    /// value as read.
    pub originals: Vec<(String, WireValue)>,
}

// ---------------------------------------------------------------- outcomes

/// One statement's outcome (T-010). A script yields a sequence of these.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Rows {
        columns: Vec<ColumnDesc>,
        rows: Vec<Vec<WireValue>>,
        truncated: bool,
    },
    Affected {
        count: u64,
    },
    Ack {
        message: String,
    },
}

/// T-012: the transactional context the outcome sequence executed in, so a
/// partial-success report cannot be silently false.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TxContext {
    /// Each statement committed independently as it ran.
    Autocommit,
    /// All statements ran inside one transaction that committed.
    SingleCommitted,
    /// All statements ran inside one transaction that rolled back.
    SingleRolledBack,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExecutionResult {
    pub outcomes: Vec<Outcome>,
    pub tx: TxContext,
}

#[derive(Debug)]
pub struct DriverError {
    pub kind: String,
    pub message: String,
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.kind, self.message)
    }
}

impl std::error::Error for DriverError {}

// ---------------------------------------------------------------- the trait

#[async_trait]
pub trait Driver: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    /// `None` parent = tree roots.
    async fn navigator_children(
        &self,
        parent: Option<NodeId>,
    ) -> Result<Vec<NavNode>, DriverError>;

    async fn browse(&self, req: BrowseRequest) -> Result<ExecutionResult, DriverError>;

    async fn apply_edit(&self, edit: EditRequest) -> Result<ExecutionResult, DriverError>;

    /// The raw passthrough the query editor sends (D-029).
    async fn raw_command(&self, script: String) -> Result<ExecutionResult, DriverError>;
}
