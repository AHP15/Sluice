//! Compile a structured browse into parameterised T-SQL (T-082 / D-029,
//! D-005, D-006, D-010), and decode what comes back losslessly.
//!
//! The UI sends a relation, a filter tree, a sort and a page position; this
//! module is the only place that becomes T-SQL. Three rules shape it:
//!
//! - **Values are always bound** (`@P1`, `@P2` …), never interpolated (D-005).
//!   Identifiers are bracket-quoted with `]` doubled. The only things written
//!   into the statement text are identifiers, integers the compiler owns
//!   (`TOP`, `OFFSET`, the size ceiling) and fixed SQL.
//! - **Nothing reaches tiberius's decoder in a form it would damage or panic
//!   on** (T-072). The projection is written per column from the catalog's
//!   declared type, never `SELECT *`:
//!   - `money`/`smallmoney` decode through `f64` in tiberius and are provably
//!     lossy, so they are selected as `CONVERT(VARCHAR(64), col, 2)` and cross
//!     as exact `Numeric` text; `decimal`/`numeric` likewise as text, and
//!     `bigint` (beyond f64's exact range) as `Numeric` text of its `i64`;
//!   - `sql_variant`, `geography` and `hierarchyid` hit a `todo!()` inside
//!     tiberius and **panic**. They are converted to text server-side
//!     (`CONVERT(nvarchar(4000), col)`, `col.ToString()`) and surface as
//!     `Unsupported`, as does every CLR type; a type this module does not
//!     know is never selected at all — only whether it is NULL — and
//!     surfaces as `Unsupported` with its type name;
//!   - temporals are rendered by the server in ISO 8601 (style 126), keeping
//!     offsets and all seven fractional digits.
//! - **The per-value ceiling is applied in SQL** (T-018 / R-008): a column
//!   that can be large gets a `DATALENGTH` companion, and its value is NULLed
//!   server-side when over the ceiling, so the bytes are never sent; the
//!   decoder turns that into `Oversized`.
//!
//! Paging follows D-010: keyset on the primary key (the requested sort, then
//! the key as tiebreaker) with `TOP`; views, keyless tables and sorts the
//! keyset cannot honour use `OFFSET … FETCH`, and the driver declares which
//! it used and why.

use base64::Engine as _;
use sluice_core::{
    BrowseRequest, ColumnDesc, DriverError, ErrorKind, ExactDomain, FilterTree, NodeId, PageInfo, Predicate,
    TypeClass, WireValue,
};
use tiberius::ColumnData;

use crate::navigator::quote_ident;

/// Default per-value ceiling. R-008 asks for "the low hundreds of KB"; the
/// same figure as the Postgres driver.
pub const DEFAULT_VALUE_CEILING: i64 = 256 * 1024;

/// SQL Server allows 2100 parameters per request; keep clear of it with a
/// message rather than a server error.
const MAX_PARAMS: usize = 2000;

/// The page size is the compiler's to bound, not the request's.
const MAX_LIMIT: u32 = 100_000;

// ---- the relation, as the catalog declares it --------------------------------

/// One column as `sys.columns` / `sys.types` declare it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// The declared (user) type name: `nvarchar`, `money`, `geography`, an
    /// alias type's own name.
    pub type_name: String,
    /// The system type it is stored as (`nvarchar` for `sysname`), or `None`
    /// for CLR types, which have no system base.
    pub base_type: Option<String>,
    /// A CLR type (`geography`, `geometry`, `hierarchyid`, user assemblies).
    pub assembly: bool,
    /// Bytes; -1 for `(max)` types.
    pub max_length: i16,
    pub precision: u8,
    pub scale: u8,
    pub nullable: bool,
    /// 1-based position in the primary key, when part of it.
    pub pk_ordinal: Option<u8>,
}

impl ColumnInfo {
    pub fn kind(&self) -> Kind {
        classify(self)
    }
}

/// What the catalog says about one relation.
#[derive(Clone, Debug, Default)]
pub struct RelationShape {
    /// In column order.
    pub columns: Vec<ColumnInfo>,
    /// A base table (`U`), as opposed to a view.
    pub is_table: bool,
}

impl RelationShape {
    /// Primary-key columns in key order.
    pub fn primary_key(&self) -> Vec<&ColumnInfo> {
        let mut pk: Vec<&ColumnInfo> = self.columns.iter().filter(|c| c.pk_ordinal.is_some()).collect();
        pk.sort_by_key(|c| c.pk_ordinal);
        pk
    }

    fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.columns.iter().find(|c| c.name == name)
    }
}

/// The catalog read, through a three-part name so any database the login can
/// open is browsable without reconnecting. Schema and relation are bound;
/// only the database is an identifier.
pub fn shape_sql(db: &str) -> String {
    let d = quote_ident(db);
    format!(
        "SELECT CAST(c.name AS nvarchar(128)), CAST(t.name AS nvarchar(128)), CAST(bt.name AS nvarchar(128)),
                t.is_assembly_type, c.max_length, c.precision, c.scale, c.is_nullable,
                CAST(pk.key_ordinal AS int), CAST(o.type AS nvarchar(2))
         FROM {d}.sys.objects o
         JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id
         JOIN {d}.sys.columns c ON c.object_id = o.object_id
         JOIN {d}.sys.types t ON t.user_type_id = c.user_type_id
         LEFT JOIN {d}.sys.types bt ON bt.user_type_id = c.system_type_id AND bt.is_user_defined = 0
         OUTER APPLY (
             SELECT ic.key_ordinal
             FROM {d}.sys.indexes i
             JOIN {d}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
             WHERE i.object_id = o.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id
         ) pk
         WHERE s.name = @P1 AND o.name = @P2 AND o.type IN ('U', 'V')
         ORDER BY c.column_id"
    )
}

/// `[db].[schema].[relation]` from a navigator path.
pub fn qualified(relation: &NodeId) -> Result<(String, String, String), DriverError> {
    match relation.path.as_slice() {
        [db, schema, rel] => Ok((db.clone(), schema.clone(), rel.clone())),
        other => Err(DriverError::new(ErrorKind::NotFound, format!("expected [database, schema, relation], got {other:?}"))),
    }
}

fn qualified_sql(relation: &NodeId) -> Result<String, DriverError> {
    let (db, schema, rel) = qualified(relation)?;
    Ok(format!("{}.{}.{}", quote_ident(&db), quote_ident(&schema), quote_ident(&rel)))
}

// ---- type classification ------------------------------------------------------

/// Temporal flavours; each is rendered by the server and bound back by its
/// own rule (see [`bind_cast`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Temporal {
    Date,
    Time,
    DateTime,
    SmallDateTime,
    DateTime2,
    DateTimeOffset,
}

/// How a SQL Server type is selected, decoded and bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bit,
    /// `tinyint`, `smallint`, `int`: exact in f64, so raw (D-006).
    SmallInt,
    /// `bigint`: beyond f64's exact range, so `Numeric` text.
    BigInt,
    /// `decimal`/`numeric`, selected as text.
    Decimal,
    /// `money`/`smallmoney`, selected with `CONVERT(VARCHAR(64), col, 2)`.
    Money,
    Float,
    Real,
    /// `char`/`varchar`/`nchar`/`nvarchar`; `unicode` is the n-variants.
    Str { unicode: bool },
    /// The deprecated `text`/`ntext`, cast to `nvarchar(max)`.
    LegacyText,
    Xml,
    /// `binary`/`varbinary`/`timestamp` (rowversion).
    Binary,
    /// The deprecated `image`, cast to `varbinary(max)`.
    Image,
    Uuid,
    Temporal(Temporal),
    /// `sql_variant`: panics in tiberius, converted to text server-side.
    Variant,
    /// `geography`, `geometry`, `hierarchyid` and user CLR types: rendered
    /// with `.ToString()` server-side, never decoded raw.
    Clr,
    /// Anything else. Never selected — only its NULL-ness.
    Unknown,
}

