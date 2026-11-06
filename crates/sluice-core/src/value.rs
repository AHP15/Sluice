//! The wire value algebra.
//!
//! Shaped by T-073's findings against a real mongod. The first draft was
//! scalar-only, written from a relational mental model, and the spike broke it
//! in four places; the variants below that look unusual for a SQL tool are the
//! repairs, each annotated with the finding that forced it.

use serde::{Deserialize, Serialize};

/// A single cell value, carrying its own type identity.
///
/// T-073 F-2: type identity lives on the **value**, not the column. A Mongo
/// collection can hold three BSON types in one column of one page, so a single
/// per-column type tag is wrong for some rows by construction.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum WireValue {
    /// SQL NULL, or an explicitly null document field.
    Null,
    /// T-073 F-1: the field was **absent**, which in a document store is
    /// semantically distinct from present-and-null. Collapsing the two at the
    /// contract boundary destroys information the UI needs to edit correctly.
    Missing,
    Bool(bool),
    Int(i64),
    /// Binary float. Inexact by nature — never used for exact numerics.
    Float(f64),
    /// Exact numeric preserved as its literal text (D-006).
    ///
    /// T-072 found SQL Server `MONEY` decodes through `f64` in tiberius, losing
    /// precision before the driver sees it, so the SQL Server driver must emit
    /// `CONVERT(VARCHAR, col, 2)` and carry the result here.
    Numeric(String),
    Text(String),
    /// Binary, **base64-encoded** (D-006, T-016).
    ///
    /// Deliberately a `String` and not `Vec<u8>`: serde would render a byte
    /// vector as a JSON array of integers, which is larger than base64 and
    /// forces the webview to rebuild a buffer per cell. D-006 specifies "binary
    /// as base64" and T-016 requires binary to cross as a *tagged string*.
    Bytes(String),
    /// ISO-8601 text; engines disagree on resolution and offset handling, so the
    /// canonical form is text rather than a parsed instant.
    Timestamp(String),
    Uuid(String),

    /// T-018 / R-008: the value exceeded the per-value byte ceiling and was NOT
    /// inlined.
    ///
    /// Browsing a table holding a 50MB `bytea` is an ordinary action with no
    /// warning attached, and inlining it produces a ~67MB base64 string that has
    /// to be serialized in Rust, parsed in the webview, and held in a grid cell.
    /// The guard therefore lives in the encoder, not in user caution.
    ///
    /// `handle` is an opaque token for fetching the full value on demand; it is
    /// `None` when the driver cannot identify the row well enough to re-fetch,
    /// in which case the UI shows the size but offers no load action rather than
    /// offering one that would fail.
    Oversized {
        type_name: String,
        /// Exact size of the value on the server, in bytes.
        byte_len: u64,
        handle: Option<String>,
    },

    /// T-073 F-3: structural values are first-class.
    ///
    /// Without these a nested document reaches the UI as opaque text and can
    /// only be replaced wholesale, never edited field-wise — and D-018's
    /// schemaless work (T-063/T-064) has no value type to build on.
    Document(Vec<(String, WireValue)>),
    Array(Vec<WireValue>),

    /// T-073 F-4: a value the driver **understands and can round-trip**, but
    /// which has no structural equivalent here — Mongo's `ObjectId` being the
    /// motivating case, since it is the de-facto primary key and mapping it to
    /// `Text` silently destroys the type the edit key filter depends on.
    Opaque { type_name: String, rendered: String },

    /// T-017: a type the driver could **not** decode. Display only; not
    /// editable, not comparable, never written back.
    Unsupported { type_name: String, rendered: String },
}

impl WireValue {
    /// Whether this value may take part in an optimistic-concurrency predicate.
    ///
    /// T-073 F-4: comparability is a property of the **value**, not the column.
    /// Structural values compare by key order in BSON, so equality on a
    /// semantically identical document produces false conflicts.
    pub fn is_comparable(&self) -> bool {
        !matches!(
            self,
            WireValue::Document(_)
                | WireValue::Array(_)
                | WireValue::Unsupported { .. }
                // T-018: the UI never held this value, so it cannot assert it
                // is unchanged. Including it in a concurrency predicate would be
                // comparing against something we never read.
                | WireValue::Oversized { .. }
        )
    }

