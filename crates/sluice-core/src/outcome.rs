//! The execution result envelope (T-010, T-011, T-012 / D-029).
//!
//! Every execution returns an **ordered sequence** of statement outcomes, one
//! per statement the engine ran. R-020: a single columns-and-rows shape cannot
//! express DDL, affected-row counts, or multiple result sets, and forcing them
//! into a rows envelope either fabricates empty tables or needs an out-of-band
//! channel the contract never defined.
//!
//! Structured browse always yields exactly one `Rows` outcome, so the grid path
//! costs nothing for the generality.

use serde::{Deserialize, Serialize};

use crate::error::DriverError;
use crate::value::{ColumnDesc, WireValue};

/// How an outcome is tied back to the text the user wrote.
///
/// R-040 is the reason this is an enum rather than a span: mapping outcomes to
/// source spans requires splitting the script client-side, and a subtly wrong
/// split highlights innocent text with full confidence. Attribution is therefore
/// allowed to degrade to an ordinal, and the UI must render `Ordinal` with **no
/// highlighting at all** rather than guessing.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(tag = "how", rename_all = "snake_case")]
pub enum Attribution {
    /// The splitter's statement count agreed with the engine's outcome count.
    /// Offsets are UTF-16 code units into the submitted script, so the webview
    /// can use them directly.
    Span { ordinal: u32, start_utf16: u32, end_utf16: u32 },
    /// The splitter disagreed with the engine, or reported low confidence.
    /// Position is unknown; do not highlight.
    Ordinal { ordinal: u32 },
}

impl Attribution {
    pub fn ordinal(&self) -> u32 {
        match self {
            Attribution::Span { ordinal, .. } | Attribution::Ordinal { ordinal } => *ordinal,
        }
    }
}

/// What one statement produced. D-029's four variants.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutcomeBody {
    Rows {
        /// T-073 F-1: for a driver with `fixed_schema: false` these describe
        /// THIS PAGE, not the relation. Check `ColumnDesc::derived`.
        columns: Vec<ColumnDesc>,
        rows: Vec<Vec<WireValue>>,
        /// A budget stopped the read before the result was exhausted (T-050).
        truncated: bool,
    },
    Affected {
        count: u64,
        /// `INSERT`, `UPDATE`, `DELETE` … as the engine reported it.
        statement_kind: Option<String>,
        /// T-073 F-4: originals the driver could NOT put in the concurrency
        /// predicate. Non-empty means the guarantee was weaker than requested.
        #[serde(default)]
        concurrency_omitted: Vec<String>,
        /// D-017 models partial failure **per statement and per document**.
        /// This is the per-document half: one statement can partially succeed,
        /// with some rows/documents written and others rejected individually.
        ///
        /// Empty on Postgres, where a statement is atomic. It exists now rather
        /// than later because Mongo's bulk writes report exactly this shape and
        /// T-073 established that Mongo's outcome *sequence* is permanently
        /// length 1 — so without a per-document channel inside one outcome,
        /// Mongo would have nowhere to report partial writes at all.
        #[serde(default)]
        document_errors: Vec<DocumentError>,
    },
    /// A single document or message: Mongo command results, DDL acknowledgements.
    Status {
        message: String,
        /// Engine-supplied document, serialized as JSON, when there was one.
        document: Option<String>,
    },
    /// D-017's per-statement partial failure. A script that fails at statement
    /// three returns two successful outcomes and one of these — not one global
    /// failure.
    Error {
        error: DriverError,
    },
}

/// One rejected row/document inside an otherwise-successful statement (D-017).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DocumentError {
    /// Position within the submitted batch, so the UI can point at the input.
    pub index: u64,
    /// The row/document's own identifier, when the engine reports one.
    pub id: Option<String>,
    pub error: DriverError,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Outcome {
    pub attribution: Attribution,
    #[serde(flatten)]
    pub body: OutcomeBody,
}

impl Outcome {
    pub fn is_error(&self) -> bool {
        matches!(self.body, OutcomeBody::Error { .. })
    }

    /// A cancel is not a failure (D-017), so it must not count as one here.
    pub fn is_failure(&self) -> bool {
        match &self.body {
            OutcomeBody::Error { error } => error.is_failure(),
            _ => false,
        }
    }
}

/// Which transaction mode the batch actually ran under.
///
/// R-041: chosen deliberately per execution rather than inherited from the
/// driver default, because the same rendered result means different things
/// depending on this value.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TxMode {
    /// The engine wrapped the whole batch in one implicit transaction —
    /// Postgres' behaviour for a multi-statement simple query.
    ImplicitSingle,
    /// The user's own `BEGIN` was in force, on a pinned session (T-041).
    ExplicitUser,
    /// Each statement committed independently as it ran.
    AutocommitPerStatement,
}

