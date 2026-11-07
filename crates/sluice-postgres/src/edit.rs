//! Staged edits: the unique-key gate, the per-row `UPDATE` (T-056 / D-027),
//! and the per-row `INSERT` for rows added in the grid.
//!
//! D-027's posture, as this module enforces it:
//!
//! - **Unique key required.** A row can be edited only if a unique key whose
//!   columns are all NOT NULL identifies it. Anything else (views, keyless
//!   tables, unique keys that admit NULL) is read-only, and the gate says why.
//! - **The gate is re-derived on apply.** The webview's copy decides what it
//!   *offers*; this module decides what is *allowed*, from the catalog, every
//!   time. An edit whose key does not match the derived key is refused.
//! - **Exactly one row per edit.** The caller runs every compiled statement in
//!   one transaction and rolls the whole batch back unless each affected
//!   exactly one row.
//!
//! - **Null-safe optimistic concurrency (T-058).** Each written column whose
//!   type the driver vouches for (`encode::concurrency_comparable`) is checked
//!   against its value as read with `IS NOT DISTINCT FROM`, never bare `=`:
//!   `col = NULL` is unknown, so `=` reports a spurious conflict on every edit
//!   to a NULL cell (R-004). Every other written column is protected by the
//!   unique key only, is reported in `concurrency_omitted`, and is named in
//!   the conflict message.
//! - **Inserts ride the same path.** A new row is one `INSERT` with only the
//!   columns the user gave a value; the rest take their defaults. It is offered
//!   under the same gate as editing, compiled into the same previewed artifact,
//!   and must add exactly one row inside the same single transaction.

use sluice_core::{
    BoundParam, DriverError, EditGate, EditOp, EditRequest, ErrorKind, Original, PlannedStatement, WireValue,
};
use tokio_postgres::types::Type;

use crate::browse::{cast_target, qualified, render_segs, typed_param, typed_param_segs, wire_to_text, Seg};
use crate::encode::{quote_ident, Comparability};

/// The relation's unique keys: one per valid, non-partial, non-expression
/// unique index, key columns only (not `INCLUDE` columns), in key order.
pub const UNIQUE_KEYS_SQL: &str = "
    SELECT x.indisprimary, array_agg(a.attname::text ORDER BY k.ord)
    FROM pg_index x
    CROSS JOIN LATERAL unnest(x.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord)
    JOIN pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = k.attnum
    WHERE x.indrelid = to_regclass($1)
      AND x.indisunique AND x.indisvalid
      AND x.indpred IS NULL AND x.indexprs IS NULL
      AND k.ord <= x.indnkeyatts
    GROUP BY x.indexrelid, x.indisprimary";

/// One unique key the catalog reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyCandidate {
    pub primary: bool,
    pub columns: Vec<String>,
}

/// Decide whether a relation is editable, and by which key.
///
/// Preference: the primary key; otherwise the narrowest unique key whose
/// columns are all NOT NULL (ties broken by column names, so the choice is
/// stable across calls).
pub fn edit_gate(kind: &str, candidates: &[KeyCandidate], not_null: &[String]) -> EditGate {
    if kind != "table" {
        return EditGate::ReadOnly {
            reason: format!(
                "A {} has no unique key the database enforces, so an edit cannot be tied to exactly one row.",
                match kind {
                    "view" => "view",
                    "matview" => "materialized view",
                    _ => "relation of this kind",
                }
            ),
        };
    }
    if let Some(pk) = candidates.iter().find(|c| c.primary) {
        return EditGate::Editable { key: pk.columns.clone() };
    }
    let mut usable: Vec<&KeyCandidate> = candidates
        .iter()
        .filter(|c| c.columns.iter().all(|col| not_null.contains(col)))
        .collect();
    usable.sort_by(|a, b| a.columns.len().cmp(&b.columns.len()).then_with(|| a.columns.cmp(&b.columns)));
    if let Some(k) = usable.first() {
        return EditGate::Editable { key: k.columns.clone() };
    }
    if candidates.is_empty() {
        EditGate::ReadOnly {
            reason: "This table has no primary key or unique constraint, so an edit cannot be guaranteed to change exactly one row.".into(),
        }
    } else {
        EditGate::ReadOnly {
            reason: "Every unique key on this table allows NULL, so it does not identify a row; add a primary key or a NOT NULL unique constraint to edit here.".into(),
        }
    }
}

/// One compiled `UPDATE`, ready to bind.
///
/// T-057 / R-003: `segs` + `params` are the single artifact. `sql` (what
/// executes) and `planned()` (what the preview shows) are both renderings of
/// it; nothing rebuilds the statement from the edit.
#[derive(Clone, Debug)]
pub struct CompiledEdit {
    pub op: EditOp,
    pub segs: Vec<Seg>,
    /// The executed statement: `segs` with `$n` placeholders.
    pub sql: String,
    pub params: Vec<Option<String>>,
    /// The cast target of each parameter, for the preview's bound-value table.
    pub param_types: Vec<String>,
    /// Written columns the statement does NOT check against their value as
    /// read (T-073 F-4, T-058): protected by the unique key only.
    pub concurrency_omitted: Vec<String>,
    /// T-060: written columns compared by their displayed text (opt-in).
    pub concurrency_rendered: Vec<String>,
    /// `SELECT 1 … WHERE <key>`, run after a zero-row update to tell "the row
    /// is gone" from "the row changed since it was read".
    pub probe_sql: String,
    pub probe_params: Vec<Option<String>>,
}