pub fn classify(c: &ColumnInfo) -> Kind {
    if c.assembly {
        return Kind::Clr;
    }
    let base = c.base_type.as_deref().unwrap_or(&c.type_name).to_ascii_lowercase();
    match base.as_str() {
        "bit" => Kind::Bit,
        "tinyint" | "smallint" | "int" => Kind::SmallInt,
        "bigint" => Kind::BigInt,
        "decimal" | "numeric" => Kind::Decimal,
        "money" | "smallmoney" => Kind::Money,
        "float" => Kind::Float,
        "real" => Kind::Real,
        "char" | "varchar" => Kind::Str { unicode: false },
        "nchar" | "nvarchar" => Kind::Str { unicode: true },
        "text" | "ntext" => Kind::LegacyText,
        "xml" => Kind::Xml,
        "binary" | "varbinary" | "timestamp" => Kind::Binary,
        "image" => Kind::Image,
        "uniqueidentifier" => Kind::Uuid,
        "date" => Kind::Temporal(Temporal::Date),
        "time" => Kind::Temporal(Temporal::Time),
        "datetime" => Kind::Temporal(Temporal::DateTime),
        "smalldatetime" => Kind::Temporal(Temporal::SmallDateTime),
        "datetime2" => Kind::Temporal(Temporal::DateTime2),
        "datetimeoffset" => Kind::Temporal(Temporal::DateTimeOffset),
        "sql_variant" => Kind::Variant,
        // System CLR types reached without the assembly flag (defensive).
        "geography" | "geometry" | "hierarchyid" => Kind::Clr,
        _ => Kind::Unknown,
    }
}

impl Kind {
    pub fn type_class(self) -> TypeClass {
        match self {
            Kind::Bit => TypeClass::Boolean,
            Kind::SmallInt => TypeClass::Integer,
            Kind::BigInt | Kind::Decimal | Kind::Money => TypeClass::ExactNumeric,
            Kind::Float | Kind::Real => TypeClass::Float,
            Kind::Str { .. } | Kind::LegacyText | Kind::Xml => TypeClass::Text,
            Kind::Binary | Kind::Image => TypeClass::Binary,
            Kind::Uuid => TypeClass::Uuid,
            Kind::Temporal(_) => TypeClass::Timestamp,
            // Their values cross as `Unsupported`, whose class is Unknown.
            Kind::Variant | Kind::Clr | Kind::Unknown => TypeClass::Unknown,
        }
    }

    /// Values of this kind can be bound back as a parameter (filters and
    /// keyset cursors).
    fn bindable(self) -> bool {
        !matches!(self, Kind::Variant | Kind::Clr | Kind::Unknown)
    }

    /// SQL Server can `ORDER BY` this kind.
    fn sortable(self) -> bool {
        !matches!(self, Kind::LegacyText | Kind::Xml | Kind::Image | Kind::Clr | Kind::Unknown)
    }

    fn is_text(self) -> bool {
        matches!(self, Kind::Str { .. } | Kind::LegacyText | Kind::Xml)
    }
}

pub fn column_desc(c: &ColumnInfo) -> ColumnDesc {
    let kind = c.kind();
    ColumnDesc {
        name: c.name.clone(),
        type_name: c.type_name.clone(),
        type_class: kind.type_class(),
        nullable: c.nullable,
        mixed: false,
        derived: false,
        exact: match kind {
            Kind::BigInt => Some(ExactDomain::i64()),
            // The catalog carries precision and scale (money is decimal(19,4),
            // smallmoney decimal(10,4)), unlike a Postgres prepared statement.
            Kind::Decimal | Kind::Money => {
                Some(ExactDomain::Decimal { precision: Some(u32::from(c.precision)), scale: Some(u32::from(c.scale)) })
            }
            _ => None,
        },
        // T-058: an allowlist — types with an exact equality whose text form
        // round-trips. Floats, LOBs without equality, and anything shipped as
        // `Unsupported` are excluded.
        concurrency_comparable: matches!(
            kind,
            Kind::Bit
                | Kind::SmallInt
                | Kind::BigInt
                | Kind::Decimal
                | Kind::Money
                | Kind::Str { .. }
                | Kind::Binary
                | Kind::Uuid
                | Kind::Temporal(_)
        ),
    }
}

// ---- projection ---------------------------------------------------------------

/// Whether a column needs the T-018 ceiling: anything whose value can exceed
/// it — `(max)` types, the legacy LOBs, `xml`, CLR renderings, and a fixed
/// width declared above a (low) configured ceiling.
fn needs_ceiling(c: &ColumnInfo, kind: Kind, ceiling: i64) -> bool {
    match kind {
        Kind::LegacyText | Kind::Xml | Kind::Image | Kind::Clr => true,
        Kind::Str { .. } | Kind::Binary => c.max_length == -1 || i64::from(c.max_length) > ceiling,
        // sql_variant is at most 8016 bytes; rendered into nvarchar(4000).
        Kind::Variant => 8000 > ceiling,
        _ => false,
    }
}

/// The expression that renders one column for the wire.
fn render_expr(ident: &str, kind: Kind) -> String {
    match kind {
        Kind::Bit | Kind::SmallInt | Kind::BigInt | Kind::Float | Kind::Real | Kind::Str { .. } | Kind::Binary => {
            ident.to_string()
        }
        // Exact decimal text, never through tiberius's Numeric/f64 path.
        Kind::Decimal => format!("CONVERT(VARCHAR(64), {ident})"),
        // T-072: tiberius decodes MONEY through f64 (922337203685477.5807
        // came back 922337203685477.6). Style 2: four decimals, no commas.
        Kind::Money => format!("CONVERT(VARCHAR(64), {ident}, 2)"),
        Kind::LegacyText | Kind::Xml => format!("CAST({ident} AS nvarchar(max))"),
        Kind::Image => format!("CAST({ident} AS varbinary(max))"),
        Kind::Uuid => format!("CONVERT(CHAR(36), {ident})"),
        // ISO 8601: language- and DATEFORMAT-independent, with the offset and
        // every fractional digit the type stores.
        Kind::Temporal(_) => format!("CONVERT(VARCHAR(40), {ident}, 126)"),
        // T-072: these panic inside tiberius's decoder. Text, server-side.
        Kind::Variant => format!("CONVERT(nvarchar(4000), {ident})"),
        Kind::Clr => format!("{ident}.ToString()"),
        // Never selected: only whether it is NULL.
        Kind::Unknown => format!("CASE WHEN {ident} IS NULL THEN NULL ELSE N'' END"),
    }
}

/// The exact byte length on the server, as `bigint` (DATALENGTH is `int`
/// for most types and `bigint` for `(max)` ones; tiberius will not widen).
fn length_expr(ident: &str, kind: Kind) -> String {
    match kind {
        // The size of what would be shipped, not of the CLR serialization.
        Kind::Clr | Kind::Variant => format!("CAST(DATALENGTH({}) AS bigint)", render_expr(ident, kind)),
        _ => format!("CAST(DATALENGTH({ident}) AS bigint)"),
    }
}

/// Where each column's value (and optional length) sits in the projection.
#[derive(Clone, Debug)]
pub struct ColumnPlan {
    pub name: String,
    pub type_name: String,
    pub kind: Kind,
    pub value_idx: usize,
    pub len_idx: Option<usize>,
    pub desc: ColumnDesc,
}

// ---- parameters ---------------------------------------------------------------

/// One bound parameter. Text crosses as `nvarchar` and is cast in SQL to the
/// column's type, so an exact value never passes through a float; binary is
/// decoded from base64 here and bound as `varbinary`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Param {
    Text(String),
    Bytes(Vec<u8>),
}

impl Param {
    pub fn as_sql(&self) -> &dyn tiberius::ToSql {
        match self {
            Param::Text(s) => s,
            Param::Bytes(b) => b,
        }
    }
}

/// Filters compare with Unicode semantics; keyset cursors must compare the
/// way the column sorts (see [`bind_cast`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ctx {
    Filter,
    Cursor,
}