/// What survived.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TxDisposition {
    /// Everything reported above is durable.
    Committed,
    /// The listed outcome ordinals were undone despite reporting success.
    /// This is the case R-041 exists for.
    RolledBack { rolled_back: Vec<u32> },
    /// Still open on a pinned session; durability is not yet decided.
    Open,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TxContext {
    pub mode: TxMode,
    pub disposition: TxDisposition,
}

impl TxContext {
    pub fn autocommit() -> Self {
        Self { mode: TxMode::AutocommitPerStatement, disposition: TxDisposition::Committed }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ExecutionResult {
    pub outcomes: Vec<Outcome>,
    pub tx: TxContext,
    /// T-047 / D-010: for a structured browse, how it paged and where the next
    /// page starts. `None` for raw scripts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<PageInfo>,
}

/// How a browse paginated, declared by the driver (T-047 / D-010).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum PageInfo {
    /// Keyset on `key` (the requested sort, then the primary key as a
    /// tiebreaker). Pass `next_cursor` as the next request's `cursor`.
    Keyset { key: Vec<String>, next_cursor: Option<Vec<WireValue>> },
    /// LIMIT/OFFSET, and why keyset was not possible. Pass `next_offset` as
    /// the next request's `offset`. Pages can shift if rows change between
    /// requests — the reason keyset is preferred.
    Offset { reason: String, next_offset: Option<u64> },
}

impl ExecutionResult {
    /// A single `Rows` outcome — the browse path (T-010).
    pub fn single_rows(columns: Vec<ColumnDesc>, rows: Vec<Vec<WireValue>>, truncated: bool) -> Self {
        Self {
            outcomes: vec![Outcome {
                attribution: Attribution::Ordinal { ordinal: 0 },
                body: OutcomeBody::Rows { columns, rows, truncated },
            }],
            tx: TxContext::autocommit(),
            page: None,
        }
    }

    pub fn first_rows(&self) -> Option<(&Vec<ColumnDesc>, &Vec<Vec<WireValue>>)> {
        self.outcomes.iter().find_map(|o| match &o.body {
            OutcomeBody::Rows { columns, rows, .. } => Some((columns, rows)),
            _ => None,
        })
    }

