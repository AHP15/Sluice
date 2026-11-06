//! Lossless wire encoding for Postgres (T-016, T-017, T-018 / D-006).
//!
//! ## The governing rule
//!
//! D-006: *the core never hands the UI a value in a form JavaScript would
//! silently damage — when in doubt, string plus type tag.* Only booleans, small
//! integers, and f64-native floats go raw. Everything else crosses as a tagged
//! string.
//!
//! ## How that is achieved, and why it also solves R-014
//!
//! Rather than decoding Postgres' binary wire format per type — which requires a
//! `FromSql` impl for every OID and is exactly what R-014 says fails for PostGIS
//! geometry, `tsvector`, domains and extension enums — the browse compiler asks
//! **Postgres itself** to render non-native columns, by selecting them as
//! `(col)::text`.
//!
//! That single choice delivers three things at once:
//!
//! - **T-016**: `numeric`, `int8`, temporals and `uuid` arrive as their exact
//!   canonical text, with no f64 anywhere in the path.
//! - **T-017 / R-014**: a type with no Rust mapping needs no Rust mapping. The
//!   server stringifies it, so an unknown OID is not a failure mode — it is the
//!   default path, with the type name resolved from the prepared statement's
//!   own type metadata.
//! - **T-018 / R-008**: because the compiler writes the projection, the size
//!   ceiling is applied **in SQL**, so an oversized value is never sent at all
//!   rather than being sent and then discarded.

use base64::Engine as _;
use sluice_core::{ColumnDesc, ExactDomain, TypeClass, WireValue};
use tokio_postgres::types::Type;
use tokio_postgres::Row;

/// Default per-value ceiling. R-008 asks for "the low hundreds of KB".
pub const DEFAULT_VALUE_CEILING: i64 = 256 * 1024;

/// Types JavaScript can hold without damage, per D-006.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Native {
    Bool,
    /// `int2`/`int4` only. `int8` is NOT here: it exceeds f64's exact integer
    /// range and is the headline case D-006 was written for.
    SmallInt,
    /// `float4`/`float8` — already binary floats, so JSON costs nothing.
    Float,
}

/// How a value should be shaped once Postgres has rendered it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextShape {
    ExactNumeric,
    Timestamp,
    Uuid,
    /// Rendered with `encode(col,'base64')` rather than `::text`, which would
    /// give Postgres' `\x…` hex form.
    Binary,
    Text,
    Json,
    /// R-014's territory: geometry, `tsvector`, enums, domains, composites,
    /// arrays. No Rust mapping needed or wanted.
    Exotic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rendering {
    Native(Native),
    Text(TextShape),
}

impl Rendering {
    /// Whether a value of this shape can be arbitrarily large, and therefore
    /// needs the T-018 ceiling applied.
    pub fn can_be_huge(&self) -> bool {
        matches!(
            self,
            Rendering::Text(TextShape::Binary)
                | Rendering::Text(TextShape::Text)
                | Rendering::Text(TextShape::Json)
                | Rendering::Text(TextShape::Exotic)
        )
    }
}

/// Classify a Postgres type. The `_ =>` arm is the point: an unrecognised OID is
/// ordinary, not exceptional (R-014).
pub fn rendering_for(ty: &Type) -> Rendering {
    match *ty {
        Type::BOOL => Rendering::Native(Native::Bool),
        Type::INT2 | Type::INT4 => Rendering::Native(Native::SmallInt),
        Type::FLOAT4 | Type::FLOAT8 => Rendering::Native(Native::Float),

        Type::INT8 | Type::NUMERIC | Type::MONEY => Rendering::Text(TextShape::ExactNumeric),
        Type::TIMESTAMP | Type::TIMESTAMPTZ | Type::DATE | Type::TIME | Type::TIMETZ
        | Type::INTERVAL => Rendering::Text(TextShape::Timestamp),
        Type::UUID => Rendering::Text(TextShape::Uuid),
        Type::BYTEA => Rendering::Text(TextShape::Binary),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::CHAR | Type::NAME => {
            Rendering::Text(TextShape::Text)
        }
        Type::JSON | Type::JSONB => Rendering::Text(TextShape::Json),

        _ => Rendering::Text(TextShape::Exotic),
    }
}