/// `@Pn`, cast to what the column compares against.
///
/// The cast target *widens* where narrowing would round a filter value and
/// so change a predicate's meaning (`price > 1.23456` must not become
/// `price > 1.2346`): integers to `bigint`, decimals to `decimal(38, s')`
/// keeping the column's integer digits, `datetime2`/`time`/`datetimeoffset`
/// to scale 7. `datetime` is the exception: converting it to `datetime2`
/// exposes its 1/300 s ticks (.003 becomes .0033333), so a value as read
/// would no longer equal itself; it binds as `datetime`.
fn bind_cast(n: usize, c: &ColumnInfo, kind: Kind, ctx: Ctx) -> String {
    let p = format!("@P{n}");
    match kind {
        Kind::Bit => format!("CAST({p} AS bit)"),
        Kind::SmallInt | Kind::BigInt => format!("CAST({p} AS bigint)"),
        Kind::Decimal | Kind::Money => {
            let (prec, scale) = wide_decimal(c);
            format!("CAST({p} AS decimal({prec}, {scale}))")
        }
        Kind::Float | Kind::Real => format!("CAST({p} AS float)"),
        // A varchar column compared to an nvarchar parameter is converted to
        // nvarchar, and under a SQL_ collation Unicode ordering differs from
        // the column's own sort order — fine for a filter's meaning, wrong for
        // a cursor, which must compare exactly as ORDER BY sorts. A cursor
        // value was read from the column, so it is representable in it.
        Kind::Str { unicode: false } if ctx == Ctx::Cursor => format!("CAST({p} AS varchar(max))"),
        Kind::Str { .. } | Kind::LegacyText | Kind::Xml | Kind::Binary | Kind::Image => p,
        Kind::Uuid => format!("CAST({p} AS uniqueidentifier)"),
        Kind::Temporal(t) => match t {
            Temporal::Date => format!("CAST({p} AS date)"),
            Temporal::Time => format!("CAST({p} AS time(7))"),
            Temporal::DateTime => format!("CAST({p} AS datetime)"),
            Temporal::SmallDateTime | Temporal::DateTime2 => format!("CAST({p} AS datetime2(7))"),
            Temporal::DateTimeOffset => format!("CAST({p} AS datetimeoffset(7))"),
        },
        Kind::Variant | Kind::Clr | Kind::Unknown => unreachable!("guarded by Kind::bindable"),
    }
}

/// `decimal(38, s')` with the column's integer digits and as much scale as
/// remains.
fn wide_decimal(c: &ColumnInfo) -> (u8, u8) {
    let int_digits = c.precision.saturating_sub(c.scale).min(38);
    (38, 38 - int_digits)
}

/// The left-hand side a predicate compares: the column itself, except for
/// the legacy LOBs and `xml`, which have no comparison operators.
fn lhs(ident: &str, kind: Kind) -> String {
    match kind {
        Kind::LegacyText | Kind::Xml => format!("CAST({ident} AS nvarchar(max))"),
        Kind::Image => format!("CAST({ident} AS varbinary(max))"),
        _ => ident.to_string(),
    }
}

fn invalid(msg: impl Into<String>) -> DriverError {
    DriverError::new(ErrorKind::Unknown, msg)
}

/// A wire value as the parameter that binds it to `c`.
fn to_param(c: &ColumnInfo, kind: Kind, v: &WireValue) -> Result<Param, DriverError> {
    if matches!(kind, Kind::Binary | Kind::Image) {
        let b64 = match v {
            WireValue::Bytes(s) | WireValue::Text(s) => s,
            other => return Err(invalid(format!("`{}` is binary; expected a base64 value, got {other:?}", c.name))),
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|e| invalid(format!("`{}` is binary; the value is not valid base64 ({e})", c.name)))?;
        return Ok(Param::Bytes(bytes));
    }
    let text = match v {
        WireValue::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        WireValue::Int(i) => i.to_string(),
        // Rust's f64 Display is the shortest text that round-trips.
        WireValue::Float(f) => f.to_string(),
        WireValue::Numeric(s) | WireValue::Text(s) | WireValue::Timestamp(s) | WireValue::Uuid(s) | WireValue::Bytes(s) => {
            s.clone()
        }
        WireValue::Opaque { rendered, .. } => rendered.clone(),
        other => return Err(invalid(format!("a {other:?} value cannot be compared with `{}`", c.name))),
    };
    if matches!(kind, Kind::Decimal | Kind::Money) {
        check_scale(c, &text)?;
    }
    Ok(Param::Text(text))
}

/// Refuse a decimal value with more fractional digits than the bind target
/// holds: SQL Server would round it silently, and a rounded bound changes
/// what the predicate matches.
fn check_scale(c: &ColumnInfo, text: &str) -> Result<(), DriverError> {
    let t = text.trim().trim_start_matches(['+', '-']);
    let (int, frac) = t.split_once('.').unwrap_or((t, ""));
    if !int.chars().all(|ch| ch.is_ascii_digit()) || !frac.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(()); // not plain decimal text; the server's conversion decides
    }
    let digits = frac.trim_end_matches('0').len();
    let (_, scale) = wide_decimal(c);
    if digits > usize::from(scale) {
        return Err(invalid(format!(
            "`{text}` has more decimal places than `{}` ({}) can compare exactly",
            c.name, c.type_name
        )));
    }
    Ok(())
}

// ---- compilation --------------------------------------------------------------

/// The pagination plan the compiler chose (D-010).
#[derive(Clone, Debug, PartialEq)]
pub enum Paging {
    /// Ordered by `key` (the requested sort, then the primary key); each entry
    /// is (column, descending).
    Keyset { key: Vec<(String, bool)> },
    Offset { reason: String, offset: u64 },
}

