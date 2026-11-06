//! Live integration tests against a real Postgres.
//!
//! These are the tests that actually prove T-016/T-017/T-018 — unit tests can
//! only check the SQL we generate, not what the server does with it. R-014 in
//! particular is unfalsifiable without a server holding an extension type.
//!
//! Point `SLUICE_TEST_PG` at an instance, or run the container the phase used:
//!
//! ```sh
//! docker run -d --name sluice-pg -e POSTGRES_PASSWORD=sluice_dev \
//!   -e POSTGRES_DB=sluice_dev -p 55432:5432 postgis/postgis:17-3.5
//! ```
//!
//! Skipped (not failed) when no server is reachable, so `cargo test` stays green
//! on a machine without Docker. T-069 is the task that puts this in CI, where
//! skipping will no longer be acceptable.

use sluice_core::{
    BrowseRequest, Driver, NodeId, OutcomeBody, RawRequest, Secret, TxDisposition, WireValue,
};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};
use std::sync::Arc;

fn spec() -> ConnectSpec {
    ConnectSpec {
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(55432),
        user: "postgres".into(),
        password: Some(Secret::new("sluice_dev".into())),
        dbname: "sluice_dev".into(),
        // A loopback container has no certificate worth verifying; T-021's
        // default remains Verify everywhere else.
        tls: TlsMode::Disabled,
        application_name: "sluice-tests".into(),
    }
}

async fn driver() -> Option<Arc<PgDriver>> {
    match PgDriver::connect(spec()).await {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("SKIP: no Postgres reachable ({e})");
            None
        }
    }
}

macro_rules! skip_unless {
    ($d:expr) => {
        match $d {
            Some(d) => d,
            None => return,
        }
    };
}

async fn exec(d: &Arc<PgDriver>, sql: &str) {
    let r = d.raw(RawRequest::Script(sql.into())).await.expect("setup sql");
    for o in &r.outcomes {
        if let OutcomeBody::Error { error } = &o.body {
            panic!("setup failed: {error} on: {sql}");
        }
    }
}

fn rel(name: &str) -> NodeId {
    NodeId { kind: "table".into(), path: vec!["public".into(), name.into()] }
}

fn browse_req(name: &str) -> BrowseRequest {
    BrowseRequest {
        relation: rel(name),
        filter: None,
        sort: vec![],
        limit: 100,
        cursor: None,
    }
}

fn cell<'a>(
    cols: &'a [sluice_core::ColumnDesc],
    row: &'a [WireValue],
    name: &str,
) -> &'a WireValue {
    let i = cols.iter().position(|c| c.name == name).unwrap_or_else(|| {
        panic!("no column `{name}` in {:?}", cols.iter().map(|c| &c.name).collect::<Vec<_>>())
    });
    &row[i]
}

// ---------------------------------------------------------------- T-008

#[tokio::test]
async fn navigator_tree_is_built_from_driver_declared_kinds() {
    let d = skip_unless!(driver().await);

    let roots = d.navigator_children(None).await.expect("roots");
    assert!(roots.iter().any(|n| n.label == "public"), "expected a public schema");
    assert!(roots.iter().all(|n| n.id.kind == "schema"));
    assert!(!roots.iter().any(|n| n.label.starts_with("pg_")), "catalog schemas filtered");

    exec(&d, "drop table if exists nav_t; create table nav_t(a int); \
              drop view if exists nav_v; create view nav_v as select 1 as x;").await;

    let schema = roots.into_iter().find(|n| n.label == "public").unwrap();
    let kids = d.navigator_children(Some(schema.id)).await.expect("children");

    let t = kids.iter().find(|n| n.label == "nav_t").expect("nav_t present");
    let v = kids.iter().find(|n| n.label == "nav_v").expect("nav_v present");
    assert_eq!(t.id.kind, "table");
    assert_eq!(v.id.kind, "view");

    // Every kind the tree emits must be one the descriptor declared, or the
    // React layer has nothing to render it with (D-029).
    let declared: Vec<String> = d.capabilities().node_kinds.iter().map(|k| k.id.clone()).collect();
    for n in &kids {
        assert!(declared.contains(&n.id.kind), "undeclared kind {}", n.id.kind);
    }
}

// ---------------------------------------------------------------- T-016