pub fn type_class_for(ty: &Type) -> TypeClass {
    match rendering_for(ty) {
        Rendering::Native(Native::Bool) => TypeClass::Boolean,
        Rendering::Native(Native::SmallInt) => TypeClass::Integer,
        Rendering::Native(Native::Float) => TypeClass::Float,
        Rendering::Text(TextShape::ExactNumeric) => TypeClass::ExactNumeric,
        Rendering::Text(TextShape::Timestamp) => TypeClass::Timestamp,
        Rendering::Text(TextShape::Uuid) => TypeClass::Uuid,
        Rendering::Text(TextShape::Binary) => TypeClass::Binary,
        Rendering::Text(TextShape::Text) => TypeClass::Text,
        Rendering::Text(TextShape::Json) => TypeClass::Structural,
        Rendering::Text(TextShape::Exotic) => TypeClass::Opaque,
    }
}

/// Quote an identifier for interpolation. Postgres doubles embedded quotes.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The SQL expression that renders one column, with the T-018 ceiling folded in.
///
/// For a size-capped column this emits a `CASE` that yields NULL when the value
/// is over the ceiling, so **the oversized bytes never leave the server** — the
/// companion length expression is what tells the encoder a placeholder is needed.
pub fn value_expr(name: &str, ty: &Type, ceiling: i64) -> String {
    let ident = quote_ident(name);
    let rendering = rendering_for(ty);

    let rendered = match rendering {
        Rendering::Native(_) => return ident,
        Rendering::Text(TextShape::Binary) => format!("encode({ident}, 'base64')"),
        Rendering::Text(_) => format!("({ident})::text"),
    };

    if rendering.can_be_huge() {
        let len = length_expr(name, ty);
        format!("CASE WHEN {len} > {ceiling} THEN NULL ELSE {rendered} END")
    } else {
        rendered
    }
}

/// Exact byte length on the server, for the ceiling test and for the placeholder.
pub fn length_expr(name: &str, ty: &Type) -> String {
    let ident = quote_ident(name);
    match rendering_for(ty) {
        // True stored bytes, not the inflated base64 length.
        Rendering::Text(TextShape::Binary) => format!("octet_length({ident})"),
        _ => format!("octet_length(({ident})::text)"),
    }
}

pub fn column_desc(name: &str, ty: &Type) -> ColumnDesc {
    ColumnDesc {
        name: name.to_string(),
        // The engine's own name, verbatim — including for types with no Rust
        // mapping, which is what R-014 asked for.
        type_name: ty.name().to_string(),
        type_class: type_class_for(ty),
        // Postgres is authoritative about nullability, but a projected
        // expression has no attnotnull. Conservative default; T-053 refines it
        // from the catalog.
        nullable: true,
        // Postgres has a real catalog: one type per column, always.
        mixed: false,
        derived: false,
        exact: exact_domain_for(ty),
    }
}

/// T-019 / R-005: the edit-path marker for exact numerics.
///
/// `int8` gets its real 64-bit bounds. `numeric` and `money` are decimals of
/// unknown precision here: a prepared statement's column metadata carries the
/// type OID but not its typmod, so `numeric(10,2)` is indistinguishable from
/// bare `numeric` at this layer. T-053's catalog introspection can refine it;
/// until then the server is the range validator for decimals.
pub fn exact_domain_for(ty: &Type) -> Option<ExactDomain> {
    match *ty {
        Type::INT8 => Some(ExactDomain::i64()),
        Type::NUMERIC | Type::MONEY => Some(ExactDomain::unconstrained_decimal()),
        _ => None,
    }
}