impl Paging {
    /// The declared strategy and the next page's position, for a page of
    /// `rows` rows ending in `last`.
    pub fn page_info(&self, columns: &[ColumnPlan], last: Option<&[WireValue]>, rows: usize, limit: u32) -> PageInfo {
        let full = rows as u64 >= u64::from(limit);
        match self {
            Paging::Keyset { key } => {
                let next_cursor = full.then_some(last).flatten().and_then(|row| {
                    key.iter()
                        .map(|(col, _)| {
                            let i = columns.iter().position(|c| &c.name == col)?;
                            let v = row.get(i)?.clone();
                            // A cursor value must bind back exactly.
                            (!matches!(v, WireValue::Oversized { .. } | WireValue::Null | WireValue::Unsupported { .. }))
                                .then_some(v)
                        })
                        .collect::<Option<Vec<_>>>()
                });
                PageInfo::Keyset { key: key.iter().map(|(c, _)| c.clone()).collect(), next_cursor }
            }
            Paging::Offset { reason, offset } => {
                PageInfo::Offset { reason: reason.clone(), next_offset: full.then(|| offset + rows as u64) }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompiledBrowse {
    pub sql: String,
    pub params: Vec<Param>,
    pub columns: Vec<ColumnPlan>,
    pub paging: Paging,
    pub ceiling: i64,
    /// The page size as compiled (the request's, clamped).
    pub limit: u32,
}

impl CompiledBrowse {
    pub fn column_descs(&self) -> Vec<ColumnDesc> {
        self.columns.iter().map(|c| c.desc.clone()).collect()
    }

    pub fn sql_params(&self) -> Vec<&dyn tiberius::ToSql> {
        self.params.iter().map(Param::as_sql).collect()
    }
}

pub fn compile(relation: &NodeId, shape: &RelationShape, req: &BrowseRequest, ceiling: i64) -> Result<CompiledBrowse, DriverError> {
    let rel = qualified_sql(relation)?;
    if shape.columns.is_empty() {
        return Err(DriverError::new(ErrorKind::NotFound, format!("relation {rel} was not found, or exposes no columns to this login")));
    }

    let mut select = Vec::new();
    let mut plans = Vec::with_capacity(shape.columns.len());
    for c in &shape.columns {
        let kind = c.kind();
        let ident = quote_ident(&c.name);
        let rendered = render_expr(&ident, kind);
        let huge = needs_ceiling(c, kind, ceiling);
        let value_idx = select.len();
        if huge {
            let len = length_expr(&ident, kind);
            select.push(format!("CASE WHEN {len} > {ceiling} THEN NULL ELSE {rendered} END"));
        } else {
            select.push(rendered);
        }
        let len_idx = huge.then(|| {
            select.push(length_expr(&ident, kind));
            select.len() - 1
        });
        plans.push(ColumnPlan { name: c.name.clone(), type_name: c.type_name.clone(), kind, value_idx, len_idx, desc: column_desc(c) });
    }

    let mut params = Vec::new();
    let mut where_parts = Vec::new();
    if let Some(filter) = &req.filter {
        if let Some(sql) = compile_filter(filter, shape, &mut params)? {
            where_parts.push(sql);
        }
    }

    for sk in &req.sort {
        let c = shape.column(&sk.column).ok_or_else(|| {
            DriverError::new(ErrorKind::NotFound, format!("sort column `{}` not found in relation", sk.column))
        })?;
        if !c.kind().sortable() {
            return Err(invalid(format!("`{}` ({}) cannot be sorted on SQL Server", c.name, c.type_name)));
        }
    }

    let paging = plan_paging(req, shape);
    let order: Vec<(String, bool)> = match &paging {
        Paging::Keyset { key } => key.clone(),
        Paging::Offset { .. } => {
            // Still make the order as total as the relation allows, so offset
            // pages are at least stable when nothing changes.
            let mut o: Vec<(String, bool)> = req.sort.iter().map(|s| (s.column.clone(), s.descending)).collect();
            for pk in shape.primary_key() {
                if !o.iter().any(|(c, _)| c == &pk.name) {
                    o.push((pk.name.clone(), false));
                }
            }
            o
        }
    };

    match (&paging, &req.cursor) {
        (Paging::Keyset { key }, Some(cursor)) => {
            if cursor.len() != key.len() {
                return Err(invalid(format!(
                    "this relation pages on {} key column(s); the cursor has {}",
                    key.len(),
                    cursor.len()
                )));
            }
            where_parts.push(keyset_predicate(key, cursor, shape, &mut params)?);
        }
        (Paging::Offset { .. }, Some(_)) => return Err(invalid("this relation pages by offset, not by keyset cursor")),
        _ => {}
    }

    if params.len() > MAX_PARAMS {
        return Err(invalid(format!(
            "this filter needs {} values; SQL Server accepts at most 2100 parameters per query",
            params.len()
        )));
    }

    let limit = req.limit.clamp(1, MAX_LIMIT);
    let top = if matches!(paging, Paging::Keyset { .. }) { format!("TOP ({limit}) ") } else { String::new() };
    let mut sql = format!("SELECT {top}{} FROM {rel}", select.join(", "));
    if !where_parts.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_parts.join(" AND "));
    }
    let order_sql = if order.is_empty() {
        // OFFSET needs an ORDER BY; a keyless relation with no sort has none.
        "(SELECT NULL)".to_string()
    } else {
        order
            .iter()
            .map(|(c, desc)| format!("{} {}", quote_ident(c), if *desc { "DESC" } else { "ASC" }))
            .collect::<Vec<_>>()
            .join(", ")
    };
    sql.push_str(" ORDER BY ");
    sql.push_str(&order_sql);
    if let Paging::Offset { offset, .. } = &paging {
        sql.push_str(&format!(" OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"));
    }

    Ok(CompiledBrowse { sql, params, columns: plans, paging, ceiling, limit })
}

/// Keyset when the relation is a table with a primary key, every sort column
/// is NOT NULL, and every key column can be bound back; otherwise offset,
/// with the reason the UI shows.
fn plan_paging(req: &BrowseRequest, shape: &RelationShape) -> Paging {
    let offset = req.offset.unwrap_or(0);
    let pk = shape.primary_key();
    if !shape.is_table || pk.is_empty() {
        return Paging::Offset {
            reason: "the relation has no primary key (views and keyless tables page by offset)".into(),
            offset,
        };
    }
    // NULL ordering breaks the `>` comparisons a keyset relies on.
    if let Some(s) = req.sort.iter().find(|s| shape.column(&s.column).is_none_or(|c| c.nullable)) {
        return Paging::Offset { reason: format!("sorting on `{}`, which can be NULL", s.column), offset };
    }
    let mut key: Vec<(String, bool)> = req.sort.iter().map(|s| (s.column.clone(), s.descending)).collect();
    for c in pk {
        if !key.iter().any(|(k, _)| k == &c.name) {
            key.push((c.name.clone(), false));
        }
    }
    if let Some((col, _)) = key.iter().find(|(k, _)| shape.column(k).is_none_or(|c| !c.kind().bindable())) {
        return Paging::Offset { reason: format!("`{col}` cannot be used as a page cursor"), offset };
    }
    Paging::Keyset { key }
}

/// "After the cursor" in a mixed-direction total order, as an OR-chain:
/// `(k1 > v1) OR (k1 = v1 AND k2 > v2) OR …`, `<` for descending keys.
fn keyset_predicate(
    key: &[(String, bool)],
    cursor: &[WireValue],
    shape: &RelationShape,
    params: &mut Vec<Param>,
) -> Result<String, DriverError> {
    let mut binds = Vec::with_capacity(key.len());
    for ((col, _), v) in key.iter().zip(cursor) {
        if matches!(v, WireValue::Null | WireValue::Missing | WireValue::Oversized { .. } | WireValue::Unsupported { .. }) {
            return Err(invalid(format!("cursor value for `{col}` cannot be bound")));
        }
        let c = shape.column(col).ok_or_else(|| invalid(format!("key column `{col}` not found in relation")))?;
        params.push(to_param(c, c.kind(), v)?);
        binds.push(bind_cast(params.len(), c, c.kind(), Ctx::Cursor));
    }
    let mut disjuncts = Vec::with_capacity(key.len());
    for i in 0..key.len() {
        let mut terms: Vec<String> = (0..i).map(|j| format!("{} = {}", quote_ident(&key[j].0), binds[j])).collect();
        let op = if key[i].1 { "<" } else { ">" };
        terms.push(format!("{} {op} {}", quote_ident(&key[i].0), binds[i]));
        disjuncts.push(format!("({})", terms.join(" AND ")));
    }
    Ok(format!("({})", disjuncts.join(" OR ")))
}

fn compile_filter(tree: &FilterTree, shape: &RelationShape, params: &mut Vec<Param>) -> Result<Option<String>, DriverError> {
    match tree {
        FilterTree::And(kids) | FilterTree::Or(kids) => {
            let joiner = if matches!(tree, FilterTree::And(_)) { " AND " } else { " OR " };
            let mut parts = Vec::new();
            for k in kids {
                if let Some(s) = compile_filter(k, shape, params)? {
                    parts.push(s);
                }
            }
            Ok((!parts.is_empty()).then(|| format!("({})", parts.join(joiner))))
        }
        FilterTree::Pred(p) => compile_predicate(p, shape, params).map(Some),
    }
}

fn compile_predicate(p: &Predicate, shape: &RelationShape, params: &mut Vec<Param>) -> Result<String, DriverError> {
    let c = shape
        .column(&p.column)
        .ok_or_else(|| DriverError::new(ErrorKind::NotFound, format!("filter column `{}` not found in relation", p.column)))?;
    let kind = c.kind();
    let ident = quote_ident(&c.name);

    // Null tests take no parameter and apply to every type.
    match p.op.as_str() {
        "is_null" => return Ok(format!("{ident} IS NULL")),
        "is_not_null" => return Ok(format!("{ident} IS NOT NULL")),
        _ => {}
    }

    if !kind.bindable() {
        return Err(invalid(format!(
            "`{}` ({}) can only be filtered with `is_null` / `is_not_null` on SQL Server",
            c.name, c.type_name
        )));
    }

    // T-015 / R-009: the case toggle, for string operators on text columns;
    // refused elsewhere rather than silently ignored.
    let fold = p.case_insensitive;
    if fold {
        if !matches!(p.op.as_str(), "eq" | "ne" | "in" | "contains" | "starts_with" | "ends_with") {
            return Err(invalid(format!("`{}` has no case-insensitive form", p.op)));
        }
        if !kind.is_text() {
            return Err(invalid(format!(
                "case-insensitive comparison applies to text columns; `{}` is {}",
                c.name, c.type_name
            )));
        }
    }
    let left = if fold { format!("LOWER({})", lhs(&ident, kind)) } else { lhs(&ident, kind) };
    let bind = |params: &mut Vec<Param>, v: &WireValue| -> Result<String, DriverError> {
        params.push(to_param(c, kind, v)?);
        let b = bind_cast(params.len(), c, kind, Ctx::Filter);
        Ok(if fold { format!("LOWER({b})") } else { b })
    };

    let cmp = match p.op.as_str() {
        "eq" => Some("="),
        "ne" => Some("<>"),
        "gt" => Some(">"),
        "gte" => Some(">="),
        "lt" => Some("<"),
        "lte" => Some("<="),
        _ => None,
    };
    if let Some(op) = cmp {
        // `col = NULL` is never true; D-005 has `is_null` for this.
        if matches!(p.value, WireValue::Null | WireValue::Missing) {
            return Err(invalid(format!("`{}` cannot compare with NULL; use `is_null` or `is_not_null`", p.op)));
        }
        let b = bind(params, &p.value)?;
        return Ok(format!("{left} {op} {b}"));
    }

    match p.op.as_str() {
        "in" => {
            let items = non_null_items(p, "in")?;
            if items.is_empty() {
                // `col IN ()` is a syntax error; an empty list matches nothing.
                return Ok("1 = 0".into());
            }
            let binds = items.iter().map(|v| bind(params, v)).collect::<Result<Vec<_>, _>>()?;
            Ok(format!("{left} IN ({})", binds.join(", ")))
        }
        "between" => {
            let items = non_null_items(p, "between")?;
            let [lo, hi] = items else {
                return Err(invalid(format!("`between` takes exactly two values, got {}", items.len())));
            };
            let a = bind(params, lo)?;
            let b = bind(params, hi)?;
            Ok(format!("{left} BETWEEN {a} AND {b}"))
        }
        // R-009: the needle matches literally — `%`, `_`, `[` and the escape
        // character are escaped under an explicit ESCAPE clause. Case follows
        // the column's collation unless the toggle asks otherwise.
        "contains" | "starts_with" | "ends_with" => {
            if !kind.is_text() {
                return Err(invalid(format!("`{}` applies to text columns; `{}` is {}", p.op, c.name, c.type_name)));
            }
            let needle = match &p.value {
                WireValue::Text(s) | WireValue::Numeric(s) | WireValue::Uuid(s) | WireValue::Timestamp(s) => s.clone(),
                WireValue::Int(i) => i.to_string(),
                other => return Err(invalid(format!("`{}` needs a text value, got {other:?}", p.op))),
            };
            let needle = escape_like(&needle);
            let pattern = match p.op.as_str() {
                "contains" => format!("%{needle}%"),
                "starts_with" => format!("{needle}%"),
                _ => format!("%{needle}"),
            };
            params.push(Param::Text(pattern));
            let b = format!("@P{}", params.len());
            let b = if fold { format!("LOWER({b})") } else { b };
            Ok(format!("{left} LIKE {b} ESCAPE '\\'"))
        }
        other => Err(invalid(format!("filter operator `{other}` is not declared by this driver"))),
    }
}

/// The elements of a list-valued operator, refusing NULLs.
fn non_null_items<'a>(p: &'a Predicate, op: &str) -> Result<&'a [WireValue], DriverError> {
    let WireValue::Array(items) = &p.value else {
        return Err(invalid(format!("`{op}` takes a list of values")));
    };
    if items.iter().any(|v| matches!(v, WireValue::Null | WireValue::Missing)) {
        return Err(invalid(format!("`{op}` values cannot include NULL; combine with `is_null`")));
    }
    Ok(items)
}

