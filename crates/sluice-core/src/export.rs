//! Export writers: CSV, JSONL and SQL `INSERT` (T-074 / D-024).
//!
//! D-024: export is the escape hatch D-010's truncation notice offers for
//! results too large to show, so it must never route rows through the webview
//! — the writer runs in the core and is fed batch by batch as the driver reads
//! them, and only the file on disk ever holds the whole result.
//!
//! Every value is written from D-006's lossless string forms. Exact numerics
//! stay their literal text, binary stays base64, timestamps stay the engine's
//! own rendering; nothing passes through a float or a parsed instant on the
//! way to disk, which is exactly the precision loss D-006 exists to prevent.
//!
//! Engine-specific SQL spelling (identifier quoting, literal syntax) is not
//! known here: the driver supplies it as [`SqlLiterals`], so this module stays
//! free of engine knowledge (D-003).

use std::io::{self, Write};

use crate::value::{ColumnDesc, WireValue};

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Csv,
    Jsonl,
    SqlInsert,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Jsonl => "jsonl",
            ExportFormat::SqlInsert => "sql",
        }
    }
}

/// How one engine spells identifiers and literals in an `INSERT` statement.
pub trait SqlLiterals: Send + Sync {
    /// The quoted, possibly qualified, target relation.
    fn target(&self) -> String;
    fn ident(&self, name: &str) -> String;
    /// A literal for `value`, to be written into column `column`.
    fn literal(&self, value: &WireValue, column: &ColumnDesc) -> String;
}

#[derive(Debug)]
pub enum ExportError {
    Io(io::Error),
    /// A value arrived as a placeholder rather than in full. Writing the
    /// placeholder would silently corrupt the export, so the export stops.
    Incomplete { column: String },
    /// The row's width does not match the column descriptors.
    Shape { expected: usize, got: usize },
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Io(e) => write!(f, "could not write the export file: {e}"),
            ExportError::Incomplete { column } => write!(
                f,
                "a value in column `{column}` was not read in full, so the export was stopped rather than write a placeholder"
            ),
            ExportError::Shape { expected, got } => {
                write!(f, "a row had {got} values for {expected} columns")
            }
        }
    }
}

impl std::error::Error for ExportError {}

impl From<io::Error> for ExportError {
    fn from(e: io::Error) -> Self {
        ExportError::Io(e)
    }
}

/// Writes one result set in one format, row by row.
pub struct ExportWriter<W: Write> {
    out: W,
    format: ExportFormat,
    columns: Vec<ColumnDesc>,
    /// JSONL object keys: column names, made unique (a script can select
    /// `a, a`, and a JSON object with two `a` keys loses one in most parsers).
    keys: Vec<String>,
    sql: Option<Box<dyn SqlLiterals>>,
    /// Pre-rendered `INSERT INTO t (cols) VALUES ` prefix.
    insert_prefix: String,
    rows: u64,
}

impl<W: Write> ExportWriter<W> {
    /// `sql` is required for [`ExportFormat::SqlInsert`] and ignored otherwise.
    pub fn new(
        mut out: W,
        format: ExportFormat,
        columns: Vec<ColumnDesc>,
        sql: Option<Box<dyn SqlLiterals>>,
    ) -> Result<Self, ExportError> {
        let keys = unique_keys(&columns);
        let mut insert_prefix = String::new();
        match format {
            ExportFormat::Csv => {
                let header: Vec<String> = columns.iter().map(|c| csv_field(&c.name)).collect();
                out.write_all(header.join(",").as_bytes())?;
                out.write_all(b"\r\n")?;
            }
            ExportFormat::Jsonl => {}
            ExportFormat::SqlInsert => {
                let s = sql.as_ref().ok_or_else(|| {
                    ExportError::Io(io::Error::other("SQL export needs the engine's literal rules"))
                })?;
                let cols: Vec<String> = columns.iter().map(|c| s.ident(&c.name)).collect();
                insert_prefix = format!("INSERT INTO {} ({}) VALUES (", s.target(), cols.join(", "));
            }
        }
        Ok(Self { out, format, columns, keys, sql, insert_prefix, rows: 0 })
    }

