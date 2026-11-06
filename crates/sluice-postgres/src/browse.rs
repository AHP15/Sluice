//! Compile a structured browse request into Postgres SQL (T-009 / D-029).
//!
//! The UI never emits SQL. It sends a relation reference, a filter tree, a sort
//! list and a page cursor; this module is the only place that becomes a query.
//!
//! Parameters are always bound — never interpolated — and are sent as **text**
//! then cast to the column's own type (`($1)::numeric`). That keeps one code
//! path for every type, and keeps it lossless: a `numeric` filter value never
//! passes through f64 on its way into the predicate, which would reintroduce on
//! the write side exactly what D-006 prevents on the read side.

use sluice_core::{
    BrowseRequest, ColumnDesc, DriverError, ErrorKind, FilterTree, NodeId, Predicate, WireValue,
};
use tokio_postgres::types::Type;

use crate::encode::{column_desc, length_expr, quote_ident, rendering_for, value_expr};

/// Where each column's value (and optional length) sits in the projection.
#[derive(Clone, Debug)]
pub struct ColumnPlan {
    pub name: String,
    pub ty: Type,
    pub value_idx: usize,
    pub len_idx: Option<usize>,
    pub desc: ColumnDesc,
}

#[derive(Clone, Debug)]
pub struct CompiledBrowse {
    pub sql: String,
    pub params: Vec<Option<String>>,
    pub columns: Vec<ColumnPlan>,
    /// Projection index of the hidden `ctid`, when the relation has one.
    /// T-018 uses it to build a re-fetch handle for oversized values.
    pub ctid_idx: Option<usize>,
}

impl CompiledBrowse {
    pub fn column_descs(&self) -> Vec<ColumnDesc> {
        self.columns.iter().map(|c| c.desc.clone()).collect()
    }
}

pub fn qualified(relation: &NodeId) -> Result<String, DriverError> {
    match relation.path.as_slice() {
        [schema, name] => Ok(format!("{}.{}", quote_ident(schema), quote_ident(name))),
        [name] => Ok(quote_ident(name)),
        other => Err(DriverError::new(
            ErrorKind::NotFound,
            format!("expected [schema, relation], got {other:?}"),
        )),
    }
}

/// Build the query.
///
/// `columns` comes from preparing `SELECT * FROM rel LIMIT 0`, so the types are
/// the server's own — including for types with no Rust mapping (R-014).
pub fn compile(
    relation: &NodeId,
    columns: &[(String, Type)],
    req: &BrowseRequest,
    ceiling: i64,
    with_ctid: bool,
) -> Result<CompiledBrowse, DriverError> {
    let rel = qualified(relation)?;

    let mut select: Vec<String> = Vec::new();
    let mut plans: Vec<ColumnPlan> = Vec::new();

    let ctid_idx = if with_ctid {
        select.push("ctid::text".into());
        Some(0usize)
    } else {
        None
    };

    for (name, ty) in columns {
        let value_idx = select.len();
        select.push(format!("{} AS {}", value_expr(name, ty, ceiling), quote_ident(name)));

        let len_idx = if rendering_for(ty).can_be_huge() {
            let idx = select.len();
            select.push(format!(
                "{} AS {}",
                length_expr(name, ty),
                quote_ident(&format!("{name}__len"))
            ));
            Some(idx)
        } else {
            None
        };

        plans.push(ColumnPlan {
            name: name.clone(),
            ty: ty.clone(),
            value_idx,
            len_idx,
            desc: column_desc(name, ty),
        });
    }

    if plans.is_empty() {
        return Err(DriverError::new(
            ErrorKind::NotFound,
            format!("relation {rel} exposes no columns"),
        ));
    }

    let mut params: Vec<Option<String>> = Vec::new();
    let mut where_parts: Vec<String> = Vec::new();

    if let Some(filter) = &req.filter {
        if let Some(sql) = compile_filter(filter, columns, &mut params)? {
            where_parts.push(sql);
        }
    }

    // Keyset cursor. T-047/T-048 own the full multi-column keyset and its
    // per-relation declaration; this is the single-key form the contract's
    // `cursor` field already implies.
    if let (Some(cursor), Some(first)) = (&req.cursor, req.sort.first()) {
        if let Some(v) = cursor.first() {
            let ty = lookup(columns, &first.column).ok_or_else(|| {
                DriverError::new(
                    ErrorKind::NotFound,
                    format!("sort column `{}` not found in relation", first.column),
                )
            })?;
            let op = if first.descending { "<" } else { ">" };
            params.push(wire_to_text(v));
            where_parts.push(format!(
                "{} {} {}",
                quote_ident(&first.column),
                op,
                typed_param(params.len(), ty)
            ));
        }
    }

    let mut sql = format!("SELECT {} FROM {}", select.join(", "), rel);
    if !where_parts.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_parts.join(" AND "));
    }

    if !req.sort.is_empty() {
        let order: Vec<String> = req
            .sort
            .iter()
            .map(|s| {
                format!(
                    "{} {}",
                    quote_ident(&s.column),
                    if s.descending { "DESC" } else { "ASC" }
                )
            })
            .collect();
        sql.push_str(" ORDER BY ");
        sql.push_str(&order.join(", "));
    }

    // Bound in the SQL text rather than as a parameter: it is an integer the
    // compiler owns, never user text.
    sql.push_str(&format!(" LIMIT {}", req.limit.clamp(1, 100_000)));

    Ok(CompiledBrowse { sql, params, columns: plans, ctid_idx })
}