/// Escape T-SQL LIKE metacharacters (`%`, `_`, `[`) and the escape itself.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '%' | '_' | '[' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

// ---- decoding -----------------------------------------------------------------

/// Decode one row of a compiled browse. Matches on tiberius's `ColumnData`
/// directly — no `get` (which panics on a mismatch, T-072) and no `FromSql`
/// conversion that could widen or round.
pub fn decode_row(compiled: &CompiledBrowse, row: &tiberius::Row) -> Vec<WireValue> {
    let cells: Vec<&ColumnData<'static>> = row.cells().map(|(_, d)| d).collect();
    compiled
        .columns
        .iter()
        .map(|plan| {
            if let Some(li) = plan.len_idx {
                if let Some(ColumnData::I64(Some(len))) = cells.get(li) {
                    if *len > compiled.ceiling {
                        return WireValue::Oversized {
                            type_name: plan.type_name.clone(),
                            byte_len: *len as u64,
                            // Re-fetching a full value is not built for SQL
                            // Server yet; the UI shows the size, no load action.
                            handle: None,
                        };
                    }
                }
            }
            match cells.get(plan.value_idx) {
                Some(d) => decode_value(plan, d),
                None => WireValue::Unsupported { type_name: plan.type_name.clone(), rendered: "missing from the result".into() },
            }
        })
        .collect()
}

fn decode_value(plan: &ColumnPlan, d: &ColumnData<'_>) -> WireValue {
    let unsupported = |rendered: String| WireValue::Unsupported { type_name: plan.type_name.clone(), rendered };
    match (plan.kind, d) {
        (_, ColumnData::Bit(None))
        | (_, ColumnData::U8(None))
        | (_, ColumnData::I16(None))
        | (_, ColumnData::I32(None))
        | (_, ColumnData::I64(None))
        | (_, ColumnData::F32(None))
        | (_, ColumnData::F64(None))
        | (_, ColumnData::String(None))
        | (_, ColumnData::Binary(None)) => WireValue::Null,

        (Kind::Bit, ColumnData::Bit(Some(b))) => WireValue::Bool(*b),
        (Kind::SmallInt, ColumnData::U8(Some(v))) => WireValue::Int(i64::from(*v)),
        (Kind::SmallInt, ColumnData::I16(Some(v))) => WireValue::Int(i64::from(*v)),
        (Kind::SmallInt, ColumnData::I32(Some(v))) => WireValue::Int(i64::from(*v)),
        // D-006: an i64 is not safe as a JSON number.
        (Kind::BigInt, ColumnData::I64(Some(v))) => WireValue::Numeric(v.to_string()),
        (Kind::Float, ColumnData::F64(Some(v))) => float_to_wire(*v, &plan.type_name),
        (Kind::Real, ColumnData::F32(Some(v))) => float_to_wire(f64::from(*v), &plan.type_name),
        (Kind::Decimal | Kind::Money, ColumnData::String(Some(s))) => WireValue::Numeric(s.to_string()),
        (Kind::Str { .. } | Kind::LegacyText | Kind::Xml, ColumnData::String(Some(s))) => WireValue::Text(s.to_string()),
        (Kind::Binary | Kind::Image, ColumnData::Binary(Some(b))) => {
            WireValue::Bytes(base64::engine::general_purpose::STANDARD.encode(b))
        }
        (Kind::Uuid, ColumnData::String(Some(s))) => WireValue::Uuid(s.to_string()),
        (Kind::Temporal(_), ColumnData::String(Some(s))) => WireValue::Timestamp(s.to_string()),
        // Display only: not editable, not comparable, never written back.
        (Kind::Variant | Kind::Clr, ColumnData::String(Some(s))) => unsupported(s.to_string()),
        (Kind::Unknown, _) => unsupported(format!("{} values cannot be shown by this version of Sluice", plan.type_name)),
        (_, other) => unsupported(format!("unexpected wire form {}", wire_form(other))),
    }
}

/// A binary float, raw only when JSON can carry it. SQL Server cannot store
/// NaN or infinity, but a value that somehow is one must not become `null`.
fn float_to_wire(v: f64, type_name: &str) -> WireValue {
    if v.is_finite() {
        WireValue::Float(v)
    } else {
        WireValue::Opaque { type_name: type_name.to_string(), rendered: v.to_string() }
    }
}

