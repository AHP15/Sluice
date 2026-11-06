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

/// Adjacently tagged — `{"node": "and", "0": [...]}`, `{"node": "pred", "0":
/// {...}}` — which is the shape `src/lib/contract.ts` declares. It was
/// internally tagged, which serde cannot do for a newtype variant holding a
/// sequence: every `and`/`or` tree failed to (de)serialize, so only a bare
/// predicate could cross IPC (found by T-070's filter bar).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "node", content = "0", rename_all = "snake_case")]
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

/// Whether a browsed relation's rows may be edited (T-056 / D-027).
///
/// D-027 requires a unique key to edit: without one, an `UPDATE ... WHERE`
/// cannot be guaranteed to touch exactly the row the user is looking at. A
/// relation without one is read-only and the UI says why, rather than offering
/// an edit that would hit zero rows or several.
///
/// The driver re-derives this on every `apply_edits` call; the copy the UI
/// holds decides what it *offers*, never what is *allowed*.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EditGate {
    /// Rows are identified by `key`, a unique key whose columns are all
    /// NOT NULL, in key order.
    Editable { key: Vec<String> },
    ReadOnly { reason: String },
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

/// One bound parameter of a planned statement, as it is sent (T-057).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BoundParam {
    /// The placeholder as it appears in `statement` (`$3` on Postgres).
    pub placeholder: String,
    /// The type the statement casts it to.
    pub type_name: String,
    /// The value exactly as bound; `None` is NULL.
    pub value: Option<String>,
}

/// One statement the edit path will run, as a single artifact (T-057 / R-003).
///
/// `statement` and `params` are what executes. `inline` is a display-only
/// rendering *of that same artifact*, never a second build from the edit set,
/// and is `None` when some value cannot be faithfully shown inline — the UI then
/// shows `statement` with the `params` table instead of inventing a literal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct PlannedStatement {
    pub statement: String,
    pub params: Vec<BoundParam>,
    pub inline: Option<String>,
    /// Written columns not checked for concurrent changes (T-058).
    pub concurrency_omitted: Vec<String>,
}

/// The statements a set of staged edits compiles to, previewed before apply.
///
/// `digest` identifies exactly these statements and parameters. Apply passes
/// it back, and the driver refuses to run anything that does not recompile to
/// the same digest — so what executes is what was shown, even if the catalog
/// changed in between.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct EditPlan {
    pub statements: Vec<PlannedStatement>,
    pub digest: String,
}

impl EditPlan {
    pub fn new(statements: Vec<PlannedStatement>) -> Self {
        use std::hash::{Hash, Hasher};
        // Fixed-key SipHash: stable across calls in one process, which is the
        // lifetime a preview and its apply share. This is an equality check
        // between two builds, not a security boundary.
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for st in &statements {
            st.statement.hash(&mut h);
            for p in &st.params {
                p.type_name.hash(&mut h);
                p.value.hash(&mut h);
            }
        }
        Self { digest: format!("{:016x}", h.finish()), statements }
    }
}

/// The raw editor payload. Which variant is legal is declared by
/// `Capabilities::raw_payload` (T-073 F-5).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", content = "body", rename_all = "snake_case")]
pub enum RawRequest {
    Script(String),
    CommandDocument(String),
}

#[cfg(test)]
mod filter_wire_tests {
    use super::*;

    /// The exact JSON the webview sends (`src/lib/filter.ts`).
    #[test]
    fn an_and_tree_round_trips_in_the_webviews_shape() {
        let json = r#"{"node":"and","0":[
            {"node":"pred","0":{"column":"a","op":"eq","value":{"t":"text","v":"x"}}},
            {"node":"or","0":[{"node":"pred","0":{"column":"b","op":"is_null","value":{"t":"null"}}}]}
        ]}"#;
        let tree: FilterTree = serde_json::from_str(json).expect("webview shape must parse");
        let FilterTree::And(items) = &tree else { panic!("{tree:?}") };
        assert_eq!(items.len(), 2);
        let back = serde_json::to_string(&tree).expect("and/or must serialize");
        let again: FilterTree = serde_json::from_str(&back).unwrap();
        assert!(matches!(again, FilterTree::And(ref v) if v.len() == 2));
    }
}