#[tokio::test]
async fn exact_types_survive_the_round_trip_losslessly() {
    let d = skip_unless!(driver().await);

    exec(
        &d,
        "drop table if exists fidelity;
         create table fidelity(
            id            int primary key,
            big           int8,
            exact         numeric(40,20),
            ts            timestamptz,
            uid           uuid,
            flag          boolean,
            small         int4,
            approx        float8,
            blob          bytea,
            doc           jsonb
         );
         insert into fidelity values (
            1,
            9223372036854775807,
            1.00000000000000000001,
            '2026-08-02 12:34:56.123456+05:30',
            '0e984725-c51c-4bf4-9960-e1c80e27aba0',
            true,
            42,
            0.1,
            '\\xdeadbeef'::bytea,
            '{\"k\": 1}'::jsonb
         );",
    )
    .await;

    let r = d.browse(browse_req("fidelity")).await.expect("browse");
    let (cols, rows) = r.first_rows().expect("rows outcome");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];

    // The headline case: int8 max exceeds f64's exact integer range. If this
    // ever comes back as WireValue::Float, D-006 has been violated.
    assert_eq!(
        cell(cols, row, "big"),
        &WireValue::Numeric("9223372036854775807".into()),
        "int8 must cross as an exact tagged string, never a float"
    );

    assert_eq!(
        cell(cols, row, "exact"),
        &WireValue::Numeric("1.00000000000000000001".into()),
        "numeric must keep every digit"
    );

    match cell(cols, row, "ts") {
        WireValue::Timestamp(s) => {
            assert!(s.contains("12:34:56.123456") || s.contains("07:04:56.123456"), "{s}");
            assert!(s.contains('+') || s.contains('-'), "offset must survive: {s}");
        }
        other => panic!("timestamptz should be a tagged string, got {other:?}"),
    }

    assert_eq!(
        cell(cols, row, "uid"),
        &WireValue::Uuid("0e984725-c51c-4bf4-9960-e1c80e27aba0".into())
    );

    // Only these three shapes go raw, per D-006.
    assert_eq!(cell(cols, row, "flag"), &WireValue::Bool(true));
    assert_eq!(cell(cols, row, "small"), &WireValue::Int(42));
    assert_eq!(cell(cols, row, "approx"), &WireValue::Float(0.1));

    // Binary crosses as base64 — a tagged string, not a JSON array of ints.
    assert_eq!(cell(cols, row, "blob"), &WireValue::Bytes("3q2+7w==".into()));

    // Per-column descriptors carry the engine's own type names (T-016).
    let by = |n: &str| cols.iter().find(|c| c.name == n).unwrap();
    assert_eq!(by("big").type_name, "int8");
    assert_eq!(by("exact").type_name, "numeric");
    assert_eq!(by("uid").type_name, "uuid");
    assert!(!by("big").derived, "Postgres has a catalog; nothing is derived");
}

#[tokio::test]
async fn null_is_distinct_from_a_present_value() {
    let d = skip_unless!(driver().await);
    exec(
        &d,
        "drop table if exists nulls; create table nulls(a text, b int8);
         insert into nulls values (null, null), ('', 0);",
    )
    .await;

    let r = d.browse(browse_req("nulls")).await.expect("browse");
    let (cols, rows) = r.first_rows().unwrap();
    let nulled = rows.iter().find(|r| cell(cols, r, "a") == &WireValue::Null).expect("null row");
    assert_eq!(cell(cols, nulled, "b"), &WireValue::Null);

    let empty = rows.iter().find(|r| cell(cols, r, "a") == &WireValue::Text(String::new()));
    assert!(empty.is_some(), "empty string must not collapse into NULL");
}

// ---------------------------------------------------------------- T-017 / R-014