/// Decode one already-rendered column out of a row.
///
/// `value_idx` is the projection index of the value; `len_idx` the companion
/// length column, present only for size-capped types.
pub fn decode_value(
    row: &Row,
    value_idx: usize,
    len_idx: Option<usize>,
    ty: &Type,
    ceiling: i64,
    handle: Option<String>,
) -> Result<WireValue, tokio_postgres::Error> {
    // Check the ceiling first: over it, the value column is deliberately NULL
    // and must not be mistaken for a real SQL NULL.
    if let Some(li) = len_idx {
        let len: Option<i32> = row.try_get(li)?;
        if let Some(len) = len {
            if i64::from(len) > ceiling {
                return Ok(WireValue::Oversized {
                    type_name: ty.name().to_string(),
                    byte_len: len as u64,
                    handle,
                });
            }
        }
    }

    Ok(match rendering_for(ty) {
        Rendering::Native(Native::Bool) => match row.try_get::<_, Option<bool>>(value_idx)? {
            Some(v) => WireValue::Bool(v),
            None => WireValue::Null,
        },
        Rendering::Native(Native::SmallInt) => {
            // int2 and int4 are distinct Rust types; try the narrower first.
            if *ty == Type::INT2 {
                match row.try_get::<_, Option<i16>>(value_idx)? {
                    Some(v) => WireValue::Int(v as i64),
                    None => WireValue::Null,
                }
            } else {
                match row.try_get::<_, Option<i32>>(value_idx)? {
                    Some(v) => WireValue::Int(v as i64),
                    None => WireValue::Null,
                }
            }
        }
        Rendering::Native(Native::Float) => {
            let v = if *ty == Type::FLOAT4 {
                row.try_get::<_, Option<f32>>(value_idx)?.map(f64::from)
            } else {
                row.try_get::<_, Option<f64>>(value_idx)?
            };
            match v {
                Some(v) => float_to_wire(v, ty),
                None => WireValue::Null,
            }
        }
        Rendering::Text(shape) => {
            let raw: Option<String> = row.try_get(value_idx)?;
            match raw {
                None => WireValue::Null,
                Some(s) => text_to_wire(shape, ty, s),
            }
        }
    })
}

/// A binary float, raw only when JSON can carry it.
///
/// D-006 lets floats go raw because JSON numbers *are* f64 — but JSON has no
/// NaN or infinity, and serde serializes them as `null`. A `'NaN'::float8`
/// would then render as SQL NULL in the grid, and fail to deserialize on the
/// way back. T-068's corpus found this. Non-finite values therefore take
/// D-006's "when in doubt, string plus type tag" path, spelled the way Postgres
/// spells them so the text casts straight back to the same value.
pub fn float_to_wire(v: f64, ty: &Type) -> WireValue {
    if v.is_finite() {
        return WireValue::Float(v);
    }
    let rendered = if v.is_nan() {
        "NaN"
    } else if v > 0.0 {
        "Infinity"
    } else {
        "-Infinity"
    };
    WireValue::Opaque { type_name: ty.name().to_string(), rendered: rendered.to_string() }
}

pub fn text_to_wire(shape: TextShape, ty: &Type, s: String) -> WireValue {
    match shape {
        TextShape::ExactNumeric => WireValue::Numeric(s),
        TextShape::Timestamp => WireValue::Timestamp(s),
        TextShape::Uuid => WireValue::Uuid(s),
        // Already base64 from `encode(col,'base64')`, but Postgres wraps long
        // output with newlines, which are not valid in a compact base64 payload.
        TextShape::Binary => WireValue::Bytes(s.replace(['\n', '\r'], "")),
        TextShape::Text => WireValue::Text(s),
        TextShape::Json => WireValue::Text(s),
        // R-014's promise, delivered: the value reaches the UI as a tagged
        // string carrying the engine's own type name.
        TextShape::Exotic => WireValue::Opaque { type_name: ty.name().to_string(), rendered: s },
    }
}