    /// The editor's one-line summary.
    ///
    /// R-041 requires `statement 3 failed; statements 1-2 rolled back` to be
    /// distinguishable at a glance from `... committed`, because the rendered
    /// outcome sequence is identical in both cases.
    ///
    /// R-041 also says an envelope that cannot state which happened "is not
    /// renderable and should fail loudly in development": a rolled-back
    /// disposition that does not name what it undid trips a debug assertion
    /// rather than rendering an ambiguous line.
    pub fn summary(&self) -> String {
        let failed: Vec<u32> = self
            .outcomes
            .iter()
            .filter(|o| o.is_failure())
            .map(|o| o.attribution.ordinal() + 1)
            .collect();

        let cancelled = self
            .outcomes
            .iter()
            .any(|o| matches!(&o.body, OutcomeBody::Error { error } if !error.is_failure()));

        let succeeded: Vec<u32> = self
            .outcomes
            .iter()
            .filter(|o| !o.is_error())
            .map(|o| o.attribution.ordinal() + 1)
            .collect();

        if failed.is_empty() {
            let n = succeeded.len();
            let base = match (&self.tx.disposition, cancelled) {
                (_, true) => format!("{n} statement(s) ran; execution cancelled"),
                (TxDisposition::Open, _) => {
                    format!("{n} statement(s) succeeded; transaction still open")
                }
                (TxDisposition::RolledBack { .. }, _) => {
                    format!("{n} statement(s) reported success but were rolled back")
                }
                (TxDisposition::Committed, _) => format!("{n} statement(s) committed"),
            };
            return base;
        }

        let failed_txt = join_ranges(&failed);
        match &self.tx.disposition {
            TxDisposition::RolledBack { rolled_back } => {
                debug_assert!(
                    !rolled_back.is_empty() || succeeded.is_empty(),
                    "R-041: a RolledBack disposition must name what it undid, \
                     otherwise the summary is ambiguous"
                );
                // The disposition is authoritative about what was undone, NOT
                // the outcome list: a driver may know statements ran and were
                // rolled back without being able to report their outcomes (the
                // Postgres simple-query path is exactly this case). Deriving the
                // undone set from `succeeded` would silently say "nothing
                // committed" for a batch that in fact undid work.
                if rolled_back.is_empty() {
                    format!("statement {failed_txt} failed; nothing committed")
                } else {
                    let undone: Vec<u32> = rolled_back.iter().map(|o| o + 1).collect();
                    format!(
                        "statement {failed_txt} failed; statements {} rolled back",
                        join_ranges(&undone)
                    )
                }
            }
            TxDisposition::Committed => {
                if succeeded.is_empty() {
                    format!("statement {failed_txt} failed")
                } else {
                    format!(
                        "statement {failed_txt} failed; statements {} committed",
                        join_ranges(&succeeded)
                    )
                }
            }
            TxDisposition::Open => format!(
                "statement {failed_txt} failed; transaction still open, nothing decided yet"
            ),
        }
    }
}

/// `[1,2,3,5]` -> `"1-3, 5"`.
fn join_ranges(items: &[u32]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut start = items[0];
    let mut prev = items[0];
    for &n in &items[1..] {
        if n == prev + 1 {
            prev = n;
            continue;
        }
        out.push(fmt_range(start, prev));
        start = n;
        prev = n;
    }
    out.push(fmt_range(start, prev));
    out.join(", ")
}

fn fmt_range(a: u32, b: u32) -> String {
    if a == b {
        a.to_string()
    } else {
        format!("{a}-{b}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    fn ok_outcome(ordinal: u32) -> Outcome {
        Outcome {
            attribution: Attribution::Ordinal { ordinal },
            body: OutcomeBody::Affected {
                count: 1,
                statement_kind: Some("UPDATE".into()),
                concurrency_omitted: vec![],
                document_errors: vec![],
            },
        }
    }

    fn err_outcome(ordinal: u32) -> Outcome {
        Outcome {
            attribution: Attribution::Ordinal { ordinal },
            body: OutcomeBody::Error {
                error: DriverError::new(ErrorKind::SyntaxError, "boom"),
            },
        }
    }

    /// The exact distinction R-041 exists to make. Identical outcome sequences,
    /// different transactional context, and the summaries MUST differ.
    #[test]
    fn rolled_back_and_committed_summaries_are_distinguishable() {
        let outcomes = vec![ok_outcome(0), ok_outcome(1), err_outcome(2)];

        let rolled = ExecutionResult {
            outcomes: outcomes.clone(),
            tx: TxContext {
                mode: TxMode::ImplicitSingle,
                disposition: TxDisposition::RolledBack { rolled_back: vec![0, 1] },
            },
            page: None,
        };
        let committed = ExecutionResult {
            outcomes,
            tx: TxContext {
                mode: TxMode::AutocommitPerStatement,
                disposition: TxDisposition::Committed,
            },
            page: None,
        };

        assert_eq!(rolled.summary(), "statement 3 failed; statements 1-2 rolled back");
        assert_eq!(committed.summary(), "statement 3 failed; statements 1-2 committed");
        assert_ne!(rolled.summary(), committed.summary());
    }

    #[test]
    fn browse_produces_exactly_one_rows_outcome() {
        let r = ExecutionResult::single_rows(vec![], vec![], false);
        assert_eq!(r.outcomes.len(), 1);
        assert!(matches!(r.outcomes[0].body, OutcomeBody::Rows { .. }));
    }

    #[test]
    fn cancellation_is_not_reported_as_a_failed_statement() {
        let r = ExecutionResult {
            outcomes: vec![
                ok_outcome(0),
                Outcome {
                    attribution: Attribution::Ordinal { ordinal: 1 },
                    body: OutcomeBody::Error { error: DriverError::cancelled() },
                },
            ],
            tx: TxContext::autocommit(),
            page: None,
        };
        assert!(r.summary().contains("cancelled"));
        assert!(!r.summary().contains("failed"), "summary: {}", r.summary());
    }

    #[test]
    fn range_joining_is_readable() {
        assert_eq!(join_ranges(&[1]), "1");
        assert_eq!(join_ranges(&[1, 2, 3]), "1-3");
        assert_eq!(join_ranges(&[1, 2, 5]), "1-2, 5");
        assert_eq!(join_ranges(&[1, 3, 5]), "1, 3, 5");
    }

    #[test]
    fn ordinal_attribution_carries_no_span_for_the_ui_to_highlight() {
        let a = Attribution::Ordinal { ordinal: 4 };
        let json = serde_json::to_string(&a).unwrap();
        assert!(!json.contains("start_utf16"), "{json}");
        assert_eq!(a.ordinal(), 4);
    }
}