#[tokio::test]
async fn exotic_types_browse_without_error_and_arrive_as_tagged_strings() {
    let d = skip_unless!(driver().await);

    // Everything here has no `FromSql` impl in tokio-postgres. Under R-014's
    // failure mode the whole table would refuse to browse.
    exec(
        &d,
        "drop table if exists exotic;
         drop type if exists mood cascade;
         drop domain if exists positive_int cascade;
         create type mood as enum ('sad','ok','great');
         create domain positive_int as int check (value > 0);
         create table exotic(
            id     int primary key,
            tsv    tsvector,
            feel   mood,
            pos    positive_int,
            arr    int[],
            rng    int4range,
            net    inet,
            mac    macaddr,
            bits   bit(8),
            iv     interval
         );
         insert into exotic values (
            1,
            to_tsvector('english','the quick brown fox'),
            'great',
            7,
            '{1,2,3}',
            '[1,10)',
            '192.168.1.1',
            '08:00:2b:01:02:03',
            B'10101010',
            '1 day 02:03:04'
         );",
    )
    .await;

    let r = d
        .browse(browse_req("exotic"))
        .await
        .expect("R-014: a table with extension types must browse, not fail");
    let (cols, rows) = r.first_rows().expect("rows");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];

    // Each arrives as a tagged string carrying the engine's own type name.
    for (col, expect_type) in [
        ("tsv", "tsvector"),
        ("feel", "mood"),
        ("arr", "_int4"),
        ("rng", "int4range"),
        ("net", "inet"),
        ("mac", "macaddr"),
        ("bits", "bit"),
    ] {
        let desc = cols.iter().find(|c| c.name == col).unwrap();
        assert_eq!(desc.type_name, expect_type, "column {col}");
        match cell(cols, row, col) {
            WireValue::Opaque { type_name, rendered } => {
                assert_eq!(type_name, expect_type);
                assert!(!rendered.is_empty(), "{col} rendered empty");
            }
            other => panic!("{col} should be a tagged Opaque string, got {other:?}"),
        }
    }

    // A domain over int resolves to its base type's rendering, which is fine —
    // the point is that it browses at all.
    assert!(matches!(
        cell(cols, row, "pos"),
        WireValue::Int(7) | WireValue::Numeric(_) | WireValue::Opaque { .. }
    ));

    match cell(cols, row, "feel") {
        WireValue::Opaque { rendered, .. } => assert_eq!(rendered, "great"),
        other => panic!("enum should render its label, got {other:?}"),
    }
}

#[tokio::test]
async fn postgis_geometry_browses_when_the_extension_is_available() {
    let d = skip_unless!(driver().await);

    let created = d
        .raw(RawRequest::Script("create extension if not exists postgis;".into()))
        .await
        .map(|r| !r.outcomes.iter().any(|o| o.is_error()))
        .unwrap_or(false);
    if !created {
        eprintln!("SKIP: PostGIS not available in this server");
        return;
    }

    exec(
        &d,
        "drop table if exists geo;
         create table geo(id int primary key, shape geometry(Point,4326));
         insert into geo values (1, ST_SetSRID(ST_MakePoint(-122.3, 47.6), 4326));",
    )
    .await;

    let r = d.browse(browse_req("geo")).await.expect("R-014: geometry must browse");
    let (cols, rows) = r.first_rows().unwrap();
    let desc = cols.iter().find(|c| c.name == "shape").unwrap();
    assert_eq!(desc.type_name, "geometry");
    match cell(cols, &rows[0], "shape") {
        WireValue::Opaque { type_name, rendered } => {
            assert_eq!(type_name, "geometry");
            assert!(!rendered.is_empty());
        }
        other => panic!("geometry should be a tagged string, got {other:?}"),
    }
}

// ---------------------------------------------------------------- T-018 / R-008

#[tokio::test]
async fn oversized_values_become_placeholders_and_are_never_inlined() {
    let d = skip_unless!(driver().await);

    exec(
        &d,
        "drop table if exists big_values;
         create table big_values(id int primary key, body text, blob bytea);
         insert into big_values values (1, repeat('x', 2000000), decode(repeat('ff', 500000),'hex'));
         insert into big_values values (2, 'small', '\\x01'::bytea);",
    )
    .await;

    let capped = d.clone().with_ceiling(1024);
    let r = capped.browse(browse_req("big_values")).await.expect("browse");
    let (cols, rows) = r.first_rows().unwrap();

    let big = rows.iter().find(|r| cell(cols, r, "id") == &WireValue::Int(1)).unwrap();
    match cell(cols, big, "body") {
        WireValue::Oversized { type_name, byte_len, handle } => {
            assert_eq!(type_name, "text");
            assert_eq!(*byte_len, 2_000_000, "exact byte length, not an estimate");
            assert!(handle.is_some(), "a table row has a ctid, so a handle is available");
        }
        other => panic!("2MB text must not be inlined, got {other:?}"),
    }
    match cell(cols, big, "blob") {
        WireValue::Oversized { byte_len, .. } => {
            // True stored bytes, NOT the inflated base64 length.
            assert_eq!(*byte_len, 500_000);
        }
        other => panic!("500KB bytea must not be inlined, got {other:?}"),
    }

    // A small value in the same column is unaffected.
    let small = rows.iter().find(|r| cell(cols, r, "id") == &WireValue::Int(2)).unwrap();
    assert_eq!(cell(cols, small, "body"), &WireValue::Text("small".into()));

    // And with a ceiling above the value, it inlines normally.
    let roomy = d.with_ceiling(4 * 1024 * 1024);
    let r2 = roomy.browse(browse_req("big_values")).await.unwrap();
    let (c2, rows2) = r2.first_rows().unwrap();
    let big2 = rows2.iter().find(|r| cell(c2, r, "id") == &WireValue::Int(1)).unwrap();
    assert!(matches!(cell(c2, big2, "body"), WireValue::Text(s) if s.len() == 2_000_000));
}