fn placeholder(n: usize) -> String {
    format!("${n}")
}

/// A SQL cast target for a parameter.
///
/// `Type::name()` gives Postgres' internal name, where array types are spelled
/// `_int4`. That is not valid cast syntax, so it is rewritten to `int4[]`.
pub fn cast_target(ty: &Type) -> String {
    let n = ty.name();
    match n.strip_prefix('_') {
        Some(base) => format!("{base}[]"),
        None => n.to_string(),
    }
}

/// The SQL that turns text parameter `$n` into a value of the column's type.
///
/// `($1)::text::<type>` for everything except `bytea`. Binary crosses the wire
/// as base64 (D-006), and `'AP8A'::bytea` would store those four *characters*
/// as bytes rather than the bytes they encode — every bound bytea value would be
/// silently wrong, and an edit would write the base64 text into the column.
/// T-068's corpus found this through its write-back check.
pub fn typed_param(n: usize, ty: &Type) -> String {
    if *ty == Type::BYTEA {
        format!("decode(({})::text, 'base64')", placeholder(n))
    } else {
        format!("({})::text::{}", placeholder(n), cast_target(ty))
    }
}

fn lookup<'a>(columns: &'a [(String, Type)], col: &str) -> Option<&'a Type> {
    columns.iter().find(|(n, _)| n == col).map(|(_, t)| t)
}

fn compile_filter(
    tree: &FilterTree,
    columns: &[(String, Type)],
    params: &mut Vec<Option<String>>,
) -> Result<Option<String>, DriverError> {
    match tree {
        FilterTree::And(kids) | FilterTree::Or(kids) => {
            let joiner = if matches!(tree, FilterTree::And(_)) { " AND " } else { " OR " };
            let mut parts = Vec::new();
            for k in kids {
                if let Some(s) = compile_filter(k, columns, params)? {
                    parts.push(s);
                }
            }
            if parts.is_empty() {
                Ok(None)
            } else {
                Ok(Some(format!("({})", parts.join(joiner))))
            }
        }
        FilterTree::Pred(p) => compile_predicate(p, columns, params).map(Some),
    }
}

