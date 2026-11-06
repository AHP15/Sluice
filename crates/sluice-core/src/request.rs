//! Request types crossing the contract.
//!
//! Kept to what the `Driver` signatures need. The operator set (T-013/T-014),
//! keyset declaration (T-047/T-048) and staged-edit machinery (T-056) are
//! fleshed out by their own tasks; this is the shape they slot into.

use crate::value::WireValue;
use serde::{Deserialize, Serialize};

/// Identity of a navigator node: a declared kind plus a path the driver
/// interprets. Owned plain data — no engine handle, no lifetime.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Predicate {
    pub column: String,
    /// An operator id the driver declared in `Capabilities::filter_ops`.
    pub op: String,
    pub value: WireValue,
    /// T-015 / R-009: compare case-insensitively, whatever the engine's
    /// collation does by default. Off means the engine's own semantics, which
    /// the capability descriptor reports (`FilterOpDecl::case_sensitive`).
    /// Each driver compiles it its own way (`ILIKE`, `LOWER()`, a collation).
    #[serde(default)]
    pub case_insensitive: bool,
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

/// A structured browse. The UI never emits engine syntax (D-029); the driver
/// compiles this into whatever its engine speaks.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BrowseRequest {
    pub relation: NodeId,
    pub filter: Option<FilterTree>,
    pub sort: Vec<SortKey>,
    pub limit: u32,
    /// Keyset cursor: the previous result's `PageInfo::Keyset::next_cursor`
    /// (T-047), one value per key column.
    pub cursor: Option<Vec<WireValue>>,
    /// The previous result's `PageInfo::Offset::next_offset`, for relations
    /// that cannot page by keyset (T-047).
    #[serde(default)]
    pub offset: Option<u64>,
}

/// One optimistic-concurrency original (T-058).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Original {
    pub column: String,
    pub value: WireValue,
}

/// A staged edit against one row (T-056/D-027).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct EditRequest {
    pub relation: NodeId,
    /// Unique-key columns identifying the row.
    pub key: Vec<Original>,
    /// Columns being written.
    pub set: Vec<Original>,
    /// Values as they were read. The driver decides per value which of these can
    /// enter the predicate and MUST report the ones it dropped — see
    /// `Outcome::Affected::concurrency_omitted` (T-073 F-4).
    pub originals: Vec<Original>,
}

/// The raw editor payload. Which variant is legal is declared by
/// `Capabilities::raw_payload` (T-073 F-5).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", content = "body", rename_all = "snake_case")]
pub enum RawRequest {
    Script(String),
    CommandDocument(String),
}