    pub fn write_row(&mut self, row: &[WireValue]) -> Result<(), ExportError> {
        if row.len() != self.columns.len() {
            return Err(ExportError::Shape { expected: self.columns.len(), got: row.len() });
        }
        for (v, c) in row.iter().zip(&self.columns) {
            if contains_placeholder(v) {
                return Err(ExportError::Incomplete { column: c.name.clone() });
            }
        }
        match self.format {
            ExportFormat::Csv => {
                let fields: Vec<String> = row.iter().map(csv_value).collect();
                self.out.write_all(fields.join(",").as_bytes())?;
                self.out.write_all(b"\r\n")?;
            }
            ExportFormat::Jsonl => {
                // Written by hand rather than through `serde_json::Map`, which
                // sorts keys: the object keeps the result's column order.
                let fields: Vec<(&str, &WireValue)> = self.keys.iter().map(String::as_str).zip(row).collect();
                self.out.write_all(json_object(fields).as_bytes())?;
                self.out.write_all(b"\n")?;
            }
            ExportFormat::SqlInsert => {
                let s = self.sql.as_ref().expect("checked in new");
                let vals: Vec<String> =
                    row.iter().zip(&self.columns).map(|(v, c)| s.literal(v, c)).collect();
                self.out.write_all(self.insert_prefix.as_bytes())?;
                self.out.write_all(vals.join(", ").as_bytes())?;
                self.out.write_all(b");\n")?;
            }
        }
        self.rows += 1;
        Ok(())
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Flush and hand back the sink.
    pub fn finish(mut self) -> Result<(W, u64), ExportError> {
        self.out.flush()?;
        Ok((self.out, self.rows))
    }
}

fn contains_placeholder(v: &WireValue) -> bool {
    match v {
        WireValue::Oversized { .. } => true,
        WireValue::Document(fields) => fields.iter().any(|(_, v)| contains_placeholder(v)),
        WireValue::Array(items) => items.iter().any(contains_placeholder),
        _ => false,
    }
}

fn unique_keys(columns: &[ColumnDesc]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    columns
        .iter()
        .map(|c| {
            let mut key = c.name.clone();
            let mut n = 2;
            while !seen.insert(key.clone()) {
                key = format!("{}_{n}", c.name);
                n += 1;
            }
            key
        })
        .collect()
}

/// The lossless text of a scalar, or `None` for null/absent.
fn scalar_text(v: &WireValue) -> Option<String> {
    Some(match v {
        WireValue::Null | WireValue::Missing => return None,
        WireValue::Bool(b) => b.to_string(),
        WireValue::Int(i) => i.to_string(),
        // Rust prints the shortest text that parses back to the same f64.
        WireValue::Float(f) => f.to_string(),
        WireValue::Numeric(s)
        | WireValue::Text(s)
        | WireValue::Bytes(s)
        | WireValue::Timestamp(s)
        | WireValue::Uuid(s) => s.clone(),
        WireValue::Opaque { rendered, .. } | WireValue::Unsupported { rendered, .. } => rendered.clone(),
        WireValue::Document(_) | WireValue::Array(_) => json_value(v),
        WireValue::Oversized { .. } => unreachable!("rejected before formatting"),
    })
}

/// RFC 4180. NULL is an empty *unquoted* field; an empty string is `""`, so
/// the two stay distinguishable — the convention most CSV loaders (Postgres
/// `COPY ... CSV` among them) read back.
fn csv_value(v: &WireValue) -> String {
    match scalar_text(v) {
        None => String::new(),
        Some(s) if s.is_empty() => "\"\"".into(),
        Some(s) => csv_field(&s),
    }
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// JSON for one value. Integers and finite floats are JSON numbers (their
/// digits are exact); exact numerics are strings, because a JSON number would
/// be read back through a binary float by most consumers — the loss D-006
/// guards against on the wire. Objects keep their field order.
fn json_value(v: &WireValue) -> String {
    use serde_json::Value as J;
    match v {
        WireValue::Null | WireValue::Missing => "null".into(),
        WireValue::Bool(b) => b.to_string(),
        WireValue::Int(i) => i.to_string(),
        WireValue::Float(f) => match serde_json::Number::from_f64(*f) {
            Some(n) => n.to_string(),
            None => J::String(f.to_string()).to_string(),
        },
        WireValue::Document(fields) => json_object(fields.iter().map(|(k, v)| (k.as_str(), v)).collect()),
        WireValue::Array(items) => format!("[{}]", items.iter().map(json_value).collect::<Vec<_>>().join(",")),
        other => J::String(scalar_text(other).unwrap_or_default()).to_string(),
    }
}

/// An ordered JSON object; F-1: absent fields are omitted, null ones kept.
fn json_object(fields: Vec<(&str, &WireValue)>) -> String {
    let body: Vec<String> = fields
        .into_iter()
        .filter(|(_, v)| !matches!(v, WireValue::Missing))
        .map(|(k, v)| format!("{}:{}", serde_json::Value::String(k.to_string()), json_value(v)))
        .collect();
    format!("{{{}}}", body.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::TypeClass;

    fn col(name: &str) -> ColumnDesc {
        ColumnDesc {
            name: name.into(),
            type_name: "text".into(),
            type_class: TypeClass::Text,
            nullable: true,
            mixed: false,
            derived: false,
            exact: None,
            concurrency_comparable: false,
        }
    }

    fn write(format: ExportFormat, cols: &[&str], rows: &[Vec<WireValue>], sql: Option<Box<dyn SqlLiterals>>) -> String {
        let mut w = ExportWriter::new(Vec::new(), format, cols.iter().map(|c| col(c)).collect(), sql).unwrap();
        for r in rows {
            w.write_row(r).unwrap();
        }
        String::from_utf8(w.finish().unwrap().0).unwrap()
    }

    #[test]
    fn csv_distinguishes_null_from_empty_and_quotes_what_it_must() {
        let out = write(
            ExportFormat::Csv,
            &["a", "b,c"],
            &[
                vec![WireValue::Null, WireValue::Text(String::new())],
                vec![WireValue::Text("say \"hi\"\nthere".into()), WireValue::Int(-3)],
            ],
            None,
        );
        assert_eq!(out, "a,\"b,c\"\r\n,\"\"\r\n\"say \"\"hi\"\"\nthere\",-3\r\n");
    }

    /// D-006: exact numerics and wide integers reach the file digit for digit.
    #[test]
    fn exact_values_are_written_from_their_lossless_text() {
        let big = "123456789012345678901234567890.000000000000000000001";
        let row = vec![WireValue::Numeric(big.into()), WireValue::Int(i64::MAX), WireValue::Float(0.1)];
        let csv = write(ExportFormat::Csv, &["n", "i", "f"], &[row.clone()], None);
        assert!(csv.ends_with(&format!("{big},{},0.1\r\n", i64::MAX)), "{csv}");

        let jsonl = write(ExportFormat::Jsonl, &["n", "i", "f"], &[row], None);
        assert_eq!(jsonl, format!("{{\"n\":\"{big}\",\"i\":{},\"f\":0.1}}\n", i64::MAX));
    }

    #[test]
    fn jsonl_keeps_absent_distinct_from_null_and_dedupes_keys() {
        let out = write(
            ExportFormat::Jsonl,
            &["a", "a", "b"],
            &[vec![WireValue::Null, WireValue::Bool(true), WireValue::Missing]],
            None,
        );
        assert_eq!(out, "{\"a\":null,\"a_2\":true}\n");
    }

    #[test]
    fn structural_values_export_as_nested_json() {
        let doc = WireValue::Document(vec![
            ("x".into(), WireValue::Int(1)),
            ("gone".into(), WireValue::Missing),
            ("tags".into(), WireValue::Array(vec![WireValue::Text("t".into())])),
        ]);
        assert_eq!(write(ExportFormat::Jsonl, &["d"], &[vec![doc]], None), "{\"d\":{\"x\":1,\"tags\":[\"t\"]}}\n");
    }

    /// A placeholder must never be written as though it were the value.
    #[test]
    fn an_oversized_placeholder_stops_the_export() {
        let mut w = ExportWriter::new(Vec::new(), ExportFormat::Csv, vec![col("blob")], None).unwrap();
        let e = w
            .write_row(&[WireValue::Oversized { type_name: "bytea".into(), byte_len: 9, handle: None }])
            .unwrap_err();
        assert!(matches!(e, ExportError::Incomplete { ref column } if column == "blob"), "{e}");
    }

    struct Fake;
    impl SqlLiterals for Fake {
        fn target(&self) -> String {
            "\"t\"".into()
        }
        fn ident(&self, n: &str) -> String {
            format!("\"{n}\"")
        }
        fn literal(&self, v: &WireValue, _: &ColumnDesc) -> String {
            match scalar_text(v) {
                None => "NULL".into(),
                Some(s) => format!("'{s}'"),
            }
        }
    }

    #[test]
    fn sql_insert_writes_one_statement_per_row_through_the_engine_rules() {
        let out = write(
            ExportFormat::SqlInsert,
            &["id", "name"],
            &[vec![WireValue::Int(1), WireValue::Null], vec![WireValue::Int(2), WireValue::Text("b".into())]],
            Some(Box::new(Fake)),
        );
        assert_eq!(
            out,
            "INSERT INTO \"t\" (\"id\", \"name\") VALUES ('1', NULL);\nINSERT INTO \"t\" (\"id\", \"name\") VALUES ('2', 'b');\n"
        );
    }

    #[test]
    fn a_row_of_the_wrong_width_is_refused() {
        let mut w = ExportWriter::new(Vec::new(), ExportFormat::Csv, vec![col("a")], None).unwrap();
        assert!(matches!(w.write_row(&[]), Err(ExportError::Shape { expected: 1, got: 0 })));
    }
}
