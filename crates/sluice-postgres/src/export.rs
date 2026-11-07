//! Streamed export (T-074 / D-024): the Postgres side.
//!
//! Two sources, both read incrementally from the server and handed to the
//! caller's `emit` in batches, which writes them to disk. `emit` awaits the
//! write, so the server cursor advances only as fast as the file does; no
//! batch is ever sent to the webview.
//!
//! - **A browsed relation**: the browse's own projection, filter and order,
//!   compiled with no LIMIT (`browse::compile_unbounded`) and with the T-018
//!   ceiling lifted, so every value is read in full.
//! - **A raw statement**: re-executed for export, so it is held to what makes
//!   re-execution safe. It must be **one** statement — checked by the server's
//!   own parser (`prepare` refuses several), never by the display-only script
//!   splitter (R-040). And it runs inside a `READ ONLY` transaction that is
//!   rolled back, so the *database* refuses any write it attempts; nothing
//!   here tries to recognise a write by reading the SQL.

use futures_util::TryStreamExt as _;
use sluice_core::{ColumnDesc, DriverError, ErrorKind, NodeId, SqlLiterals, WireValue};
use tokio_postgres::SimpleQueryMessage;

use crate::{browse, encode, error_map, PgDriver};

// T-076: the stream items are engine-neutral and live in core.
pub use sluice_core::{ExportBatch, ExportEnd};

/// Postgres spelling of identifiers and literals for SQL `INSERT` export.
///
/// Every non-binary value is written as a plain *untyped* string literal.
/// Postgres coerces an untyped literal in `INSERT ... VALUES` to the target
/// column's type through that type's own input function, which is the same
/// text the value was read as — so the round trip is exact for every type,
/// including user-defined ones, with no cast to spell.
pub struct PgLiterals {
    target: String,
}

impl PgLiterals {
    pub fn for_relation(relation: &NodeId) -> Result<Self, DriverError> {
        Ok(Self { target: browse::qualified(relation)? })
    }

    /// A raw query has no relation; the file names a placeholder to edit.
    pub fn for_name(name: &str) -> Self {
        Self { target: encode::quote_ident(name) }
    }
}

impl SqlLiterals for PgLiterals {
    fn target(&self) -> String {
        self.target.clone()
    }

    fn ident(&self, name: &str) -> String {
        encode::quote_ident(name)
    }

    fn literal(&self, value: &WireValue, _column: &ColumnDesc) -> String {
        match value {
            WireValue::Null => "NULL".into(),
            // Absent (a document store's missing field): the column default.
            WireValue::Missing => "DEFAULT".into(),
            WireValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
            WireValue::Int(i) => i.to_string(),
            // Browse reads bytea as base64 (D-006); decode it back to bytes.
            WireValue::Bytes(b64) => format!("decode({}, 'base64')", quote_literal(b64)),
            WireValue::Float(f) => quote_literal(&f.to_string()),
            WireValue::Numeric(s) | WireValue::Text(s) | WireValue::Timestamp(s) | WireValue::Uuid(s) => {
                quote_literal(s)
            }
            WireValue::Opaque { rendered, .. } | WireValue::Unsupported { rendered, .. } => quote_literal(rendered),
            WireValue::Document(_) | WireValue::Array(_) => {
                // Postgres never produces these; kept total for the trait.
                quote_literal(&format!("{value:?}"))
            }
            WireValue::Oversized { .. } => unreachable!("the writer rejects placeholders first"),
        }
    }
}