/// Encode raw bytes as base64, for values fetched outside the projection path
/// (T-018's on-demand fetch).
pub fn to_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-006's raw/tagged split, asserted exactly as the decision words it.
    /// T-067: every engine type maps into a declared type class — the layer
    /// that lets the UI pick editors and operators without knowing Postgres.
    #[test]
    fn engine_types_map_into_the_declared_type_classes() {
        let table: &[(Type, TypeClass)] = &[
            (Type::BOOL, TypeClass::Boolean),
            (Type::INT2, TypeClass::Integer),
            (Type::INT4, TypeClass::Integer),
            (Type::INT8, TypeClass::ExactNumeric),
            (Type::NUMERIC, TypeClass::ExactNumeric),
            (Type::MONEY, TypeClass::ExactNumeric),
            (Type::FLOAT4, TypeClass::Float),
            (Type::FLOAT8, TypeClass::Float),
            (Type::TEXT, TypeClass::Text),
            (Type::VARCHAR, TypeClass::Text),
            (Type::BPCHAR, TypeClass::Text),
            (Type::NAME, TypeClass::Text),
            (Type::UUID, TypeClass::Uuid),
            (Type::DATE, TypeClass::Timestamp),
            (Type::TIMESTAMP, TypeClass::Timestamp),
            (Type::TIMESTAMPTZ, TypeClass::Timestamp),
            (Type::TIME, TypeClass::Timestamp),
            (Type::INTERVAL, TypeClass::Timestamp),
            (Type::BYTEA, TypeClass::Binary),
            (Type::JSON, TypeClass::Structural),
            (Type::JSONB, TypeClass::Structural),
            // R-014: anything without a Rust mapping is Opaque, never an error.
            (Type::INET, TypeClass::Opaque),
            (Type::TS_VECTOR, TypeClass::Opaque),
            (Type::INT4_ARRAY, TypeClass::Opaque),
            (Type::POINT, TypeClass::Opaque),
        ];
        for (ty, want) in table {
            assert_eq!(type_class_for(ty), *want, "{ty}");
        }
    }

    /// T-067: the class a column is *declared* with is the class of the values
    /// the encoder produces for it — otherwise the UI offers one type's editor
    /// and operators for another type's values.
    #[test]
    fn a_columns_declared_class_matches_its_encoded_values() {
        for ty in [
            Type::INT8, Type::NUMERIC, Type::TEXT, Type::VARCHAR, Type::UUID,
            Type::TIMESTAMPTZ, Type::DATE, Type::BYTEA, Type::INET,
        ] {
            let Rendering::Text(shape) = rendering_for(&ty) else { panic!("{ty} is text-rendered") };
            let v = text_to_wire(shape, &ty, "x".into());
            assert_eq!(v.type_class(), type_class_for(&ty), "{ty} encodes as {v:?}");
        }
        // The one deliberate exception, pinned so it cannot drift silently:
        // json/jsonb columns are declared Structural, but values cross as
        // verbatim Text — parsing into a Document would lose key order and
        // duplicate keys (T-068). Consequence, recorded on T-067: no filter
        // operator is declared for Structural, so json columns get none.
        for ty in [Type::JSON, Type::JSONB] {
            let Rendering::Text(shape) = rendering_for(&ty) else { panic!() };
            assert_eq!(type_class_for(&ty), TypeClass::Structural);
            assert_eq!(text_to_wire(shape, &ty, "{}".into()).type_class(), TypeClass::Text);
        }
        assert_eq!(float_to_wire(1.5, &Type::FLOAT8).type_class(), type_class_for(&Type::FLOAT8));
    }

    /// T-068: JSON has no NaN or infinity — serde writes `null`, which the
    /// grid would show as SQL NULL. Non-finite floats become tagged strings in
    /// Postgres' own spelling; finite ones stay raw.
    #[test]
    fn non_finite_floats_are_tagged_strings_not_json_null() {
        assert_eq!(float_to_wire(0.1, &Type::FLOAT8), WireValue::Float(0.1));
        for (v, want) in [(f64::NAN, "NaN"), (f64::INFINITY, "Infinity"), (f64::NEG_INFINITY, "-Infinity")] {
            let w = float_to_wire(v, &Type::FLOAT4);
            assert_eq!(w, WireValue::Opaque { type_name: "float4".into(), rendered: want.into() });
            let json = serde_json::to_string(&w).unwrap();
            assert!(!json.contains("null"), "{json}");
        }
    }

    /// T-019 / R-005: every exact numeric carries the edit-path marker, and
    /// nothing else does — in particular not the f64-safe small ints, which may
    /// keep a numeric editor.
    #[test]
    fn exact_numerics_and_only_exact_numerics_are_marked_exact() {
        assert_eq!(column_desc("id", &Type::INT8).exact, Some(ExactDomain::i64()));
        for ty in [Type::NUMERIC, Type::MONEY] {
            assert_eq!(
                column_desc("amount", &ty).exact,
                Some(ExactDomain::unconstrained_decimal()),
                "{ty}"
            );
        }
        for ty in [Type::INT2, Type::INT4, Type::FLOAT8, Type::TEXT, Type::BOOL, Type::UUID] {
            assert_eq!(column_desc("c", &ty).exact, None, "{ty} must not be marked exact");
        }
    }

    /// The bounds cross as JSON *strings*: an `i64` bound sent as a JSON number
    /// would be rounded by the webview's `JSON.parse` before validation began.
    #[test]
    fn exact_integer_bounds_serialize_as_strings() {
        let json = serde_json::to_value(ExactDomain::i64()).unwrap();
        assert_eq!(json["kind"], "integer");
        assert_eq!(json["max"], serde_json::Value::String("9223372036854775807".into()));
        assert_eq!(json["min"], serde_json::Value::String("-9223372036854775808".into()));
    }

    #[test]
    fn only_bools_small_ints_and_floats_go_raw() {
        assert!(matches!(rendering_for(&Type::BOOL), Rendering::Native(Native::Bool)));
        assert!(matches!(rendering_for(&Type::INT2), Rendering::Native(Native::SmallInt)));
        assert!(matches!(rendering_for(&Type::INT4), Rendering::Native(Native::SmallInt)));
        assert!(matches!(rendering_for(&Type::FLOAT4), Rendering::Native(Native::Float)));
        assert!(matches!(rendering_for(&Type::FLOAT8), Rendering::Native(Native::Float)));

        // The headline case: int8 exceeds f64's exact integer range.
        assert!(matches!(rendering_for(&Type::INT8), Rendering::Text(TextShape::ExactNumeric)));
        assert!(matches!(rendering_for(&Type::NUMERIC), Rendering::Text(TextShape::ExactNumeric)));
        assert!(matches!(rendering_for(&Type::TIMESTAMPTZ), Rendering::Text(TextShape::Timestamp)));
        assert!(matches!(rendering_for(&Type::UUID), Rendering::Text(TextShape::Uuid)));
        assert!(matches!(rendering_for(&Type::BYTEA), Rendering::Text(TextShape::Binary)));
    }

    /// R-014: an OID with no Rust mapping must be ordinary, not an error.
    #[test]
    fn unknown_oids_fall_through_to_exotic_rather_than_failing() {
        // 3614 = tsvector, 1700 handled above; use a made-up high OID too.
        let tsvector = Type::from_oid(3614).unwrap_or(Type::TEXT);
        if tsvector != Type::TEXT {
            assert!(matches!(rendering_for(&tsvector), Rendering::Text(TextShape::Exotic)));
        }
        // An OID tokio-postgres has never heard of.
        let unknown = Type::new("geometry".into(), 999999, tokio_postgres::types::Kind::Simple, "public".into());
        assert!(matches!(rendering_for(&unknown), Rendering::Text(TextShape::Exotic)));
        assert_eq!(column_desc("g", &unknown).type_name, "geometry");
    }

    #[test]
    fn identifiers_with_quotes_are_escaped() {
        assert_eq!(quote_ident("plain"), "\"plain\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }

    #[test]
    fn native_columns_are_selected_bare() {
        assert_eq!(value_expr("flag", &Type::BOOL, DEFAULT_VALUE_CEILING), "\"flag\"");
        assert_eq!(value_expr("n", &Type::INT4, DEFAULT_VALUE_CEILING), "\"n\"");
    }

    #[test]
    fn exact_types_are_rendered_by_the_server_without_a_ceiling() {
        assert_eq!(value_expr("big", &Type::INT8, DEFAULT_VALUE_CEILING), "(\"big\")::text");
        assert_eq!(value_expr("amt", &Type::NUMERIC, DEFAULT_VALUE_CEILING), "(\"amt\")::text");
    }

    /// R-008's guard lives in the projection: the oversized value is never sent.
    #[test]
    fn huge_capable_columns_are_capped_in_sql_not_after_transfer() {
        let e = value_expr("doc", &Type::TEXT, 1024);
        assert!(e.contains("CASE WHEN"), "{e}");
        assert!(e.contains("octet_length"), "{e}");
        assert!(e.contains("> 1024"), "{e}");
        assert!(e.contains("THEN NULL"), "{e}");
    }

    #[test]
    fn bytea_is_base64_encoded_by_the_server_and_sized_in_true_bytes() {
        let e = value_expr("blob", &Type::BYTEA, 1024);
        assert!(e.contains("encode(\"blob\", 'base64')"), "{e}");
        // The ceiling must compare against stored bytes, not the inflated
        // base64 length, or the reported size would be a third too large.
        assert_eq!(length_expr("blob", &Type::BYTEA), "octet_length(\"blob\")");
    }

    #[test]
    fn base64_newlines_are_stripped() {
        let v = text_to_wire(TextShape::Binary, &Type::BYTEA, "AAAA\nBBBB\n".into());
        assert_eq!(v, WireValue::Bytes("AAAABBBB".into()));
    }
}
