//! The capability descriptor.
//!
//! D-003: every behavioural difference between engines is expressed as declared
//! capability data, never as engine checks in the React layer. R-021 is on the
//! record as a defer — flags proliferating until the UI is as coupled as the
//! checks they replaced — with a trigger at 12 flags, so this set is kept
//! deliberately small and each field records why it must be declared.

use crate::value::TypeClass;
use serde::{Deserialize, Serialize};

/// What can be pinned for the lifetime of an editor tab.
///
/// Three-valued from the start per R-039, which was raised precisely because a
/// two-valued form would have foreclosed Mongo's real capability.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionAffinity {
    /// A socket can be pinned (Postgres, SQL Server).
    Connection,
    /// No socket pin, but a logical session carrying causal consistency and
    /// interactive multi-document transactions can be.
    ///
    /// T-046 confirmed this is Mongo's correct value against a live server:
    /// `Client::start_session()` yields an owned, lifetime-free session that
    /// held a genuine interactive transaction across two collections.
    Session,
    /// Nothing pinnable; every statement is independent.
    None,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResetSupport {
    /// A real reset primitive exists (Postgres `DISCARD ALL`).
    Native,
    /// No reachable primitive; the pool must reopen to guarantee a clean
    /// session.
    ///
    /// T-072 confirmed this is SQL Server's value: `sp_reset_connection` is a
    /// TDS protocol RPC signalled by a header bit, not callable T-SQL, and
    /// tiberius exposes no API to set it. This is what makes T-040 (reset only
    /// connections actually dirtied) load-bearing rather than an optimisation,
    /// because the naive reading degrades the pool to connect-per-query (R-036).
    ReopenOnly,
    /// Session state is not a concept for this engine.
    NotApplicable,
}

/// How an in-flight query can actually be stopped (T-051).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CancelSupport {
    /// A protocol-level cancel the driver can issue on the same connection
    /// (Postgres cancel request).
    Native,
    /// No protocol cancel; the only route is a second connection terminating
    /// the session by id.
    ///
    /// T-072: tiberius exposes no attention/cancel API at all. Dropping the
    /// query future does not cancel — measured, it left the connection unusable
    /// AND the server-side query still running. The `KILL <spid>` fallback needs
    /// `ALTER ANY CONNECTION`, which many day-to-day accounts lack.
    KillSession,
    /// Nothing available; only a statement timeout applies.
    None,
}

/// What the raw editor may send this engine.
///
/// T-073 F-5: `raw(String)` assumed a SQL script because the editor is a SQL
/// editor. Mongo's raw surface is a single BSON command *document* and there is
/// no such thing as a multi-statement Mongo script, so the payload shape is
/// declared rather than assumed.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RawPayloadKind {
    /// A possibly multi-statement script in the engine's dialect.
    Script,
    /// A single command document, serialized as JSON.
    CommandDocument,
}

/// A node kind the driver declares for the navigator tree (T-008).
///
/// T-073 F-6: this design is the contract's strongest validation — two declared
/// kinds (`database` -> `collection`) expressed Mongo's whole tree with no
/// schema level and no placeholder node.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NodeKind {
    pub id: String,
    pub label: String,
    pub has_children: bool,
    /// Nodes of this kind can be the target of a browse request.
    pub is_relation: bool,
}

/// D-005's universal core: every driver must compile and declare each of these.
///
/// Value shapes, which every driver honours identically:
/// - `eq` `ne` `lt` `lte` `gt` `gte` `contains` `starts_with` `ends_with` — one
///   non-null value (compare to NULL with `is_null`, never `eq`);
/// - `in` — a `WireValue::Array` of non-null values; an empty list matches
///   nothing;
/// - `between` — a `WireValue::Array` of exactly two non-null values, inclusive;
/// - `is_null` `is_not_null` — no value.
///
/// Engine-specific operators are *extensions*, declared beside these (T-014).
pub const CORE_FILTER_OPS: &[&str] = &[
    "eq",
    "ne",
    "lt",
    "lte",
    "gt",
    "gte",
    "in",
    "between",
    "is_null",
    "is_not_null",
    "contains",
    "starts_with",
    "ends_with",
];

/// Core operators a driver's capability descriptor fails to declare — the
/// conformance check every driver's tests run (T-013).
pub fn missing_core_ops(declared: &[FilterOpDecl]) -> Vec<&'static str> {
    CORE_FILTER_OPS
        .iter()
        .copied()
        .filter(|op| !declared.iter().any(|d| d.id == *op))
        .collect()
}

/// A filter operator the driver can compile (T-013/T-014).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FilterOpDecl {
    pub id: String,
    pub label: String,
    pub type_classes: Vec<TypeClass>,
    /// R-009: the universal `contains` is case-insensitive on Postgres (ILIKE)
    /// and case-sensitive on Mongo (`$regex`) unless the driver opts out.
    /// T-073 F-9 reproduced this live. `None` means not a string operator.
    pub case_sensitive: Option<bool>,
    /// T-015 / R-009: the operator honours `Predicate::case_insensitive`, so
    /// the filter bar may offer the toggle. False means the flag is refused.
    #[serde(default)]
    pub case_insensitive_toggle: bool,
}

/// Transaction behaviour an editor tab can rely on.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionSupport {
    /// The application controls the boundary and may hold it open across its
    /// own await points — as opposed to a driver-run callback helper.
    pub interactive: bool,
    /// Server-side ceiling on how long an idle transaction survives.
    ///
    /// T-046: Mongo's `transactionLifetimeLimitSeconds` defaults to **60**, so
    /// an editor tab that opens a transaction and waits while the user thinks
    /// is killed server-side. Postgres and SQL Server hold one indefinitely.
    /// `SessionAffinity` declares that a session *can* be pinned and says
    /// nothing about how long the pin survives, so the ceiling is declared
    /// separately. `None` means no server-imposed limit.
    pub max_idle_seconds: Option<u32>,
}

/// The full descriptor a driver publishes about itself.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Capabilities {
    /// Stable engine id (`postgres`, `sqlserver`, `mongodb`).
    pub engine: String,
    /// Server version banner, once known (T-054).
    pub server_version: Option<String>,
    /// Flavour, where the banner is not the whole story (T-055).
    pub flavour: Option<String>,

    pub affinity: SessionAffinity,
    pub reset: ResetSupport,
    pub cancel: CancelSupport,
    pub transactions: Option<TransactionSupport>,
    pub raw_payload: RawPayloadKind,

    pub node_kinds: Vec<NodeKind>,
    pub filter_ops: Vec<FilterOpDecl>,

    /// False for schemaless engines: every relation's column set is derived per
    /// page rather than declared by a catalog (T-073 F-1, D-018).
    pub fixed_schema: bool,
    /// Whether a single raw submission can yield more than one outcome.
    ///
    /// T-073 F-5: false on Mongo, where the outcome *sequence* (T-010) is
    /// permanently length 1 and T-011's script splitting has no meaning.
    pub supports_script: bool,
    /// T-015 / R-009: the server's default string collation, as the engine
    /// names it — shown in the filter bar beside each operator's declared
    /// case sensitivity, because that sensitivity is the collation's, not the
    /// tool's. `None` where the engine has no such notion.
    #[serde(default)]
    pub string_collation: Option<String>,
}