    /// Coarse class the UI switches on to offer filter operators (T-013).
    pub fn type_class(&self) -> TypeClass {
        match self {
            WireValue::Null | WireValue::Missing => TypeClass::Unknown,
            WireValue::Bool(_) => TypeClass::Boolean,
            WireValue::Int(_) => TypeClass::Integer,
            WireValue::Float(_) => TypeClass::Float,
            WireValue::Numeric(_) => TypeClass::ExactNumeric,
            WireValue::Text(_) => TypeClass::Text,
            WireValue::Bytes(_) => TypeClass::Binary,
            WireValue::Timestamp(_) => TypeClass::Timestamp,
            WireValue::Uuid(_) => TypeClass::Uuid,
            WireValue::Document(_) | WireValue::Array(_) => TypeClass::Structural,
            WireValue::Opaque { .. } => TypeClass::Opaque,
            WireValue::Oversized { .. } => TypeClass::Oversized,
            WireValue::Unsupported { .. } => TypeClass::Unknown,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TypeClass {
    Boolean,
    Integer,
    Float,
    ExactNumeric,
    Text,
    Binary,
    Timestamp,
    Uuid,
    Structural,
    Opaque,
    /// T-018: the value was over the ceiling and is represented by a placeholder.
    Oversized,
    Unknown,
}

/// Describes one column of one **page** of results.
///
/// T-073 F-1: for a schemaless engine the column set is a property of the page,
/// not of the relation — page 2 may legitimately differ from page 1. `derived`
/// tells the UI which situation it is in so it does not cache a column set as
/// though the catalog had guaranteed it.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ColumnDesc {
    pub name: String,
    /// Engine-native type name, verbatim, for display and for T-068's corpus.
    pub type_name: String,
    pub type_class: TypeClass,
    pub nullable: bool,
    /// T-073 F-2: values in this column were **not** all the same type.
    /// The UI must not trust `type_class` for per-row decisions when set.
    pub mixed: bool,
    /// T-073 F-1: inferred by sampling rather than read from a catalog.
    pub derived: bool,
    /// T-019 / R-005: set for exact numerics — arbitrary-precision decimals and
    /// integers wider than f64's exact range — and `None` for everything else.
    ///
    /// D-006 protects these on read by shipping them as tagged strings. The
    /// marker is what keeps the **edit** path equally exact: a column carrying
    /// it gets a text editor that validates the string and never routes it
    /// through a JavaScript number. `type_class` alone is not enough, because a
    /// UI choosing editors by class is precisely where R-005's `parseFloat`
    /// creeps in; the domain also carries what range validation needs.
    #[serde(default)]
    pub exact: Option<ExactDomain>,
    /// T-058 / D-027: the driver vouches that this column's type has a usable
    /// equality, so an edit to it can be checked against its value as read.
    ///
    /// False for types with no equality or an ill-defined one (`json`, `xml`,
    /// floating point) and for any type the driver does not vouch for. An edit
    /// to such a column is protected by the unique key only, and the UI says
    /// so. Defaults to false: a guarantee is never assumed.
    #[serde(default)]
    pub concurrency_comparable: bool,
}

/// The value domain of an exact numeric column, for string-only validation in
/// the cell editor (T-019 / R-005). Bounds are decimal **text**, never numbers:
/// an `i64` bound serialized as a JSON number would itself be rounded by the
/// webview's `JSON.parse`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExactDomain {
    /// A fixed-width integer, inclusive bounds as decimal text.
    Integer { min: String, max: String },
    /// An arbitrary-precision decimal. `None` means the driver does not know the
    /// constraint (it is unconstrained, or not yet read from the catalog — T-053
    /// can refine it); the database remains the final validator either way.
    Decimal { precision: Option<u32>, scale: Option<u32> },
}

impl ExactDomain {
    /// The domain of a signed 64-bit integer column (`int8`, `bigint`).
    pub fn i64() -> Self {
        ExactDomain::Integer { min: i64::MIN.to_string(), max: i64::MAX.to_string() }
    }

    /// A decimal with no known precision or scale.
    pub fn unconstrained_decimal() -> Self {
        ExactDomain::Decimal { precision: None, scale: None }
    }
}