fn compile_predicate(
    p: &Predicate,
    columns: &[(String, Type)],
    params: &mut Vec<Option<String>>,
) -> Result<String, DriverError> {
    let ty = lookup(columns, &p.column).ok_or_else(|| {
        DriverError::new(
            ErrorKind::NotFound,
            format!("filter column `{}` not found in relation", p.column),
        )
    })?;
    let col = quote_ident(&p.column);

    // T-015 / R-009: the toggle exists for string operators on text columns.
    // Anywhere else it is refused rather than silently ignored.
    let fold = p.case_insensitive;
    if fold {
        let toggleable = matches!(p.op.as_str(), "eq" | "ne" | "in" | "contains" | "starts_with" | "ends_with");
        if !toggleable {
            return Err(invalid(format!("`{}` has no case-insensitive form", p.op)));
        }
        if crate::encode::type_class_for(ty) != sluice_core::TypeClass::Text {
            return Err(invalid(format!(
                "case-insensitive comparison applies to text columns; `{}` is {}",
                p.column,
                ty.name()
            )));
        }
    }

    // Null tests take no parameter.
    match p.op.as_str() {
        "is_null" => return Ok(format!("{col} IS NULL")),
        "is_not_null" => return Ok(format!("{col} IS NOT NULL")),
        _ => {}
    }

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
        // `col = NULL` is never true: a silently empty result, not an error the
        // user would notice. D-005 has `is_null` for this; say so.
        if matches!(p.value, WireValue::Null | WireValue::Missing) {
            return Err(invalid(format!(
                "`{}` cannot compare with NULL; use `is_null` or `is_not_null`",
                p.op
            )));
        }
        params.push(wire_to_text(&p.value));
        if fold {
            // `lower()` on both sides: the toggle's meaning for `=`/`<>`.
            return Ok(format!("lower(({col})::text) {op} lower(({})::text)", placeholder(params.len())));
        }
        // `($1)::int4` makes PostgreSQL infer the PARAMETER as int4, so a text
        // value then fails to serialize. Going through ::text first pins the
        // parameter to text and lets the second cast do the conversion — which
        // is also what keeps exact numerics off the f64 path.
        return Ok(format!("{col} {op} {}", typed_param(params.len(), ty)));
    }

    match p.op.as_str() {
        // D-005: a list, each element bound and cast like any single value.
        "in" => {
            let items = non_null_items(p, "in")?;
            if items.is_empty() {
                // `col IN ()` is a syntax error; an empty list matches nothing.
                return Ok("FALSE".to_string());
            }
            let mut binds = Vec::with_capacity(items.len());
            for v in items {
                params.push(wire_to_text(v));
                binds.push(if fold {
                    format!("lower(({})::text)", placeholder(params.len()))
                } else {
                    typed_param(params.len(), ty)
                });
            }
            let lhs = if fold { format!("lower(({col})::text)") } else { col.clone() };
            return Ok(format!("{lhs} IN ({})", binds.join(", ")));
        }
        "between" => {
            let items = non_null_items(p, "between")?;
            let [lo, hi] = items else {
                return Err(invalid(format!(
                    "`between` takes exactly two values, got {}",
                    items.len()
                )));
            };
            params.push(wire_to_text(lo));
            let a = typed_param(params.len(), ty);
            params.push(wire_to_text(hi));
            let b = typed_param(params.len(), ty);
            return Ok(format!("{col} BETWEEN {a} AND {b}"));
        }
        _ => {}
    }

    match p.op.as_str() {
        // R-009: the needle always matches literally (`%`, `_` and the escape
        // character are escaped, with an explicit ESCAPE clause). Case follows
        // the engine — `LIKE` under the default collation, as the descriptor
        // reports — unless the predicate asks for `ILIKE`.
        "contains" | "starts_with" | "ends_with" => {
            let needle = escape_like(&wire_to_text(&p.value).unwrap_or_default());
            let patterned = match p.op.as_str() {
                "contains" => format!("%{needle}%"),
                "starts_with" => format!("{needle}%"),
                _ => format!("%{needle}"),
            };
            params.push(Some(patterned));
            let like = if fold { "ILIKE" } else { "LIKE" };
            Ok(format!(
                "({col})::text {like} ({})::text ESCAPE '\\'",
                placeholder(params.len())
            ))
        }
        other => Err(DriverError::new(
            ErrorKind::Unknown,
            format!("filter operator `{other}` is not declared by this driver"),
        )),
    }
}

/// The elements of a list-valued operator, refusing NULLs: `x IN (1, NULL)`
/// never matches the NULL, which reads like a bug in the tool.
fn non_null_items<'a>(p: &'a Predicate, op: &str) -> Result<&'a [WireValue], DriverError> {
    let WireValue::Array(items) = &p.value else {
        return Err(invalid(format!("`{op}` takes a list of values")));
    };
    if items.iter().any(|v| matches!(v, WireValue::Null | WireValue::Missing)) {
        return Err(invalid(format!("`{op}` values cannot include NULL; combine with `is_null`")));
    }
    Ok(items)
}

fn invalid(msg: String) -> DriverError {
    DriverError::new(ErrorKind::Unknown, msg)
}