/// The longest value the preview will inline; past this it shows the
/// parameterized statement and a bound-value table instead.
pub const INLINE_MAX_CHARS: usize = 200;

/// A display-only SQL literal for a bound text parameter, or `None` when the
/// value cannot be shown inline *faithfully*.
///
/// Faithful means: pasted into the parameter's slot (which already carries the
/// explicit cast), the literal denotes exactly the bound value. That holds for
/// a standard string with `'` doubled — but only under
/// `standard_conforming_strings = on`, so any backslash is refused rather than
/// relying on a session setting; control characters and long values are
/// refused because an inline literal is no longer something a person can
/// verify by reading it.
pub fn inline_literal(v: &Option<String>) -> Option<String> {
    match v {
        None => Some("NULL".into()),
        Some(s) if s.chars().count() <= INLINE_MAX_CHARS && !s.contains('\\') && !s.chars().any(char::is_control) => {
            Some(format!("'{}'", s.replace('\'', "''")))
        }
        Some(_) => None,
    }
}

impl CompiledEdit {
    /// The preview of this artifact (T-057).
    pub fn planned(&self) -> PlannedStatement {
        let literals: Option<Vec<String>> = self.params.iter().map(inline_literal).collect();
        PlannedStatement {
            statement: self.sql.clone(),
            params: self
                .params
                .iter()
                .zip(&self.param_types)
                .enumerate()
                .map(|(i, (v, t))| BoundParam { placeholder: format!("${}", i + 1), type_name: t.clone(), value: v.clone() })
                .collect(),
            inline: literals.map(|l| render_segs(&self.segs, |i| l[i].clone())),
            concurrency_omitted: self.concurrency_omitted.clone(),
            concurrency_rendered: self.concurrency_rendered.clone(),
        }
    }

    /// The conflict for a zero-row update, given whether the row still exists.
    pub fn conflict(&self, index: usize, row_exists: bool) -> DriverError {
        if self.op == EditOp::Insert {
            return DriverError::new(
                ErrorKind::Conflict,
                format!(
                    "edit {}: the insert added no row (a trigger or rule on the table suppressed it); no edits were applied",
                    index + 1
                ),
            );
        }
        let what = if row_exists {
            "the row changed since it was read"
        } else {
            "the row it targets was not found (it may have been deleted, or its key changed)"
        };
        let unprotected = if self.concurrency_omitted.is_empty() {
            String::new()
        } else {
            format!(
                "; not checked for changes (protected by unique key only): {}",
                self.concurrency_omitted.join(", ")
            )
        };
        DriverError::new(
            ErrorKind::Conflict,
            format!("edit {}: {what}{unprotected}; no edits were applied", index + 1),
        )
    }
}

fn refuse(index: usize, msg: impl std::fmt::Display) -> DriverError {
    DriverError::new(ErrorKind::Unknown, format!("edit {}: {msg}", index + 1))
}

/// Whether a value may be written into a column. Placeholders and values the
/// driver could not decode never round-trip (T-017, T-018), and structural
/// writes are not offered on Postgres.
fn writable(v: &WireValue) -> Result<(), &'static str> {
    match v {
        WireValue::Missing => Err("a value cannot be written as absent"),
        WireValue::Oversized { .. } => Err("an oversized value was never loaded, so it cannot be written back"),
        WireValue::Unsupported { .. } => Err("a value of an undecodable type cannot be written back"),
        WireValue::Document(_) | WireValue::Array(_) => Err("structural values are not editable here"),
        _ => Ok(()),
    }
}

/// Compile one staged edit, whichever operation it is.
pub fn compile_with(
    index: usize,
    edit: &EditRequest,
    columns: &[(String, Type)],
    gate: &EditGate,
    cmp: &Comparability,
) -> Result<CompiledEdit, DriverError> {
    match edit.op {
        EditOp::Update => compile_update_with(index, edit, columns, gate, cmp),
        EditOp::Insert => compile_insert(index, edit, columns, gate),
    }
}