/// A string literal that reads the same whatever `standard_conforming_strings`
/// is set to on the server the file is replayed against: a backslash forces
/// the explicit `E''` form, where backslashes are always escapes.
pub fn quote_literal(s: &str) -> String {
    if s.contains('\\') {
        format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

impl PgDriver {
    /// Export a browsed relation in full (T-074): the request's filter and
    /// sort, every row, every value uncapped.
    pub async fn export_browse<F>(
        &self,
        request: sluice_core::BrowseRequest,
        batch_rows: usize,
        mut emit: impl FnMut(ExportBatch) -> F,
    ) -> Result<ExportEnd, DriverError>
    where
        F: std::future::Future<Output = bool>,
    {
        let batch_rows = batch_rows.max(1);
        self.with_lease(false, async |client: &tokio_postgres::Client| {
            let shape = self.relation_shape(client, &request.relation).await?;
            let compiled = browse::compile_unbounded(
                &request.relation,
                &shape.columns,
                &request,
                i64::MAX,
                &shape.keys,
                &self.comparability,
            )?;
            if !emit(ExportBatch::Columns(compiled.column_descs())).await {
                return Ok(ExportEnd { rows: 0, stopped: true });
            }

            let cancel = client.cancel_token();
            let stream = client
                .query_raw(&compiled.sql, compiled.params.iter())
                .await
                .map_err(|e| error_map::map_error(&e, Some(&compiled.sql)))?;
            let mut stream = std::pin::pin!(stream);
            let mut rows = 0u64;
            let mut batch = Vec::with_capacity(batch_rows);
            while let Some(row) =
                stream.try_next().await.map_err(|e| error_map::map_error(&e, Some(&compiled.sql)))?
            {
                let mut cells = Vec::with_capacity(compiled.columns.len());
                for plan in &compiled.columns {
                    cells.push(
                        encode::decode_value(&row, plan.value_idx, plan.len_idx, &plan.ty, i64::MAX, None)
                            .map_err(|e| error_map::map_error(&e, Some(&compiled.sql)))?,
                    );
                }
                batch.push(cells);
                if batch.len() == batch_rows {
                    let n = batch.len() as u64;
                    if !emit(ExportBatch::Rows(std::mem::take(&mut batch))).await {
                        self.send_cancel(&cancel).await;
                        return Ok(ExportEnd { rows, stopped: true });
                    }
                    rows += n;
                }
            }
            if !batch.is_empty() {
                let n = batch.len() as u64;
                if !emit(ExportBatch::Rows(batch)).await {
                    return Ok(ExportEnd { rows, stopped: true });
                }
                rows += n;
            }
            Ok(ExportEnd { rows, stopped: false })
        })
        .await
    }

    /// Re-run one raw statement read-only and export its rows (T-074).
    ///
    /// Values arrive in the server's own text rendering (the simple query
    /// protocol), which is the lossless string form for every type; the
    /// descriptors carry the real types, learned from `prepare`.
    pub async fn export_query<F>(
        &self,
        sql: String,
        batch_rows: usize,
        mut emit: impl FnMut(ExportBatch) -> F,
    ) -> Result<ExportEnd, DriverError>
    where
        F: std::future::Future<Output = bool>,
    {
        let batch_rows = batch_rows.max(1);
        // Dirty: the lease carries a transaction, and the statement may have
        // touched session state; the reset path cleans both on release.
        self.with_lease(true, async |client: &tokio_postgres::Client| {
            client
                .batch_execute("BEGIN TRANSACTION READ ONLY")
                .await
                .map_err(|e| error_map::map_error(&e, None))?;
            let out = self.export_in_read_only(client, &sql, batch_rows, &mut emit).await;
            // Nothing in it could have written; end it either way.
            let _ = client.batch_execute("ROLLBACK").await;
            out
        })
        .await
    }

    async fn export_in_read_only<F>(
        &self,
        client: &tokio_postgres::Client,
        sql: &str,
        batch_rows: usize,
        emit: &mut impl FnMut(ExportBatch) -> F,
    ) -> Result<ExportEnd, DriverError>
    where
        F: std::future::Future<Output = bool>,
    {
        // The server's parser decides "one statement", not ours (R-040).
        let stmt = client.prepare(sql).await.map_err(|e| {
            let mut err = error_map::map_error(&e, Some(sql));
            if err.message.contains("multiple commands") {
                err.message = "export re-runs exactly one statement; run the statement on its own, then export it".into();
            }
            err
        })?;
        if stmt.columns().is_empty() {
            return Err(DriverError::new(ErrorKind::Unknown, "this statement returns no rows to export"));
        }
        let columns: Vec<ColumnDesc> = stmt
            .columns()
            .iter()
            .map(|c| encode::column_desc(c.name(), c.type_(), &self.comparability))
            .collect();
        let width = columns.len();
        if !emit(ExportBatch::Columns(columns)).await {
            return Ok(ExportEnd { rows: 0, stopped: true });
        }

        let cancel = client.cancel_token();
        let stream = client.simple_query_raw(sql).await.map_err(|e| error_map::map_error(&e, Some(sql)))?;
        let mut stream = std::pin::pin!(stream);
        let mut rows = 0u64;
        let mut batch = Vec::with_capacity(batch_rows);
        while let Some(msg) = stream.try_next().await.map_err(|e| error_map::map_error(&e, Some(sql)))? {
            let SimpleQueryMessage::Row(row) = msg else { continue };
            let cells: Vec<WireValue> = (0..width)
                .map(|i| match row.get(i) {
                    Some(s) => WireValue::Text(s.to_string()),
                    None => WireValue::Null,
                })
                .collect();
            batch.push(cells);
            if batch.len() == batch_rows {
                let n = batch.len() as u64;
                if !emit(ExportBatch::Rows(std::mem::take(&mut batch))).await {
                    self.send_cancel(&cancel).await;
                    return Ok(ExportEnd { rows, stopped: true });
                }
                rows += n;
            }
        }
        if !batch.is_empty() {
            let n = batch.len() as u64;
            if !emit(ExportBatch::Rows(batch)).await {
                return Ok(ExportEnd { rows, stopped: true });
            }
            rows += n;
        }
        Ok(ExportEnd { rows, stopped: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_survive_quotes_and_backslashes_under_either_string_mode() {
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_literal(r"\x00ff"), r"E'\\x00ff'");
        assert_eq!(quote_literal(r"a\'b"), r"E'a\\''b'");
    }

    #[test]
    fn binary_decodes_from_base64_and_absent_takes_the_default() {
        let l = PgLiterals::for_name("t");
        let c = encode::column_desc("b", &tokio_postgres::types::Type::BYTEA, &Default::default());
        assert_eq!(l.literal(&WireValue::Bytes("AAE=".into()), &c), "decode('AAE=', 'base64')");
        assert_eq!(l.literal(&WireValue::Missing, &c), "DEFAULT");
        assert_eq!(l.literal(&WireValue::Numeric("1.10".into()), &c), "'1.10'");
    }
}
