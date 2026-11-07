//! T-074 / D-024 against a real server: export streams every row, past the
//! browse limit and the value ceiling, with exact values; a raw export runs one
//! statement and cannot write. Skips when the dev container is not running.

use std::sync::{Arc, Mutex};

use sluice_core::{BrowseRequest, Driver, ExportFormat, ExportWriter, NodeId, RawRequest, Secret, SqlLiterals, WireValue};
use sluice_postgres::export::{ExportBatch, PgLiterals};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

async fn driver() -> Option<Arc<PgDriver>> {
    let spec = ConnectSpec {
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: "postgres".into(),
        password: Some(Secret::new("sluice_dev".into())),
        dbname: "sluice_dev".into(),
        tls: TlsMode::Disabled,
        application_name: "sluice-export-test".into(),
        statement_timeout: None,
    };
    match PgDriver::connect(spec).await {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("SKIP: no Postgres reachable ({e})");
            None
        }
    }
}

type Sink = Arc<Mutex<Option<ExportWriter<Vec<u8>>>>>;

/// An `emit` that writes into an in-memory export, the way the app writes to a file.
fn writer_emit(
    sink: Sink,
    format: ExportFormat,
    sql: impl Fn() -> Option<Box<dyn SqlLiterals>> + Clone,
) -> impl FnMut(ExportBatch) -> std::future::Ready<bool> {
    move |b| {
        let mut s = sink.lock().unwrap();
        let ok = match b {
            ExportBatch::Columns(cols) => {
                *s = Some(ExportWriter::new(Vec::new(), format, cols, sql()).unwrap());
                true
            }
            ExportBatch::Rows(rows) => rows.iter().all(|r| s.as_mut().unwrap().write_row(r).is_ok()),
        };
        std::future::ready(ok)
    }
}

fn finish(sink: Sink) -> String {
    let w = sink.lock().unwrap().take().unwrap();
    String::from_utf8(w.finish().unwrap().0).unwrap()
}

fn rel(name: &str) -> NodeId {
    NodeId { kind: "table".into(), path: vec!["public".into(), name.into()] }
}

#[tokio::test]
async fn a_browse_export_streams_every_row_with_values_in_full() {
    let Some(d) = driver().await else { return };
    d.raw(RawRequest::Script(
        "DROP TABLE IF EXISTS t074_x; CREATE TABLE t074_x (id int primary key, n numeric, b bytea, note text);
         INSERT INTO t074_x SELECT g, 0.1 + g, NULL, CASE WHEN g = 1 THEN '' END FROM generate_series(1, 2500) g;
         UPDATE t074_x SET n = 123456789012345678901234567890.000000000000000000001, b = decode(repeat('ab', 300000), 'hex') WHERE id = 2;"
            .into(),
    ))
    .await
    .unwrap();

    // The browse limit is ignored: an export is the whole relation.
    let req = BrowseRequest { relation: rel("t074_x"), filter: None, sort: vec![], limit: 10, cursor: None, offset: None };
    let sink: Sink = Arc::default();
    let end = d.export_browse(req, 100, writer_emit(sink.clone(), ExportFormat::Csv, || None)).await.unwrap();
    assert_eq!(end.rows, 2500);
    assert!(!end.stopped);

    let csv = finish(sink);
    let lines: Vec<&str> = csv.split("\r\n").collect();
    assert_eq!(lines[0], "id,n,b,note");
    // NULL is empty; the empty string is quoted.
    assert_eq!(lines[1], "1,1.1,,\"\"");
    // Exact numeric digit for digit; a 300KB value — well past the ceiling —
    // is written in full rather than as a placeholder.
    let row2: Vec<&str> = lines[2].split(',').collect();
    assert_eq!(row2[1], "123456789012345678901234567890.000000000000000000001");
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(row2[2]).unwrap();
    assert_eq!(bytes.len(), 300_000);
}

/// The SQL export replays into an identical table.
#[tokio::test]
async fn a_sql_insert_export_round_trips() {
    let Some(d) = driver().await else { return };
    d.raw(RawRequest::Script(
        r#"DROP TABLE IF EXISTS t074_rt, t074_rt2; CREATE TABLE t074_rt (id int primary key, n numeric, b bytea, s text, j jsonb, a int[], f float8);
         INSERT INTO t074_rt VALUES (1, 1.10, '\x00ff', 'it''s \ back', '{"k": 1}', '{1,2}', 0.1), (2, NULL, NULL, NULL, NULL, NULL, 'NaN');
         CREATE TABLE t074_rt2 (LIKE t074_rt);"#
            .into(),
    ))
    .await
    .unwrap();

    let req = BrowseRequest { relation: rel("t074_rt"), filter: None, sort: vec![], limit: 1, cursor: None, offset: None };
    let sink: Sink = Arc::default();
    let target = || Some(Box::new(PgLiterals::for_relation(&rel("t074_rt2")).unwrap()) as Box<dyn SqlLiterals>);
    d.export_browse(req, 10, writer_emit(sink.clone(), ExportFormat::SqlInsert, target)).await.unwrap();
    let sql = finish(sink);
    let r = d.raw(RawRequest::Script(sql.clone())).await.unwrap();
    assert!(r.outcomes.iter().all(|o| !matches!(o.body, sluice_core::OutcomeBody::Error { .. })), "{sql}");

    let diff = d
        .raw(RawRequest::Script(
            "SELECT count(*) FROM ((TABLE t074_rt EXCEPT TABLE t074_rt2) UNION ALL (TABLE t074_rt2 EXCEPT TABLE t074_rt)) x".into(),
        ))
        .await
        .unwrap();
    let (_, rows) = diff.first_rows().unwrap();
    assert_eq!(rows[0][0], WireValue::Text("0".into()), "replayed rows differ:\n{sql}");
}

#[tokio::test]
async fn a_raw_export_runs_one_statement_and_cannot_write() {
    let Some(d) = driver().await else { return };
    d.raw(RawRequest::Script(
        "DROP TABLE IF EXISTS t074_w; CREATE TABLE t074_w (id int); INSERT INTO t074_w VALUES (1), (2), (3);".into(),
    ))
    .await
    .unwrap();

    let sink: Sink = Arc::default();
    let end = d
        .export_query("SELECT id, id * 2 AS twice FROM t074_w ORDER BY id;".into(), 2, writer_emit(sink.clone(), ExportFormat::Jsonl, || None))
        .await
        .unwrap();
    assert_eq!(end.rows, 3);
    assert_eq!(finish(sink), "{\"id\":\"1\",\"twice\":\"2\"}\n{\"id\":\"2\",\"twice\":\"4\"}\n{\"id\":\"3\",\"twice\":\"6\"}\n");

    // Two statements: refused by the server's parser before anything runs.
    let e = d
        .export_query("SELECT 1; DELETE FROM t074_w".into(), 10, writer_emit(Arc::default(), ExportFormat::Csv, || None))
        .await
        .unwrap_err();
    assert!(e.message.contains("exactly one statement"), "{}", e.message);

    // One writing statement: refused by the database, not by us.
    let e = d
        .export_query("DELETE FROM t074_w RETURNING id".into(), 10, writer_emit(Arc::default(), ExportFormat::Csv, || None))
        .await
        .unwrap_err();
    assert!(e.message.contains("read-only"), "{}", e.message);

    let left = d.raw(RawRequest::Script("SELECT count(*) FROM t074_w".into())).await.unwrap();
    assert_eq!(left.first_rows().unwrap().1[0][0], WireValue::Text("3".into()));
}