fn wire_form(d: &ColumnData<'_>) -> &'static str {
    match d {
        ColumnData::U8(_) => "tinyint",
        ColumnData::I16(_) => "smallint",
        ColumnData::I32(_) => "int",
        ColumnData::I64(_) => "bigint",
        ColumnData::F32(_) => "real",
        ColumnData::F64(_) => "float",
        ColumnData::Bit(_) => "bit",
        ColumnData::String(_) => "string",
        ColumnData::Guid(_) => "guid",
        ColumnData::Binary(_) => "binary",
        ColumnData::Numeric(_) => "numeric",
        ColumnData::Xml(_) => "xml",
        _ => "temporal",
    }
}

/// Read the catalog rows of [`shape_sql`].
pub fn shape_from_rows(rows: &[tiberius::Row]) -> Result<RelationShape, DriverError> {
    use crate::navigator::{flag, int, text};
    let mut shape = RelationShape::default();
    for r in rows {
        let Some(name) = text(r, 0)? else { continue };
        shape.columns.push(ColumnInfo {
            name,
            type_name: text(r, 1)?.unwrap_or_default(),
            base_type: text(r, 2)?,
            assembly: flag(r, 3)?,
            max_length: r.try_get::<i16, _>(4).map_err(|e| crate::error_map::map_error(&e))?.unwrap_or(0),
            precision: r.try_get::<u8, _>(5).map_err(|e| crate::error_map::map_error(&e))?.unwrap_or(0),
            scale: r.try_get::<u8, _>(6).map_err(|e| crate::error_map::map_error(&e))?.unwrap_or(0),
            nullable: flag(r, 7)?,
            pk_ordinal: int(r, 8)?.and_then(|n| u8::try_from(n).ok()),
        });
        shape.is_table = text(r, 9)?.is_some_and(|t| t.trim() == "U");
    }
    Ok(shape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sluice_core::SortKey;

    fn col(name: &str, ty: &str) -> ColumnInfo {
        let (max_length, precision, scale) = match ty {
            "int" => (4, 10, 0),
            "bigint" => (8, 19, 0),
            "money" => (8, 19, 4),
            "smallmoney" => (4, 10, 4),
            "decimal" => (17, 38, 20),
            "nvarchar" => (100, 0, 0),
            "varchar" => (100, 0, 0),
            "varbinary" => (-1, 0, 0),
            "datetime" => (8, 23, 3),
            "datetime2" => (8, 27, 7),
            _ => (-1, 0, 0),
        };
        ColumnInfo {
            name: name.into(),
            type_name: ty.into(),
            base_type: (!matches!(ty, "geography" | "hierarchyid")).then(|| ty.into()),
            assembly: matches!(ty, "geography" | "hierarchyid"),
            max_length,
            precision,
            scale,
            nullable: true,
            pk_ordinal: None,
        }
    }

    fn shape() -> RelationShape {
        let mut id = col("id", "int");
        id.nullable = false;
        id.pk_ordinal = Some(1);
        let mut big = col("big", "bigint");
        big.nullable = false;
        RelationShape {
            columns: vec![
                id,
                big,
                col("name", "nvarchar"),
                col("code", "varchar"),
                col("price", "money"),
                col("small", "smallmoney"),
                col("exact", "decimal"),
                col("blob", "varbinary"),
                col("at", "datetime2"),
                col("legacy_at", "datetime"),
                col("v", "sql_variant"),
                col("geo", "geography"),
                col("node", "hierarchyid"),
                col("vec", "vector"),
            ],
            is_table: true,
        }
    }

    fn rel() -> NodeId {
        NodeId { kind: "table".into(), path: vec!["db".into(), "dbo".into(), "t".into()] }
    }

    fn req() -> BrowseRequest {
        BrowseRequest { relation: rel(), filter: None, sort: vec![], limit: 50, cursor: None, offset: None }
    }

    fn compile_req(r: &BrowseRequest) -> Result<CompiledBrowse, DriverError> {
        compile(&rel(), &shape(), r, DEFAULT_VALUE_CEILING)
    }

    fn pred(column: &str, op: &str, value: WireValue) -> FilterTree {
        FilterTree::Pred(Predicate { case_insensitive: false, column: column.into(), op: op.into(), value })
    }

    fn filtered(f: FilterTree) -> Result<CompiledBrowse, DriverError> {
        let mut r = req();
        r.filter = Some(f);
        compile_req(&r)
    }

    #[test]
    fn money_is_selected_as_exact_text_never_through_f64() {
        let c = compile_req(&req()).unwrap();
        assert!(c.sql.contains("CONVERT(VARCHAR(64), [price], 2)"), "{}", c.sql);
        assert!(c.sql.contains("CONVERT(VARCHAR(64), [small], 2)"), "{}", c.sql);
        assert!(c.sql.contains("CONVERT(VARCHAR(64), [exact])"), "{}", c.sql);
        let price = c.columns.iter().find(|p| p.name == "price").unwrap();
        assert_eq!(price.desc.type_class, TypeClass::ExactNumeric);
        assert_eq!(price.desc.exact, Some(ExactDomain::Decimal { precision: Some(19), scale: Some(4) }));
        let big = c.columns.iter().find(|p| p.name == "big").unwrap();
        assert_eq!(big.desc.exact, Some(ExactDomain::i64()));
    }

    /// T-072: the three types that panic inside tiberius never reach its
    /// decoder raw, and an unknown type is never selected at all.
    #[test]
    fn panicking_and_unknown_types_are_converted_server_side() {
        let c = compile_req(&req()).unwrap();
        assert!(c.sql.contains("CONVERT(nvarchar(4000), [v])"), "{}", c.sql);
        assert!(c.sql.contains("[geo].ToString()"), "{}", c.sql);
        assert!(c.sql.contains("[node].ToString()"), "{}", c.sql);
        assert!(c.sql.contains("CASE WHEN [vec] IS NULL THEN NULL ELSE N'' END"), "{}", c.sql);
        assert!(!c.sql.contains("*"), "never SELECT *: {}", c.sql);
        for name in ["v", "geo", "node", "vec"] {
            let p = c.columns.iter().find(|p| p.name == name).unwrap();
            assert_eq!(p.desc.type_class, TypeClass::Unknown, "{name}");
            assert!(!p.desc.concurrency_comparable, "{name}");
        }
    }

    #[test]
    fn large_columns_are_capped_in_sql_with_a_length_companion() {
        let c = compile_req(&req()).unwrap();
        let blob = c.columns.iter().find(|p| p.name == "blob").unwrap();
        assert!(blob.len_idx.is_some());
        assert!(
            c.sql.contains(&format!("CASE WHEN CAST(DATALENGTH([blob]) AS bigint) > {DEFAULT_VALUE_CEILING} THEN NULL ELSE [blob] END")),
            "{}",
            c.sql
        );
        // nvarchar(50) cannot exceed 256 KiB; a 64-byte ceiling makes it capped.
        let name = c.columns.iter().find(|p| p.name == "name").unwrap();
        assert!(name.len_idx.is_none());
        let small = compile(&rel(), &shape(), &req(), 64).unwrap();
        assert!(small.columns.iter().find(|p| p.name == "name").unwrap().len_idx.is_some());
    }

    #[test]
    fn identifiers_are_bracket_quoted() {
        let evil = NodeId { kind: "table".into(), path: vec!["db".into(), "dbo".into(), "t]; DROP TABLE x; --".into()] };
        let c = compile(&evil, &shape(), &req(), DEFAULT_VALUE_CEILING).unwrap();
        assert!(c.sql.contains("FROM [db].[dbo].[t]]; DROP TABLE x; --]"), "{}", c.sql);
    }

    const HOSTILE: &str = "o'brien'); DROP TABLE t; --";

    /// D-005: every core operator compiles, and no value reaches the text.
    #[test]
    fn every_core_operator_binds_its_values_as_parameters() {
        let hostile = || WireValue::Text(HOSTILE.into());
        for op in sluice_core::CORE_FILTER_OPS {
            let value = match *op {
                "in" => WireValue::Array(vec![hostile(), WireValue::Text("b".into())]),
                "between" => WireValue::Array(vec![WireValue::Text("a".into()), hostile()]),
                "is_null" | "is_not_null" => WireValue::Null,
                _ => hostile(),
            };
            let c = filtered(pred("name", op, value)).unwrap_or_else(|e| panic!("{op}: {e}"));
            assert!(!c.sql.contains("DROP TABLE t"), "{op}: value interpolated: {}", c.sql);
            if !matches!(*op, "is_null" | "is_not_null") {
                assert!(c.sql.contains("@P1"), "{op}: {}", c.sql);
                assert!(
                    c.params.iter().any(|p| matches!(p, Param::Text(s) if s.contains("o'brien"))),
                    "{op}: value not bound: {:?}",
                    c.params
                );
            }
        }
    }

    #[test]
    fn core_operators_compile_to_the_expected_tsql() {
        let cases: Vec<(FilterTree, &str)> = vec![
            (pred("id", "eq", WireValue::Int(1)), "[id] = CAST(@P1 AS bigint)"),
            (pred("id", "ne", WireValue::Int(1)), "[id] <> CAST(@P1 AS bigint)"),
            (pred("id", "lt", WireValue::Int(1)), "[id] < CAST(@P1 AS bigint)"),
            (pred("id", "lte", WireValue::Int(1)), "[id] <= CAST(@P1 AS bigint)"),
            (pred("id", "gt", WireValue::Int(1)), "[id] > CAST(@P1 AS bigint)"),
            (pred("id", "gte", WireValue::Int(1)), "[id] >= CAST(@P1 AS bigint)"),
            (
                pred("id", "in", WireValue::Array(vec![WireValue::Int(1), WireValue::Int(2)])),
                "[id] IN (CAST(@P1 AS bigint), CAST(@P2 AS bigint))",
            ),
            (pred("id", "in", WireValue::Array(vec![])), "WHERE 1 = 0"),
            (
                pred("price", "between", WireValue::Array(vec![WireValue::Numeric("1".into()), WireValue::Numeric("2.5".into())])),
                "[price] BETWEEN CAST(@P1 AS decimal(38, 23)) AND CAST(@P2 AS decimal(38, 23))",
            ),
            (pred("name", "is_null", WireValue::Null), "[name] IS NULL"),
            (pred("geo", "is_not_null", WireValue::Null), "[geo] IS NOT NULL"),
            (pred("name", "contains", WireValue::Text("a".into())), "[name] LIKE @P1 ESCAPE '\\'"),
            (pred("name", "starts_with", WireValue::Text("a".into())), "[name] LIKE @P1 ESCAPE '\\'"),
            (pred("name", "ends_with", WireValue::Text("a".into())), "[name] LIKE @P1 ESCAPE '\\'"),
            (pred("at", "gte", WireValue::Timestamp("2024-01-01T00:00:00".into())), "[at] >= CAST(@P1 AS datetime2(7))"),
            (pred("legacy_at", "eq", WireValue::Timestamp("2024-01-01T00:00:00.003".into())), "[legacy_at] = CAST(@P1 AS datetime)"),
            (pred("blob", "eq", WireValue::Bytes("AP8A".into())), "[blob] = @P1"),
        ];
        for (f, want) in cases {
            let c = filtered(f.clone()).unwrap_or_else(|e| panic!("{f:?}: {e}"));
            assert!(c.sql.contains(want), "{want}\n{}", c.sql);
        }
    }

    #[test]
    fn binary_values_are_base64_decoded_and_bound_as_bytes() {
        let c = filtered(pred("blob", "eq", WireValue::Bytes("AP8A".into()))).unwrap();
        assert_eq!(c.params, vec![Param::Bytes(vec![0, 255, 0])]);
        assert!(filtered(pred("blob", "eq", WireValue::Bytes("not base64!".into()))).is_err());
    }

    #[test]
    fn like_needles_match_literally() {
        let c = filtered(pred("name", "contains", WireValue::Text("50%_[x]\\".into()))).unwrap();
        assert_eq!(c.params, vec![Param::Text("%50\\%\\_\\[x]\\\\%".into())]);
        assert!(filtered(pred("id", "contains", WireValue::Text("1".into()))).is_err(), "LIKE is for text columns");
    }

    #[test]
    fn the_case_toggle_lowers_both_sides_and_is_refused_where_meaningless() {
        let folded = |column: &str, op: &str, value: WireValue| {
            filtered(FilterTree::Pred(Predicate { case_insensitive: true, column: column.into(), op: op.into(), value }))
        };
        let c = folded("name", "eq", WireValue::Text("ALICE".into())).unwrap();
        assert!(c.sql.contains("LOWER([name]) = LOWER(@P1)"), "{}", c.sql);
        let c = folded("name", "starts_with", WireValue::Text("Al".into())).unwrap();
        assert!(c.sql.contains("LOWER([name]) LIKE LOWER(@P1) ESCAPE"), "{}", c.sql);
        assert!(folded("id", "eq", WireValue::Int(1)).unwrap_err().message.contains("text columns"));
        assert!(folded("name", "gt", WireValue::Text("a".into())).unwrap_err().message.contains("no case-insensitive form"));
    }

    #[test]
    fn null_comparisons_and_unbindable_columns_are_refused() {
        assert!(filtered(pred("id", "eq", WireValue::Null)).unwrap_err().message.contains("is_null"));
        assert!(filtered(pred("id", "in", WireValue::Array(vec![WireValue::Null]))).is_err());
        for c in ["v", "geo", "vec"] {
            assert!(filtered(pred(c, "eq", WireValue::Text("x".into()))).unwrap_err().message.contains("is_null"), "{c}");
        }
        assert!(filtered(pred("id", "between", WireValue::Array(vec![WireValue::Int(1)]))).is_err());
        assert!(filtered(pred("name", "regex", WireValue::Text("x".into()))).is_err(), "undeclared operator");
    }

    /// A decimal bound that SQL Server would round silently is refused.
    #[test]
    fn exact_values_are_never_rounded_into_a_predicate() {
        let c = filtered(pred("big", "eq", WireValue::Numeric("9223372036854775807".into()))).unwrap();
        assert_eq!(c.params, vec![Param::Text("9223372036854775807".into())]);
        assert!(filtered(pred("price", "gt", WireValue::Numeric("1.23456".into()))).is_ok(), "widened, not rounded");
        let too_fine = format!("1.{}1", "0".repeat(23));
        assert!(filtered(pred("price", "gt", WireValue::Numeric(too_fine))).unwrap_err().message.contains("decimal places"));
        assert!(filtered(pred("price", "eq", WireValue::Numeric("1.50000000000000000000000000".into()))).is_ok(), "trailing zeros are exact");
    }

    #[test]
    fn an_and_or_tree_keeps_its_grouping() {
        let tree = FilterTree::Or(vec![
            pred("id", "eq", WireValue::Int(1)),
            FilterTree::And(vec![pred("name", "starts_with", WireValue::Text("a".into())), pred("big", "gt", WireValue::Numeric("5".into()))]),
        ]);
        let c = filtered(tree).unwrap();
        assert!(
            c.sql.contains("([id] = CAST(@P1 AS bigint) OR ([name] LIKE @P2 ESCAPE '\\' AND [big] > CAST(@P3 AS bigint)))"),
            "{}",
            c.sql
        );
    }

    fn sort(col: &str, desc: bool) -> SortKey {
        SortKey { column: col.into(), descending: desc }
    }

    #[test]
    fn keyset_pages_with_top_and_the_primary_key_as_tiebreaker() {
        let mut r = req();
        r.sort = vec![sort("big", true)];
        let c = compile_req(&r).unwrap();
        assert!(c.sql.starts_with("SELECT TOP (50) "), "{}", c.sql);
        assert!(c.sql.ends_with("ORDER BY [big] DESC, [id] ASC"), "{}", c.sql);
        assert_eq!(c.paging, Paging::Keyset { key: vec![("big".into(), true), ("id".into(), false)] });

        r.cursor = Some(vec![WireValue::Numeric("50".into()), WireValue::Int(7)]);
        let c = compile_req(&r).unwrap();
        assert!(
            c.sql.contains("(([big] < CAST(@P1 AS bigint)) OR ([big] = CAST(@P1 AS bigint) AND [id] > CAST(@P2 AS bigint)))"),
            "{}",
            c.sql
        );
        assert_eq!(c.params, vec![Param::Text("50".into()), Param::Text("7".into())]);
    }

    #[test]
    fn views_keyless_tables_and_nullable_sorts_declare_offset_fetch() {
        let mut view = shape();
        view.is_table = false;
        let mut r = req();
        r.offset = Some(150);
        let c = compile(&rel(), &view, &r, DEFAULT_VALUE_CEILING).unwrap();
        assert!(c.sql.ends_with("ORDER BY [id] ASC OFFSET 150 ROWS FETCH NEXT 50 ROWS ONLY"), "{}", c.sql);
        let Paging::Offset { reason, offset } = &c.paging else { panic!("{:?}", c.paging) };
        assert_eq!(*offset, 150);
        assert!(reason.contains("no primary key"), "{reason}");

        let mut keyless = shape();
        keyless.columns.iter_mut().for_each(|c| c.pk_ordinal = None);
        let c = compile(&rel(), &keyless, &req(), DEFAULT_VALUE_CEILING).unwrap();
        assert!(c.sql.ends_with("ORDER BY (SELECT NULL) OFFSET 0 ROWS FETCH NEXT 50 ROWS ONLY"), "{}", c.sql);

        let mut r = req();
        r.sort = vec![sort("name", false)];
        let c = compile_req(&r).unwrap();
        let Paging::Offset { reason, .. } = &c.paging else { panic!("{:?}", c.paging) };
        assert!(reason.contains("`name`") && reason.contains("NULL"), "{reason}");
        assert!(matches!(c.page_info_for_test(50), PageInfo::Offset { next_offset: Some(50), .. }));
    }

    #[test]
    fn a_varchar_cursor_compares_in_the_columns_own_sort_order() {
        let mut s = shape();
        for c in &mut s.columns {
            c.pk_ordinal = None;
            if c.name == "code" {
                c.pk_ordinal = Some(1);
                c.nullable = false;
            }
        }
        let mut r = req();
        r.cursor = Some(vec![WireValue::Text("abc".into())]);
        let c = compile(&rel(), &s, &r, DEFAULT_VALUE_CEILING).unwrap();
        assert!(c.sql.contains("([code] > CAST(@P1 AS varchar(max)))"), "{}", c.sql);
        // A filter on the same column keeps Unicode semantics.
        let mut r = req();
        r.filter = Some(pred("code", "eq", WireValue::Text("é".into())));
        let c = compile(&rel(), &s, &r, DEFAULT_VALUE_CEILING).unwrap();
        assert!(c.sql.contains("[code] = @P1"), "{}", c.sql);
    }

    #[test]
    fn cursors_must_match_the_strategy_and_unsortable_columns_are_refused() {
        let mut r = req();
        r.cursor = Some(vec![WireValue::Int(1), WireValue::Int(2)]);
        assert!(compile_req(&r).unwrap_err().message.contains("1 key column"));
        r.offset = Some(0);
        let mut view = shape();
        view.is_table = false;
        r.cursor = Some(vec![WireValue::Int(1)]);
        assert!(compile(&rel(), &view, &r, DEFAULT_VALUE_CEILING).unwrap_err().message.contains("offset"));
        let mut r = req();
        r.sort = vec![sort("geo", false)];
        assert!(compile_req(&r).unwrap_err().message.contains("cannot be sorted"));
    }

    #[test]
    fn limit_is_clamped_and_too_many_parameters_are_refused() {
        let mut r = req();
        r.limit = 10_000_000;
        assert!(compile_req(&r).unwrap().sql.contains("TOP (100000)"));
        let many = WireValue::Array((0..2500).map(WireValue::Int).collect());
        assert!(filtered(pred("id", "in", many)).unwrap_err().message.contains("2100"));
    }

    #[test]
    fn classification_maps_every_type_into_a_declared_class() {
        let table: &[(&str, TypeClass)] = &[
            ("bit", TypeClass::Boolean),
            ("tinyint", TypeClass::Integer),
            ("smallint", TypeClass::Integer),
            ("int", TypeClass::Integer),
            ("bigint", TypeClass::ExactNumeric),
            ("decimal", TypeClass::ExactNumeric),
            ("numeric", TypeClass::ExactNumeric),
            ("money", TypeClass::ExactNumeric),
            ("smallmoney", TypeClass::ExactNumeric),
            ("float", TypeClass::Float),
            ("real", TypeClass::Float),
            ("char", TypeClass::Text),
            ("nvarchar", TypeClass::Text),
            ("ntext", TypeClass::Text),
            ("xml", TypeClass::Text),
            ("varbinary", TypeClass::Binary),
            ("timestamp", TypeClass::Binary),
            ("image", TypeClass::Binary),
            ("uniqueidentifier", TypeClass::Uuid),
            ("date", TypeClass::Timestamp),
            ("time", TypeClass::Timestamp),
            ("datetime", TypeClass::Timestamp),
            ("smalldatetime", TypeClass::Timestamp),
            ("datetime2", TypeClass::Timestamp),
            ("datetimeoffset", TypeClass::Timestamp),
            ("sql_variant", TypeClass::Unknown),
            ("geography", TypeClass::Unknown),
            ("geometry", TypeClass::Unknown),
            ("hierarchyid", TypeClass::Unknown),
            ("vector", TypeClass::Unknown),
        ];
        for (ty, want) in table {
            assert_eq!(col("c", ty).kind().type_class(), *want, "{ty}");
        }
        // An alias type is classified by its system base.
        let alias = ColumnInfo { type_name: "Phone".into(), base_type: Some("varchar".into()), ..col("p", "varchar") };
        assert_eq!(alias.kind(), Kind::Str { unicode: false });
        let sysname = ColumnInfo { type_name: "sysname".into(), base_type: Some("nvarchar".into()), ..col("p", "nvarchar") };
        assert_eq!(sysname.kind(), Kind::Str { unicode: true });
    }

    /// The class a column is declared with is the class of its values.
    #[test]
    fn decoded_values_carry_their_columns_class() {
        let plan = |ty: &str| {
            let c = col("c", ty);
            ColumnPlan { name: "c".into(), type_name: ty.into(), kind: c.kind(), value_idx: 0, len_idx: None, desc: column_desc(&c) }
        };
        let s = |v: &str| ColumnData::String(Some(std::borrow::Cow::Owned(v.into())));
        let cases: Vec<(&str, ColumnData<'_>, WireValue)> = vec![
            ("bit", ColumnData::Bit(Some(true)), WireValue::Bool(true)),
            ("int", ColumnData::I32(Some(-5)), WireValue::Int(-5)),
            ("bigint", ColumnData::I64(Some(i64::MAX)), WireValue::Numeric("9223372036854775807".into())),
            ("money", s("922337203685477.5807"), WireValue::Numeric("922337203685477.5807".into())),
            ("decimal", s("1.00000000000000000001"), WireValue::Numeric("1.00000000000000000001".into())),
            ("float", ColumnData::F64(Some(0.1)), WireValue::Float(0.1)),
            ("nvarchar", s("x"), WireValue::Text("x".into())),
            ("varbinary", ColumnData::Binary(Some(std::borrow::Cow::Owned(vec![0, 255, 0]))), WireValue::Bytes("AP8A".into())),
            ("uniqueidentifier", s("6F9619FF-8B86-D011-B42D-00C04FC964FF"), WireValue::Uuid("6F9619FF-8B86-D011-B42D-00C04FC964FF".into())),
            ("datetimeoffset", s("2024-01-01T10:00:00.1234567+01:00"), WireValue::Timestamp("2024-01-01T10:00:00.1234567+01:00".into())),
            ("sql_variant", s("42"), WireValue::Unsupported { type_name: "sql_variant".into(), rendered: "42".into() }),
            ("geography", s("POINT (1 2)"), WireValue::Unsupported { type_name: "geography".into(), rendered: "POINT (1 2)".into() }),
        ];
        for (ty, data, want) in cases {
            let p = plan(ty);
            let got = decode_value(&p, &data);
            assert_eq!(got, want, "{ty}");
            assert_eq!(got.type_class(), p.desc.type_class, "{ty}");
        }
        assert_eq!(decode_value(&plan("nvarchar"), &ColumnData::String(None)), WireValue::Null);
        assert!(matches!(decode_value(&plan("vector"), &s("")), WireValue::Unsupported { .. }));
    }

    impl CompiledBrowse {
        fn page_info_for_test(&self, rows: usize) -> PageInfo {
            self.paging.page_info(&self.columns, None, rows, 50)
        }
    }

    #[test]
    fn page_info_gives_the_next_position_only_for_a_full_page() {
        let c = compile_req(&req()).unwrap();
        let mut last = vec![WireValue::Null; c.columns.len()];
        last[0] = WireValue::Int(42);
        match c.paging.page_info(&c.columns, Some(&last), 50, 50) {
            PageInfo::Keyset { key, next_cursor } => {
                assert_eq!(key, vec!["id".to_string()]);
                assert_eq!(next_cursor, Some(vec![WireValue::Int(42)]));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(c.paging.page_info(&c.columns, Some(&last), 10, 50), PageInfo::Keyset { next_cursor: None, .. }));
    }
}