/// Compile one new row into `INSERT INTO rel (…) VALUES (…)`, or
/// `INSERT INTO rel DEFAULT VALUES` when no column was given a value.
///
/// Inserts are offered under the editing gate: a relation the gate holds
/// read-only (a view, a keyless table) takes no inserts here either, so a row
/// added in the grid can always be edited afterwards by its key.
pub fn compile_insert(
    index: usize,
    edit: &EditRequest,
    columns: &[(String, Type)],
    gate: &EditGate,
) -> Result<CompiledEdit, DriverError> {
    if let EditGate::ReadOnly { reason } = gate {
        return Err(DriverError::new(ErrorKind::PermissionDenied, reason.clone()));
    }
    if !edit.key.is_empty() || !edit.originals.is_empty() {
        return Err(refuse(index, "a new row has no key or original values to compare"));
    }
    let ty = |col: &str| columns.iter().find(|(n, _)| n == col).map(|(_, t)| t);
    let mut seen: Vec<&str> = Vec::new();
    for o in &edit.set {
        if seen.contains(&o.column.as_str()) {
            return Err(refuse(index, format!("column {} is set twice", o.column)));
        }
        seen.push(&o.column);
        if ty(&o.column).is_none() {
            return Err(refuse(index, format!("no column {} on this relation", o.column)));
        }
        writable(&o.value).map_err(|m| refuse(index, format!("{}: {m}", o.column)))?;
    }

    let rel = qualified(&edit.relation)?;
    let mut b = Builder::default();
    if edit.set.is_empty() {
        b.sql(format!("INSERT INTO {rel} DEFAULT VALUES"));
    } else {
        let names: Vec<String> = edit.set.iter().map(|o| quote_ident(&o.column)).collect();
        b.sql(format!("INSERT INTO {rel} ({}) VALUES (", names.join(", ")));
        for (n, o) in edit.set.iter().enumerate() {
            if n > 0 {
                b.sql(", ");
            }
            b.param(o, ty(&o.column).expect("checked above"));
        }
        b.sql(")");
    }
    let sql = render_segs(&b.segs, |i| format!("${}", i + 1));
    Ok(CompiledEdit {
        op: EditOp::Insert,
        sql,
        segs: b.segs,
        params: b.params,
        param_types: b.types,
        concurrency_omitted: vec![],
        concurrency_rendered: vec![],
        probe_sql: String::new(),
        probe_params: vec![],
    })
}

/// Compile one staged edit into `UPDATE rel SET … WHERE <key> = …`.
///
/// `columns` are the relation's real columns and types (from the same probe
/// browse uses), and `gate` was derived from the catalog for this call.
#[cfg(test)]
pub fn compile_update(
    index: usize,
    edit: &EditRequest,
    columns: &[(String, Type)],
    gate: &EditGate,
) -> Result<CompiledEdit, DriverError> {
    compile_update_with(index, edit, columns, gate, &Comparability::fallback())
}

/// `cmp` is the connection's catalog-probed comparability (T-060).
pub fn compile_update_with(
    index: usize,
    edit: &EditRequest,
    columns: &[(String, Type)],
    gate: &EditGate,
    cmp: &Comparability,
) -> Result<CompiledEdit, DriverError> {
    let key = match gate {
        EditGate::Editable { key } => key,
        EditGate::ReadOnly { reason } => {
            return Err(DriverError::new(ErrorKind::PermissionDenied, reason.clone()))
        }
    };
    let ty = |col: &str| columns.iter().find(|(n, _)| n == col).map(|(_, t)| t);

    // The key must be exactly the derived key: same columns, each once.
    let mut given: Vec<&str> = edit.key.iter().map(|o| o.column.as_str()).collect();
    given.sort_unstable();
    let mut want: Vec<&str> = key.iter().map(String::as_str).collect();
    want.sort_unstable();
    if given != want {
        return Err(refuse(
            index,
            format!("the row must be identified by its unique key ({}), not ({})", key.join(", "), given.join(", ")),
        ));
    }
    for o in &edit.key {
        if matches!(o.value, WireValue::Null | WireValue::Missing) || !o.value.is_comparable() {
            return Err(refuse(index, format!("key column {} has no usable value", o.column)));
        }
    }

    if edit.set.is_empty() {
        return Err(refuse(index, "nothing to change"));
    }
    let mut seen: Vec<&str> = Vec::new();
    for o in &edit.set {
        if seen.contains(&o.column.as_str()) {
            return Err(refuse(index, format!("column {} is set twice", o.column)));
        }
        seen.push(&o.column);
        if ty(&o.column).is_none() {
            return Err(refuse(index, format!("no column {} on this relation", o.column)));
        }
        writable(&o.value).map_err(|m| refuse(index, format!("{}: {m}", o.column)))?;
    }

    let mut seen_orig: Vec<&str> = Vec::new();
    for o in &edit.originals {
        if seen_orig.contains(&o.column.as_str()) {
            return Err(refuse(index, format!("column {} has two originals", o.column)));
        }
        seen_orig.push(&o.column);
        if ty(&o.column).is_none() {
            return Err(refuse(index, format!("no column {} on this relation", o.column)));
        }
    }

    let mut b = Builder::default();
    let rel = qualified(&edit.relation)?;
    let col_type = |o: &Original| ty(&o.column).ok_or_else(|| refuse(index, format!("no column {} on this relation", o.column)));

    b.sql(format!("UPDATE {rel} SET "));
    for (n, o) in edit.set.iter().enumerate() {
        if n > 0 {
            b.sql(", ");
        }
        b.sql(format!("{} = ", quote_ident(&o.column)));
        b.param(o, col_type(o)?);
    }
    b.sql(" WHERE ");
    // In key order, so the statement text is stable for a given edit.
    let key_values: Vec<&Original> = key
        .iter()
        .map(|col| edit.key.iter().find(|o| &o.column == col).expect("checked above"))
        .collect();
    for (n, o) in key_values.iter().enumerate() {
        if n > 0 {
            b.sql(" AND ");
        }
        // Key columns are NOT NULL (the gate guarantees it), so `=` is exact.
        b.sql(format!("{} = ", quote_ident(&o.column)));
        b.param(o, col_type(o)?);
    }

    // T-058: the concurrency predicate, over written columns only, in the
    // order they are written. T-060: comparability comes from this
    // connection's catalog; a type with no equality can, on opt-in, be
    // compared by the text the user was shown.
    let mut omitted = Vec::new();
    let mut rendered = Vec::new();
    for s in &edit.set {
        let original = edit.originals.iter().find(|o| o.column == s.column);
        let usable = original.filter(|o| !matches!(o.value, WireValue::Missing) && o.value.is_comparable());
        match usable {
            Some(o) if ty(&o.column).is_some_and(|t| cmp.comparable(t)) => {
                b.sql(format!(" AND {} IS NOT DISTINCT FROM ", quote_ident(&o.column)));
                b.param(o, col_type(o)?);
            }
            // Only where the value as read *is* the server's text form —
            // json, xml, geometric and other exotic types ship that way — so
            // the comparison is against exactly what was displayed.
            Some(o) if edit.compare_rendered && shown_as_server_text(&o.value) => {
                b.sql(format!(" AND ({})::text IS NOT DISTINCT FROM ", quote_ident(&o.column)));
                b.param_text(o);
                rendered.push(s.column.clone());
            }
            _ => omitted.push(s.column.clone()),
        }
    }

    let mut probe_params = Vec::new();
    let mut probe_pred = Vec::with_capacity(key.len());
    for o in &key_values {
        probe_params.push(wire_to_text(&o.value));
        probe_pred.push(format!("{} = {}", quote_ident(&o.column), typed_param(probe_params.len(), col_type(o)?)));
    }

    let sql = render_segs(&b.segs, |i| format!("${}", i + 1));
    Ok(CompiledEdit {
        op: EditOp::Update,
        sql,
        segs: b.segs,
        params: b.params,
        param_types: b.types,
        concurrency_omitted: omitted,
        concurrency_rendered: rendered,
        probe_sql: format!("SELECT 1 FROM {rel} WHERE {}", probe_pred.join(" AND ")),
        probe_params,
    })
}