// ---------------------------------------------------------------- T-009

#[tokio::test]
async fn structured_browse_compiles_filters_sorts_and_cursors() {
    let d = skip_unless!(driver().await);

    exec(
        &d,
        "drop table if exists browse_t;
         create table browse_t(id int primary key, name text, amount numeric);
         insert into browse_t values
            (1,'Alpha', 10.5), (2,'beta', 20.25), (3,'GAMMA', 30.125), (4, null, 40);",
    )
    .await;

    // contains is case-insensitive (R-009), and must be declared so.
    let mut req = browse_req("browse_t");
    req.filter = Some(sluice_core::FilterTree::Pred(sluice_core::Predicate {
        column: "name".into(),
        op: "contains".into(),
        value: WireValue::Text("A".into()),
    }));
    req.sort = vec![sluice_core::SortKey { column: "id".into(), descending: false }];
    let r = d.browse(req).await.expect("filtered browse");
    let (cols, rows) = r.first_rows().unwrap();
    let names: Vec<String> = rows
        .iter()
        .map(|r| match cell(cols, r, "name") {
            WireValue::Text(s) => s.clone(),
            v => format!("{v:?}"),
        })
        .collect();
    assert_eq!(names, vec!["Alpha", "beta", "GAMMA"], "ILIKE must match all three");

    // D-029: browse is always exactly one rows outcome.
    assert_eq!(r.outcomes.len(), 1);

    // Keyset cursor pages forward.
    let mut req = browse_req("browse_t");
    req.sort = vec![sluice_core::SortKey { column: "id".into(), descending: false }];
    req.cursor = Some(vec![WireValue::Int(2)]);
    let r = d.browse(req).await.unwrap();
    let (cols, rows) = r.first_rows().unwrap();
    let ids: Vec<&WireValue> = rows.iter().map(|r| cell(cols, r, "id")).collect();
    assert_eq!(ids, vec![&WireValue::Int(3), &WireValue::Int(4)]);
}

// ---------------------------------------------------------------- T-010/T-011/T-012

#[tokio::test]
async fn a_script_yields_one_outcome_per_statement_with_spans() {
    let d = skip_unless!(driver().await);
    exec(&d, "drop table if exists seq_t; create table seq_t(a int);").await;

    let script = "insert into seq_t values (1); select a from seq_t; update seq_t set a = 2;";
    let r = d.raw(RawRequest::Script(script.into())).await.expect("raw");

    assert_eq!(r.outcomes.len(), 3, "one outcome per statement");
    assert!(matches!(r.outcomes[0].body, OutcomeBody::Affected { count: 1, .. }));
    assert!(matches!(r.outcomes[1].body, OutcomeBody::Rows { .. }));
    assert!(matches!(r.outcomes[2].body, OutcomeBody::Affected { count: 1, .. }));

    // Counts agreed, so attribution carries real spans (T-011).
    for (i, o) in r.outcomes.iter().enumerate() {
        match o.attribution {
            sluice_core::Attribution::Span { ordinal, start_utf16, end_utf16 } => {
                assert_eq!(ordinal as usize, i);
                assert!(end_utf16 > start_utf16);
            }
            other => panic!("expected a span for outcome {i}, got {other:?}"),
        }
    }
}

/// R-040's headline case: a dollar-quoted body containing semicolons must reach
/// the server intact. A naive split truncates it into a syntax error.
#[tokio::test]
async fn dollar_quoted_function_bodies_survive_the_batch() {
    let d = skip_unless!(driver().await);

    // Semicolons inside the dollar-quoted body are the whole point: a naive
    // split truncates it and the server sees a syntax error the user never wrote.
    let script = "drop function if exists f_spike(); \
                  create function f_spike() returns int as $$ begin return 1; end; $$ language plpgsql; \
                  select f_spike();";
    let r = d.raw(RawRequest::Script(script.into())).await.expect("raw");

    let errs: Vec<&OutcomeBody> = r.outcomes.iter().map(|o| &o.body)
        .filter(|b| matches!(b, OutcomeBody::Error { .. })).collect();
    assert!(errs.is_empty(), "function body must not be split: {errs:?}");
    assert!(matches!(r.outcomes.last().unwrap().body, OutcomeBody::Rows { .. }));
}