/// Escape LIKE metacharacters so a needle containing `%` or `_` matches
/// literally — R-009's third behavioural difference, the one that is easy to
/// miss because it only shows up on user input containing wildcards.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Every parameter crosses as text and is cast in SQL, so exact types never pass
/// through a binary float.
pub fn wire_to_text(v: &WireValue) -> Option<String> {
    match v {
        WireValue::Null | WireValue::Missing => None,
        WireValue::Bool(b) => Some(b.to_string()),
        WireValue::Int(i) => Some(i.to_string()),
        WireValue::Float(f) => Some(f.to_string()),
        WireValue::Numeric(s)
        | WireValue::Text(s)
        | WireValue::Timestamp(s)
        | WireValue::Uuid(s)
        | WireValue::Bytes(s) => Some(s.clone()),
        WireValue::Opaque { rendered, .. } | WireValue::Unsupported { rendered, .. } => {
            Some(rendered.clone())
        }
        WireValue::Document(_) | WireValue::Array(_) => {
            Some(String::new()) // structural filters are not offered on Postgres
        }
        WireValue::Oversized { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sluice_core::{SortKey};

    fn cols() -> Vec<(String, Type)> {
        vec![
            ("id".into(), Type::INT4),
            ("big".into(), Type::INT8),
            ("name".into(), Type::TEXT),
            ("blob".into(), Type::BYTEA),
        ]
    }

    fn rel() -> NodeId {
        NodeId { kind: "table".into(), path: vec!["public".into(), "t".into()] }
    }

    fn req() -> BrowseRequest {
        BrowseRequest { relation: rel(), filter: None, sort: vec![], limit: 50, cursor: None }
    }

    #[test]
    fn projection_renders_non_native_columns_server_side() {
        let c = compile(&rel(), &cols(), &req(), 1024, false).unwrap();
        assert!(c.sql.contains("\"id\""), "{}", c.sql);
        assert!(c.sql.contains("(\"big\")::text"), "{}", c.sql);
        assert!(c.sql.contains("encode(\"blob\", 'base64')"), "{}", c.sql);
    }

    #[test]
    fn size_capped_columns_get_a_companion_length_column() {
        let c = compile(&rel(), &cols(), &req(), 1024, false).unwrap();
        let name_plan = c.columns.iter().find(|p| p.name == "name").unwrap();
        let id_plan = c.columns.iter().find(|p| p.name == "id").unwrap();
        assert!(name_plan.len_idx.is_some(), "text needs a length column");
        assert!(id_plan.len_idx.is_none(), "int4 cannot be huge");
    }

    #[test]
    fn identifiers_are_quoted_so_injection_via_relation_name_is_impossible() {
        let evil = NodeId {
            kind: "table".into(),
            path: vec!["public".into(), "t\"; drop table users; --".into()],
        };
        let q = qualified(&evil).unwrap();
        assert_eq!(q, "\"public\".\"t\"\"; drop table users; --\"");
    }

    #[test]
    fn filter_values_are_bound_not_interpolated() {
        let mut r = req();
        r.filter = Some(FilterTree::Pred(Predicate { case_insensitive: false,
            column: "name".into(),
            op: "eq".into(),
            value: WireValue::Text("o'brien'; drop table t".into()),
        }));
        let c = compile(&rel(), &cols(), &r, 1024, false).unwrap();
        assert!(c.sql.contains("$1"), "{}", c.sql);
        assert!(!c.sql.contains("drop table"), "value must not reach the SQL text");
        assert_eq!(c.params[0].as_deref(), Some("o'brien'; drop table t"));
    }

    fn pred(column: &str, op: &str, value: WireValue) -> FilterTree {
        FilterTree::Pred(Predicate { case_insensitive: false, column: column.into(), op: op.into(), value })
    }

    fn compile_filter_only(f: FilterTree) -> Result<CompiledBrowse, DriverError> {
        let mut r = req();
        r.filter = Some(f);
        compile(&rel(), &cols(), &r, 1024, false)
    }

    const HOSTILE: &str = "o'brien'); drop table t; --";

    fn hostile() -> WireValue {
        WireValue::Text(HOSTILE.into())
    }

    /// T-013 / D-005: every core operator compiles, and no value ever reaches
    /// the SQL text — a quote in a value must not silently change the query.
    #[test]
    fn every_core_operator_compiles_with_values_bound_not_interpolated() {
        for op in sluice_core::CORE_FILTER_OPS {
            let value = match *op {
                "in" => WireValue::Array(vec![hostile(), WireValue::Text("b".into())]),
                "between" => WireValue::Array(vec![WireValue::Text("a".into()), hostile()]),
                "is_null" | "is_not_null" => WireValue::Null,
                _ => hostile(),
            };
            let c = compile_filter_only(pred("name", op, value)).unwrap_or_else(|e| panic!("{op}: {e}"));
            assert!(!c.sql.contains("drop table"), "{op}: value interpolated: {}", c.sql);
            if !matches!(*op, "is_null" | "is_not_null") {
                assert!(c.params.iter().any(|p| p.as_deref().is_some_and(|v| v.contains("o'brien"))), "{op}: value not bound");
            }
        }
    }

    #[test]
    fn in_binds_each_element_with_the_column_cast() {
        let c = compile_filter_only(pred(
            "big",
            "in",
            WireValue::Array(vec![WireValue::Numeric("1".into()), WireValue::Numeric("9223372036854775807".into())]),
        ))
        .unwrap();
        assert!(c.sql.contains("\"big\" IN (($1)::text::int8, ($2)::text::int8)"), "{}", c.sql);
        assert_eq!(c.params.len(), 2);
    }

    #[test]
    fn an_empty_in_list_matches_nothing_rather_than_erroring() {
        let c = compile_filter_only(pred("id", "in", WireValue::Array(vec![]))).unwrap();
        assert!(c.sql.contains("WHERE FALSE"), "{}", c.sql);
    }

    #[test]
    fn between_takes_exactly_two_values_inclusive() {
        let c = compile_filter_only(pred(
            "id",
            "between",
            WireValue::Array(vec![WireValue::Int(1), WireValue::Int(10)]),
        ))
        .unwrap();
        assert!(c.sql.contains("\"id\" BETWEEN ($1)::text::int4 AND ($2)::text::int4"), "{}", c.sql);
        for wrong in [vec![WireValue::Int(1)], vec![WireValue::Int(1), WireValue::Int(2), WireValue::Int(3)]] {
            assert!(compile_filter_only(pred("id", "between", WireValue::Array(wrong))).is_err());
        }
    }

    /// `col = NULL` is never true — refused, with the operator to use instead.
    #[test]
    fn comparing_with_null_is_refused_toward_is_null() {
        let e = compile_filter_only(pred("id", "eq", WireValue::Null)).unwrap_err();
        assert!(e.message.contains("is_null"), "{}", e.message);
        assert!(compile_filter_only(pred("id", "in", WireValue::Array(vec![WireValue::Null]))).is_err());
    }

    /// D-005: the wire model carries an AND/OR tree, compiled with its grouping.
    #[test]
    fn a_nested_and_or_tree_keeps_its_grouping() {
        let tree = FilterTree::Or(vec![
            pred("id", "eq", WireValue::Int(1)),
            FilterTree::And(vec![
                pred("name", "starts_with", WireValue::Text("a".into())),
                pred("big", "gt", WireValue::Numeric("5".into())),
            ]),
        ]);
        let c = compile_filter_only(tree).unwrap();
        assert!(
            c.sql.contains("(\"id\" = ($1)::text::int4 OR ((\"name\")::text LIKE ($2)::text ESCAPE '\\' AND \"big\" > ($3)::text::int8))"),
            "{}",
            c.sql
        );
    }

    /// T-068: bytea crosses as base64, so its parameter must be decoded, not
    /// cast — `'AP8A'::bytea` would bind the four characters as bytes.
    #[test]
    fn bytea_parameters_are_base64_decoded_not_cast() {
        assert_eq!(typed_param(1, &Type::BYTEA), "decode(($1)::text, 'base64')");
        assert_eq!(typed_param(2, &Type::NUMERIC), "($2)::text::numeric");
        assert_eq!(typed_param(3, &Type::INT4_ARRAY), "($3)::text::int4[]");
    }

    #[test]
    fn exact_numeric_filters_are_cast_not_floated() {
        let mut r = req();
        r.filter = Some(FilterTree::Pred(Predicate { case_insensitive: false,
            column: "big".into(),
            op: "eq".into(),
            value: WireValue::Numeric("9223372036854775807".into()),
        }));
        let c = compile(&rel(), &cols(), &r, 1024, false).unwrap();
        assert!(c.sql.contains("($1)::text::int8"), "{}", c.sql);
        assert_eq!(c.params[0].as_deref(), Some("9223372036854775807"));
    }

    #[test]
    fn contains_follows_the_engine_case_rules_and_escapes_wildcards() {
        let mut r = req();
        r.filter = Some(FilterTree::Pred(Predicate {
            case_insensitive: false,
            column: "name".into(),
            op: "contains".into(),
            value: WireValue::Text("50%_off".into()),
        }));
        let c = compile(&rel(), &cols(), &r, 1024, false).unwrap();
        // R-009: report the engine's semantics — LIKE — rather than override.
        assert!(c.sql.contains("(\"name\")::text LIKE ($1)::text ESCAPE '\\'"), "{}", c.sql);
        assert_eq!(c.params[0].as_deref(), Some("%50\\%\\_off%"));
    }

    /// T-015: the explicit toggle, compiled the Postgres way per operator.
    #[test]
    fn the_case_insensitive_toggle_compiles_to_ilike_and_lower() {
        let folded = |column: &str, op: &str, value: WireValue| {
            let mut r = req();
            r.filter = Some(FilterTree::Pred(Predicate { case_insensitive: true, column: column.into(), op: op.into(), value }));
            compile(&rel(), &cols(), &r, 1024, false)
        };
        let c = folded("name", "starts_with", WireValue::Text("Al".into())).unwrap();
        assert!(c.sql.contains("ILIKE"), "{}", c.sql);
        let c = folded("name", "eq", WireValue::Text("ALICE".into())).unwrap();
        assert!(c.sql.contains("lower((\"name\")::text) = lower(($1)::text)"), "{}", c.sql);
        let c = folded("name", "in", WireValue::Array(vec![WireValue::Text("A".into())])).unwrap();
        assert!(c.sql.contains("lower((\"name\")::text) IN (lower(($1)::text))"), "{}", c.sql);

        // Refused, not silently ignored: a non-text column, a non-string operator.
        assert!(folded("id", "eq", WireValue::Int(1)).unwrap_err().message.contains("text columns"));
        assert!(folded("name", "gt", WireValue::Text("a".into())).unwrap_err().message.contains("no case-insensitive form"));
    }

    #[test]
    fn and_or_nesting_is_parenthesised() {
        let mut r = req();
        r.filter = Some(FilterTree::Or(vec![
            FilterTree::Pred(Predicate { case_insensitive: false, column: "id".into(), op: "gt".into(), value: WireValue::Int(1) }),
            FilterTree::And(vec![
                FilterTree::Pred(Predicate { case_insensitive: false, column: "id".into(), op: "lt".into(), value: WireValue::Int(9) }),
                FilterTree::Pred(Predicate { case_insensitive: false, column: "name".into(), op: "is_null".into(), value: WireValue::Null }),
            ]),
        ]));
        let c = compile(&rel(), &cols(), &r, 1024, false).unwrap();
        assert!(c.sql.contains(" OR "), "{}", c.sql);
        assert!(c.sql.contains(" AND "), "{}", c.sql);
        assert!(c.sql.contains("IS NULL"), "{}", c.sql);
    }

    #[test]
    fn undeclared_operators_are_refused_rather_than_guessed() {
        let mut r = req();
        r.filter = Some(FilterTree::Pred(Predicate { case_insensitive: false,
            column: "name".into(),
            op: "regex".into(),
            value: WireValue::Text("x".into()),
        }));
        assert!(compile(&rel(), &cols(), &r, 1024, false).is_err());
    }

    #[test]
    fn keyset_cursor_becomes_a_range_predicate_in_sort_direction() {
        let mut r = req();
        r.sort = vec![SortKey { column: "id".into(), descending: true }];
        r.cursor = Some(vec![WireValue::Int(100)]);
        let c = compile(&rel(), &cols(), &r, 1024, false).unwrap();
        assert!(c.sql.contains("\"id\" < ($1)::text::int4"), "{}", c.sql);
        assert!(c.sql.contains("ORDER BY \"id\" DESC"), "{}", c.sql);
    }

    #[test]
    fn limit_is_clamped_not_trusted() {
        let mut r = req();
        r.limit = 10_000_000;
        let c = compile(&rel(), &cols(), &r, 1024, false).unwrap();
        assert!(c.sql.contains("LIMIT 100000"), "{}", c.sql);
    }

    #[test]
    fn ctid_is_projected_first_when_requested() {
        let c = compile(&rel(), &cols(), &req(), 1024, true).unwrap();
        assert_eq!(c.ctid_idx, Some(0));
        assert!(c.sql.starts_with("SELECT ctid::text,"), "{}", c.sql);
        assert_eq!(c.columns[0].value_idx, 1, "value indices must shift past ctid");
    }
}