/// The value as read is the server's own text rendering, so `::text` of the
/// stored value equals it exactly when nothing changed.
fn shown_as_server_text(v: &WireValue) -> bool {
    matches!(v, WireValue::Text(_) | WireValue::Opaque { .. } | WireValue::Null)
}

/// Accumulates one statement's segments and its bound parameters together,
/// so a slot and its value can never get out of step.
#[derive(Default)]
struct Builder {
    segs: Vec<Seg>,
    params: Vec<Option<String>>,
    types: Vec<String>,
}

impl Builder {
    fn sql(&mut self, s: impl Into<String>) {
        self.segs.push(Seg::Sql(s.into()));
    }
    /// A parameter compared as plain text (T-060's rendered form).
    fn param_text(&mut self, o: &Original) {
        self.params.push(wire_to_text(&o.value));
        self.types.push("text".into());
        self.segs.extend(typed_param_segs(self.params.len(), &Type::TEXT));
    }

    fn param(&mut self, o: &Original, ty: &Type) {
        self.params.push(wire_to_text(&o.value));
        self.types.push(if *ty == Type::BYTEA { "bytea (base64)".into() } else { cast_target(ty) });
        self.segs.extend(typed_param_segs(self.params.len(), ty));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sluice_core::NodeId;

    fn cand(primary: bool, cols: &[&str]) -> KeyCandidate {
        KeyCandidate { primary, columns: cols.iter().map(|c| c.to_string()).collect() }
    }
    fn nn(cols: &[&str]) -> Vec<String> {
        cols.iter().map(|c| c.to_string()).collect()
    }
    fn o(col: &str, v: WireValue) -> Original {
        Original { column: col.into(), value: v }
    }
    fn cols() -> Vec<(String, Type)> {
        vec![
            ("id".into(), Type::INT8),
            ("name".into(), Type::TEXT),
            ("price".into(), Type::NUMERIC),
            ("blob".into(), Type::BYTEA),
        ]
    }
    fn rel() -> NodeId {
        NodeId { kind: "table".into(), path: vec!["public".into(), "items".into()] }
    }
    fn editable() -> EditGate {
        EditGate::Editable { key: vec!["id".into()] }
    }
    fn edit(key: Vec<Original>, set: Vec<Original>) -> EditRequest {
        EditRequest { op: sluice_core::EditOp::Update, relation: rel(), key, set, originals: vec![], compare_rendered: false }
    }

    // ---- T-057 / R-003: preview and execution are one artifact -------------

    /// A tiny deterministic PRNG, so the generated cases are reproducible
    /// without a property-testing dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
            &xs[(self.next() % xs.len() as u64) as usize]
        }
    }

    /// Column names chosen to break naive string handling: a quote, a
    /// placeholder look-alike, spaces, unicode.
    const HOSTILE_COLS: &[&str] = &["name", "it's", "$1", "a \"b\"", "Ünïcødé", "col $2 x", "price"];

    fn gen_value(r: &mut Rng) -> WireValue {
        match r.next() % 9 {
            0 => WireValue::Null,
            1 => WireValue::Text(String::new()),
            2 => WireValue::Text(r.pick(&["o'brien", "''", "$1", "a\\b", "line\nbreak", "😀 ﷽", "x".repeat(250).leak()]).to_string()),
            3 => WireValue::Numeric(r.pick(&["0", "-1.50", "99999999999999999999.0001", "NaN"]).to_string()),
            4 => WireValue::Int((r.next() % 1000) as i64 - 500),
            5 => WireValue::Bool(r.next() % 2 == 0),
            6 => WireValue::Bytes(r.pick(&["", "AP8A", "aGVsbG8="]).to_string()),
            7 => WireValue::Timestamp("2026-01-01 00:00:00+00".into()),
            _ => WireValue::Text(r.pick(&["plain", "semi;colon", "--comment", "/* c */"]).to_string()),
        }
    }

    fn gen_case(r: &mut Rng) -> (Vec<(String, Type)>, EditRequest) {
        let mut cols: Vec<(String, Type)> = vec![("id".into(), Type::INT8)];
        let mut set = Vec::new();
        let mut originals = Vec::new();
        let n = 1 + (r.next() % 4) as usize;
        for c in HOSTILE_COLS.iter().take(n) {
            let v = gen_value(r);
            let ty = match &v {
                WireValue::Bytes(_) => Type::BYTEA,
                WireValue::Numeric(_) => Type::NUMERIC,
                WireValue::Int(_) => Type::INT4,
                WireValue::Bool(_) => Type::BOOL,
                WireValue::Timestamp(_) => Type::TIMESTAMPTZ,
                _ => r.pick(&[Type::TEXT, Type::VARCHAR, Type::JSON]).clone(),
            };
            cols.push((c.to_string(), ty));
            set.push(o(c, v));
            if r.next() % 3 != 0 {
                originals.push(o(c, gen_value(r)));
            }
        }
        let key = vec![o("id", WireValue::Int((r.next() % 100) as i64))];
        (cols, EditRequest { op: sluice_core::EditOp::Update, relation: rel(), key, set, originals, compare_rendered: false })
    }

    /// Undo `inline_literal` — what the server reads the literal as, under
    /// `standard_conforming_strings = on` (the only case it inlines).
    fn read_literal(l: &str) -> Option<String> {
        if l == "NULL" {
            return None;
        }
        let body = l.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).expect("a quoted literal");
        assert!(!body.replace("''", "").contains('\''), "unescaped quote in {l}");
        Some(body.replace("''", "'"))
    }

    #[test]
    fn preview_and_execution_render_one_artifact_across_generated_edit_sets() {
        let mut r = Rng(0x5eed_0057);
        let mut inlined = 0;
        for case in 0..3000 {
            let (cols, e) = gen_case(&mut r);
            let c = match compile_update(0, &e, &cols, &editable()) {
                Ok(c) => c,
                Err(err) => panic!("case {case}: {err:?}"),
            };
            let p = c.planned();

            // The executed statement IS the segments with `$n` in the slots,
            // and the slots are exactly the bound parameters, in order.
            assert_eq!(p.statement, c.sql);
            assert_eq!(render_segs(&c.segs, |i| format!("${}", i + 1)), c.sql);
            let slots: Vec<usize> = c.segs.iter().filter_map(|s| match s { Seg::Param(i) => Some(*i), _ => None }).collect();
            assert_eq!(slots, (0..c.params.len()).collect::<Vec<_>>(), "case {case}");
            assert_eq!(p.params.len(), c.params.len());
            for (i, bp) in p.params.iter().enumerate() {
                assert_eq!(bp.placeholder, format!("${}", i + 1));
                assert_eq!(bp.value, c.params[i], "case {case}: preview shows a different bound value");
            }

            // NULL and quoting: each inline literal denotes the bound value.
            match &p.inline {
                Some(inline) => {
                    inlined += 1;
                    let lits: Vec<String> = c.params.iter().map(|v| inline_literal(v).unwrap()).collect();
                    assert_eq!(inline, &render_segs(&c.segs, |i| lits[i].clone()));
                    for (i, l) in lits.iter().enumerate() {
                        assert_eq!(read_literal(l), c.params[i], "case {case}: literal {l} is not the bound value");
                    }
                }
                // Not inlined only when some value cannot be shown faithfully.
                None => assert!(c.params.iter().any(|v| inline_literal(v).is_none()), "case {case}"),
            }

            // Column order and coercion: SET follows the edit's order, and each
            // slot carries its column's own cast.
            let mut at = 0;
            for s in &e.set {
                let needle = format!("{} = ", quote_ident(&s.column));
                let pos = c.sql[at..].find(&needle).unwrap_or_else(|| panic!("case {case}: {needle} out of order in {}", c.sql));
                at += pos + needle.len();
            }
            for (bp, s) in p.params.iter().zip(&e.set) {
                let ty = &cols.iter().find(|(n, _)| n == &s.column).unwrap().1;
                let want = if *ty == Type::BYTEA { "bytea (base64)".to_string() } else { cast_target(ty) };
                assert_eq!(bp.type_name, want, "case {case}");
            }
        }
        // The generator must exercise both preview shapes.
        assert!(inlined > 500 && inlined < 3000, "inlined {inlined} of 3000");
    }

    #[test]
    fn values_that_cannot_be_inlined_faithfully_are_not() {
        assert_eq!(inline_literal(&None).as_deref(), Some("NULL"));
        assert_eq!(inline_literal(&Some("o'brien".into())).as_deref(), Some("'o''brien'"));
        // Meaning depends on standard_conforming_strings.
        assert_eq!(inline_literal(&Some("a\\b".into())), None);
        // Not something a person can verify by reading.
        assert_eq!(inline_literal(&Some("a\nb".into())), None);
        assert_eq!(inline_literal(&Some("x".repeat(INLINE_MAX_CHARS + 1))), None);
        assert!(inline_literal(&Some("x".repeat(INLINE_MAX_CHARS))).is_some());
    }

    #[test]
    fn an_identifier_containing_a_placeholder_is_left_alone() {
        let cols = vec![("id".into(), Type::INT8), ("$1".into(), Type::TEXT)];
        let c = compile_update(0, &edit(vec![o("id", WireValue::Int(5))], vec![o("$1", WireValue::Text("v".into()))]), &cols, &editable()).unwrap();
        let p = c.planned();
        assert_eq!(p.inline.as_deref(), Some("UPDATE \"public\".\"items\" SET \"$1\" = ('v')::text::text WHERE \"id\" = ('5')::text::int8"));
    }

    #[test]
    fn the_primary_key_wins() {
        let g = edit_gate("table", &[cand(false, &["email"]), cand(true, &["id"])], &nn(&["id", "email"]));
        assert_eq!(g, EditGate::Editable { key: vec!["id".into()] });
    }

    #[test]
    fn without_a_primary_key_the_narrowest_not_null_unique_key_is_used() {
        let g = edit_gate(
            "table",
            &[cand(false, &["a", "b"]), cand(false, &["nullable"]), cand(false, &["c"])],
            &nn(&["a", "b", "c"]),
        );
        assert_eq!(g, EditGate::Editable { key: vec!["c".into()] });
    }

    #[test]
    fn a_unique_key_that_admits_null_does_not_count() {
        let g = edit_gate("table", &[cand(false, &["email"])], &nn(&[]));
        assert!(matches!(g, EditGate::ReadOnly { reason } if reason.contains("allows NULL")));
    }

    #[test]
    fn a_keyless_table_is_read_only_and_says_why() {
        let g = edit_gate("table", &[], &nn(&["id"]));
        assert!(matches!(g, EditGate::ReadOnly { reason } if reason.contains("no primary key")));
    }

    #[test]
    fn views_are_read_only() {
        for k in ["view", "matview"] {
            assert!(matches!(edit_gate(k, &[cand(true, &["id"])], &nn(&["id"])), EditGate::ReadOnly { .. }));
        }
    }

    #[test]
    fn an_update_binds_every_value_and_qualifies_by_key() {
        let c = compile_update(
            0,
            &edit(
                vec![o("id", WireValue::Int(7))],
                vec![o("name", WireValue::Text("x'; DROP TABLE t; --".into())), o("price", WireValue::Numeric("1.10".into()))],
            ),
            &cols(),
            &editable(),
        )
        .unwrap();
        assert_eq!(
            c.sql,
            "UPDATE \"public\".\"items\" SET \"name\" = ($1)::text::text, \"price\" = ($2)::text::numeric WHERE \"id\" = ($3)::text::int8"
        );
        assert_eq!(c.params, vec![Some("x'; DROP TABLE t; --".into()), Some("1.10".into()), Some("7".into())]);
        assert!(!c.sql.contains("DROP"));
    }

    #[test]
    fn null_is_bound_as_a_null_parameter() {
        let c = compile_update(0, &edit(vec![o("id", WireValue::Int(1))], vec![o("name", WireValue::Null)]), &cols(), &editable()).unwrap();
        assert_eq!(c.params[0], None);
    }

    #[test]
    fn bytea_is_decoded_from_base64_not_cast() {
        let c = compile_update(0, &edit(vec![o("id", WireValue::Int(1))], vec![o("blob", WireValue::Bytes("AP8A".into()))]), &cols(), &editable()).unwrap();
        assert!(c.sql.contains("\"blob\" = decode(($1)::text, 'base64')"), "{}", c.sql);
    }

    #[test]
    fn a_read_only_gate_refuses() {
        let e = compile_update(
            0,
            &edit(vec![o("id", WireValue::Int(1))], vec![o("name", WireValue::Null)]),
            &cols(),
            &EditGate::ReadOnly { reason: "no key".into() },
        )
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::PermissionDenied);
    }

    #[test]
    fn the_key_must_be_exactly_the_derived_key() {
        // A non-key column used as the "key" could match many rows.
        let e = compile_update(0, &edit(vec![o("name", WireValue::Text("a".into()))], vec![o("price", WireValue::Null)]), &cols(), &editable()).unwrap_err();
        assert!(e.message.contains("unique key (id)"), "{}", e.message);
        // A null key matches nothing under `=`.
        assert!(compile_update(0, &edit(vec![o("id", WireValue::Null)], vec![o("name", WireValue::Null)]), &cols(), &editable()).is_err());
    }

    #[test]
    fn placeholders_and_undecodable_values_are_never_written_back() {
        for v in [
            WireValue::Oversized { type_name: "bytea".into(), byte_len: 9, handle: None },
            WireValue::Unsupported { type_name: "x".into(), rendered: "?".into() },
            WireValue::Missing,
            WireValue::Array(vec![]),
        ] {
            assert!(compile_update(0, &edit(vec![o("id", WireValue::Int(1))], vec![o("blob", v.clone())]), &cols(), &editable()).is_err(), "{v:?}");
        }
    }

    #[test]
    fn unknown_or_duplicate_columns_and_empty_sets_are_refused() {
        let k = || vec![o("id", WireValue::Int(1))];
        assert!(compile_update(0, &edit(k(), vec![o("nope", WireValue::Null)]), &cols(), &editable()).is_err());
        assert!(compile_update(0, &edit(k(), vec![o("name", WireValue::Null), o("name", WireValue::Null)]), &cols(), &editable()).is_err());
        assert!(compile_update(0, &edit(k(), vec![]), &cols(), &editable()).is_err());
    }

    fn with_originals(set: Vec<Original>, originals: Vec<Original>) -> EditRequest {
        let mut e = edit(vec![o("id", WireValue::Int(1))], set);
        e.originals = originals;
        e
    }

    #[test]
    fn written_columns_are_checked_null_safely_against_their_value_as_read() {
        let c = compile_update(
            0,
            &with_originals(
                vec![o("name", WireValue::Text("b".into())), o("price", WireValue::Numeric("2".into()))],
                vec![o("name", WireValue::Null), o("price", WireValue::Numeric("1.10".into()))],
            ),
            &cols(),
            &editable(),
        )
        .unwrap();
        assert_eq!(
            c.sql,
            "UPDATE \"public\".\"items\" SET \"name\" = ($1)::text::text, \"price\" = ($2)::text::numeric \
             WHERE \"id\" = ($3)::text::int8 \
             AND \"name\" IS NOT DISTINCT FROM ($4)::text::text \
             AND \"price\" IS NOT DISTINCT FROM ($5)::text::numeric"
        );
        // R-004: a NULL original binds as NULL and is compared null-safely —
        // never `= NULL`, which is unknown and would always "conflict".
        assert_eq!(c.params[3], None);
        assert!(!c.sql.contains("= NULL"));
        assert!(c.concurrency_omitted.is_empty());
    }

    #[test]
    fn non_comparable_columns_are_left_out_and_reported() {
        let cols = vec![
            ("id".into(), Type::INT8),
            ("doc".into(), Type::JSON),
            ("ratio".into(), Type::FLOAT8),
            ("name".into(), Type::TEXT),
        ];
        let c = compile_update(
            0,
            &with_originals(
                vec![
                    o("doc", WireValue::Text("{}".into())),
                    o("ratio", WireValue::Float(0.5)),
                    o("name", WireValue::Text("n".into())),
                ],
                vec![
                    o("doc", WireValue::Text("{\"a\":1}".into())),
                    o("ratio", WireValue::Float(0.1)),
                    o("name", WireValue::Text("m".into())),
                ],
            ),
            &cols,
            &editable(),
        )
        .unwrap();
        // json has no equality operator at all: `json = json` is an error.
        assert!(!c.sql.contains("\"doc\" IS NOT DISTINCT"));
        assert!(!c.sql.contains("\"ratio\" IS NOT DISTINCT"));
        assert!(c.sql.contains("\"name\" IS NOT DISTINCT FROM"));
        assert_eq!(c.concurrency_omitted, vec!["doc".to_string(), "ratio".to_string()]);
    }

    #[test]
    fn a_written_column_with_no_original_is_unprotected() {
        let c = compile_update(0, &with_originals(vec![o("name", WireValue::Null)], vec![]), &cols(), &editable()).unwrap();
        assert_eq!(c.concurrency_omitted, vec!["name".to_string()]);
    }

    #[test]
    fn a_conflict_names_the_unprotected_columns() {
        let cols = vec![("id".into(), Type::INT8), ("doc".into(), Type::JSON)];
        let c = compile_update(
            2,
            &with_originals(vec![o("doc", WireValue::Text("{}".into()))], vec![o("doc", WireValue::Text("[]".into()))]),
            &cols,
            &editable(),
        )
        .unwrap();
        let changed = c.conflict(2, true);
        assert_eq!(changed.kind, ErrorKind::Conflict);
        assert!(changed.message.starts_with("edit 3: the row changed since it was read"), "{}", changed.message);
        assert!(changed.message.contains("protected by unique key only): doc"), "{}", changed.message);
        assert!(c.conflict(2, false).message.contains("not found"));
    }

    #[test]
    fn the_probe_looks_the_row_up_by_key_alone() {
        let c = compile_update(
            0,
            &with_originals(vec![o("name", WireValue::Null)], vec![o("name", WireValue::Text("a".into()))]),
            &cols(),
            &editable(),
        )
        .unwrap();
        assert_eq!(c.probe_sql, "SELECT 1 FROM \"public\".\"items\" WHERE \"id\" = ($1)::text::int8");
        assert_eq!(c.probe_params, vec![Some("1".into())]);
    }

    /// T-060: with the opt-in, a column with no equality is compared by its
    /// displayed text; without it, it is reported as unprotected. Either way
    /// the preview says which.
    #[test]
    fn rendered_form_comparison_is_opt_in_and_reported() {
        let cols = vec![("id".to_string(), Type::INT4), ("doc".to_string(), Type::JSON)];
        let doc = |v: &str| Original { column: "doc".into(), value: WireValue::Text(v.into()) };
        let mut e = EditRequest {
            op: sluice_core::EditOp::Update,
            relation: rel(),
            key: vec![o("id", WireValue::Int(1))],
            set: vec![doc(r#"{"b":2}"#)],
            originals: vec![doc(r#"{"a": 1}"#)],
            compare_rendered: false,
        };
        let probed = Comparability::from_probe([23, 25]);

        let off = compile_update_with(0, &e, &cols, &editable(), &probed).unwrap();
        assert_eq!(off.concurrency_omitted, vec!["doc"]);
        assert!(!off.sql.contains("::text IS NOT DISTINCT FROM"), "{}", off.sql);

        e.compare_rendered = true;
        let on = compile_update_with(0, &e, &cols, &editable(), &probed).unwrap();
        assert!(on.concurrency_omitted.is_empty());
        assert_eq!(on.concurrency_rendered, vec!["doc"]);
        assert!(on.sql.contains(r#"("doc")::text IS NOT DISTINCT FROM ($3)::text::text"#), "{}", on.sql);
        assert_eq!(on.params[2].as_deref(), Some(r#"{"a": 1}"#), "compared against the text as read");
        assert_eq!(on.planned().concurrency_rendered, vec!["doc"]);
    }

    // ---- Inserts: a row added in the grid ---------------------------------

    fn insert(set: Vec<Original>) -> EditRequest {
        EditRequest { op: EditOp::Insert, relation: rel(), key: vec![], set, originals: vec![], compare_rendered: false }
    }

    #[test]
    fn an_insert_binds_only_the_columns_given_a_value() {
        let c = compile_insert(0, &insert(vec![o("name", WireValue::Text("o'brien".into())), o("price", WireValue::Numeric("1.50".into()))]), &cols(), &editable())
            .unwrap();
        assert_eq!(c.sql, r#"INSERT INTO "public"."items" ("name", "price") VALUES (($1)::text::text, ($2)::text::numeric)"#);
        assert!(!c.sql.contains("\"id\""), "untouched columns take their defaults: {}", c.sql);
        assert_eq!(c.params, vec![Some("o'brien".into()), Some("1.50".into())]);
        assert_eq!(c.op, EditOp::Insert);
        let p = c.planned();
        assert!(p.inline.unwrap().contains("'o''brien'"));
        assert!(p.concurrency_omitted.is_empty());
    }

    #[test]
    fn an_insert_with_no_values_uses_every_default() {
        let c = compile_insert(0, &insert(vec![]), &cols(), &editable()).unwrap();
        assert_eq!(c.sql, r#"INSERT INTO "public"."items" DEFAULT VALUES"#);
        assert!(c.params.is_empty());
    }

    #[test]
    fn an_insert_binds_null_and_decodes_bytea() {
        let c = compile_insert(0, &insert(vec![o("name", WireValue::Null), o("blob", WireValue::Bytes("aGVsbG8=".into()))]), &cols(), &editable())
            .unwrap();
        assert_eq!(c.params[0], None);
        assert!(c.sql.contains("decode("), "{}", c.sql);
    }

    #[test]
    fn inserts_are_refused_where_editing_is() {
        let gate = EditGate::ReadOnly { reason: "no key".into() };
        let e = compile_insert(0, &insert(vec![]), &cols(), &gate).unwrap_err();
        assert_eq!(e.kind, ErrorKind::PermissionDenied);
    }

    #[test]
    fn an_insert_refuses_unknown_duplicate_and_unwritable_values_and_stray_keys() {
        let bad = [
            insert(vec![o("nope", WireValue::Int(1))]),
            insert(vec![o("name", WireValue::Null), o("name", WireValue::Null)]),
            insert(vec![o("name", WireValue::Missing)]),
            EditRequest { key: vec![o("id", WireValue::Int(1))], ..insert(vec![]) },
        ];
        for e in &bad {
            assert!(compile_insert(0, e, &cols(), &editable()).is_err(), "{e:?}");
        }
    }

    #[test]
    fn the_dispatcher_routes_by_operation_and_a_suppressed_insert_says_so() {
        let c = compile_with(2, &insert(vec![]), &cols(), &editable(), &Comparability::fallback()).unwrap();
        assert_eq!(c.op, EditOp::Insert);
        let msg = c.conflict(2, false).message;
        assert!(msg.starts_with("edit 3: the insert added no row"), "{msg}");
        let u = compile_with(0, &edit(vec![o("id", WireValue::Int(1))], vec![o("name", WireValue::Null)]), &cols(), &editable(), &Comparability::fallback())
            .unwrap();
        assert_eq!(u.op, EditOp::Update);
    }
}