/// R-041: the whole point. Postgres wraps a multi-statement simple query in one
/// implicit transaction, so earlier statements did NOT persist.
#[tokio::test]
async fn a_failed_batch_reports_what_was_rolled_back() {
    let d = skip_unless!(driver().await);
    exec(&d, "drop table if exists rb_t; create table rb_t(a int);").await;

    let script = "insert into rb_t values (1); insert into rb_t values (2); select * from no_such_table;";
    let r = d.raw(RawRequest::Script(script.into())).await.expect("raw returns a result, not Err");

    assert!(r.outcomes.iter().any(|o| o.is_failure()), "the failure must be in the sequence");
    assert!(
        matches!(r.tx.disposition, TxDisposition::RolledBack { .. }),
        "tx disposition must say the batch rolled back, got {:?}",
        r.tx.disposition
    );
    assert!(r.summary().contains("rolled back"), "summary: {}", r.summary());

    // And the database must agree with what we reported.
    let check = d.browse(browse_req("rb_t")).await.unwrap();
    let (_, rows) = check.first_rows().unwrap();
    assert_eq!(rows.len(), 0, "R-041: reporting success for rows that were rolled back is the bug");
}

// ---------------------------------------------------------------- T-066

/// Settles empirically what unit PostgreSQL reports an error position in.
/// D-017 says "byte offset"; the protocol spec says characters. The answer
/// decides whether the editor highlights the right character in a query
/// containing non-ASCII text.
#[tokio::test]
async fn error_position_is_measured_in_characters_not_bytes() {
    let d = skip_unless!(driver().await);

    // 'héllo wörld' is 11 characters but 13 bytes, so a character index and a
    // byte index into this string point at different places from here on.
    let sql = "select 'héllo wörld', from x";

    let r = d.raw(RawRequest::Script(sql.into())).await.expect("raw");
    let err = r
        .outcomes
        .iter()
        .find_map(|o| match &o.body {
            OutcomeBody::Error { error } => Some(error),
            _ => None,
        })
        .expect("a syntax error outcome");

    assert_eq!(err.kind, sluice_core::ErrorKind::SyntaxError);
    assert_eq!(err.payload.code.as_deref(), Some("42601"));
    assert!(err.payload.message.is_some(), "engine text preserved verbatim");

    let pos = err.hints.position.expect("a position hint");

    // Read the statement at the reported offset, in CHARACTERS.
    let at_chars: String = sql.chars().skip(pos.char_offset as usize).collect();
    assert!(
        at_chars.starts_with("from"),
        "as a CHARACTER index the position should land on `from`, landed on {:?}",
        at_chars.chars().take(8).collect::<String>()
    );

    // The same number read as a BYTE index lands somewhere else, which is what
    // makes the distinction load-bearing rather than academic.
    let at_bytes = sql.get(pos.char_offset as usize..);
    assert!(
        at_bytes.is_none_or(|s| !s.starts_with("from")),
        "test proves nothing if both readings agree"
    );

    // All BMP characters here, so the UTF-16 index coincides; the unit test in
    // sluice-core covers the astral case where it does not.
    assert_eq!(pos.utf16_offset, pos.char_offset);
}

#[tokio::test]
async fn constraint_violations_carry_structured_hints() {
    let d = skip_unless!(driver().await);
    exec(
        &d,
        "drop table if exists uniq_t; create table uniq_t(a int primary key);
         insert into uniq_t values (1);",
    )
    .await;

    let r = d.raw(RawRequest::Script("insert into uniq_t values (1);".into())).await.unwrap();
    let err = r
        .outcomes
        .iter()
        .find_map(|o| match &o.body {
            OutcomeBody::Error { error } => Some(error),
            _ => None,
        })
        .expect("a constraint error");

    assert_eq!(err.kind, sluice_core::ErrorKind::ConstraintViolation);
    assert_eq!(err.hints.constraint.as_deref(), Some("uniq_t_pkey"));
    assert_eq!(err.hints.table.as_deref(), Some("uniq_t"));
    assert_eq!(err.hints.retryable, Some(false));
}
