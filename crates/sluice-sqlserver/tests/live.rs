//! T-080 / T-081 against a real SQL Server.
//!
//! Skipped (not failed) when no server is reachable, like the Postgres live
//! tests. Locally: the T-072 container —
//!
//! ```sh
//! docker run -d --name sluice-spike-mssql --platform linux/amd64 \
//!   -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD='Sluice_Spike_2026!' -e MSSQL_PID=Developer \
//!   -p 14333:1433 mcr.microsoft.com/mssql/server:2022-latest
//! ```

use std::sync::Arc;

use sluice_core::{Driver, ErrorKind, NavNode, NodeId, ResetSupport, Secret, TlsMode};
use sluice_sqlserver::{navigator, ConnectSpec, SqlServerDriver};

const PASSWORD_DEFAULT: &str = "Sluice_Spike_2026!";

fn spec(tls: TlsMode) -> ConnectSpec {
    ConnectSpec {
        host: std::env::var("SLUICE_TEST_MSSQL_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("SLUICE_TEST_MSSQL_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(14333),
        user: "sa".into(),
        password: Some(Secret::new(std::env::var("SLUICE_TEST_MSSQL_PASSWORD").unwrap_or_else(|_| PASSWORD_DEFAULT.into()))),
        database: "master".into(),
        tls,
        application_name: "sluice-test".into(),
    }
}

async fn connect(budget: usize) -> Option<Arc<SqlServerDriver>> {
    match SqlServerDriver::connect(spec(TlsMode::AcceptInvalidCerts), budget).await {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("SKIP: no SQL Server reachable ({e})");
            None
        }
    }
}

async fn scalar(d: &SqlServerDriver, dirty: bool, sql: &str) -> Option<String> {
    let sql = sql.to_string();
    d.with_lease(dirty, async |c| {
        let rows = c.simple_query(sql).await.map_err(|e| sluice_sqlserver::error_map::map_error(&e))?;
        let rows = rows.into_first_result().await.map_err(|e| sluice_sqlserver::error_map::map_error(&e))?;
        Ok(rows.first().and_then(|r| r.try_get::<&str, _>(0).ok().flatten().map(str::to_string)))
    })
    .await
    .unwrap()
}

async fn exec(d: &SqlServerDriver, sql: &str) {
    let sql = sql.to_string();
    d.with_lease(true, async |c| {
        c.simple_query(sql)
            .await
            .map_err(|e| sluice_sqlserver::error_map::map_error(&e))?
            .into_results()
            .await
            .map_err(|e| sluice_sqlserver::error_map::map_error(&e))?;
        Ok(())
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn connects_over_tls_and_publishes_its_capabilities() {
    let Some(d) = connect(5).await else { return };
    let caps = d.capabilities();
    assert_eq!(caps.engine, "sqlserver");
    assert_eq!(caps.reset, ResetSupport::ReopenOnly, "T-072: no reachable sp_reset_connection");
    assert!(caps.server_version.as_deref().is_some_and(|v| v.split('.').count() >= 3), "{:?}", caps.server_version);
    assert_eq!(caps.flavour.as_deref(), Some("SQL Server"));
    assert!(caps.string_collation.is_some());
    assert_eq!(d.connection_budget(), 5);
    assert!(d.max_connections().is_some_and(|m| m > 0));
    // `sa` holds every database-wide write permission.
    assert!(matches!(caps.read_only, sluice_core::ReadOnlyGuarantee::NotEnforced { .. }), "{:?}", caps.read_only);
    assert!(caps.catalog.features.iter().all(|f| f.available), "{:?}", caps.catalog);
}

#[tokio::test]
async fn tls_disabled_connects_without_encryption() {
    if connect(1).await.is_none() {
        return;
    }
    let d = SqlServerDriver::connect(spec(TlsMode::Disabled), 1).await.expect("an unforced server accepts no TLS");
    let enc = scalar(&d, false, "SELECT CAST(encrypt_option AS nvarchar(8)) FROM sys.dm_exec_connections WHERE session_id = @@SPID").await;
    assert_eq!(enc.as_deref(), Some("FALSE"));
}

#[tokio::test]
async fn verify_refuses_the_self_signed_certificate_and_says_what_to_do() {
    if connect(1).await.is_none() {
        return;
    }
    let e = SqlServerDriver::connect(spec(TlsMode::Verify), 1).await.err().expect("self-signed cert refused");
    assert_eq!(e.kind, ErrorKind::ConnectionFailed);
    assert!(e.message.contains("Accept invalid certificates"), "{}", e.message);
}

#[tokio::test]
async fn a_wrong_password_is_an_auth_failure_that_never_echoes_it() {
    if connect(1).await.is_none() {
        return;
    }
    let mut s = spec(TlsMode::AcceptInvalidCerts);
    s.password = Some(Secret::new("wrong-Hunter2!".into()));
    let e = SqlServerDriver::connect(s, 1).await.err().expect("refused");
    assert_eq!(e.kind, ErrorKind::AuthFailed, "{e:?}");
    assert!(!format!("{e:?}").contains("wrong-Hunter2!"));
}

/// D-028 / T-040 / R-036: a clean lease goes back as-is (no reconnect), and a
/// dirty one is closed so its session state can never reach the next caller.
#[tokio::test]
async fn only_dirtied_connections_are_reopened_and_their_state_does_not_leak() {
    let Some(d) = connect(1).await else { return };
    let conn_id = "SELECT CAST(connection_id AS nvarchar(64)) FROM sys.dm_exec_connections WHERE session_id = @@SPID";

    let before = d.pool_stats();
    let a = scalar(&d, false, conn_id).await;
    let b = scalar(&d, false, conn_id).await;
    assert_eq!(a, b, "clean leases share the one pooled connection");
    let mid = d.pool_stats();
    assert_eq!(mid.reopens, before.reopens, "structured traffic never reconnects");
    assert_eq!(mid.clean_returns, before.clean_returns + 2);

    // Dirty the session on a lease that says so.
    exec(&d, "SET LOCK_TIMEOUT 1234; CREATE TABLE #t080 (x int);").await;
    let after = d.pool_stats();
    assert_eq!(after.reopens, mid.reopens + 1, "the dirty lease was closed, not returned");

    let c = scalar(&d, false, conn_id).await;
    assert_ne!(c, a, "a fresh connection replaced the dirtied one");
    assert_eq!(scalar(&d, false, "SELECT CAST(@@LOCK_TIMEOUT AS nvarchar(16))").await.as_deref(), Some("-1"));
    assert_eq!(scalar(&d, false, "SELECT CAST(OBJECT_ID('tempdb..#t080') AS nvarchar(16))").await, None);
}

fn names(nodes: &[NavNode], kind: &str) -> Vec<String> {
    nodes.iter().filter(|n| n.id.kind == kind).map(|n| n.label.clone()).collect()
}

/// T-081: databases → schemas → tables and views → columns and indexes, one
/// level per expand, cached until an explicit refresh.
#[tokio::test]
async fn the_navigator_walks_the_catalog_one_level_at_a_time() {
    let Some(d) = connect(3).await else { return };
    exec(
        &d,
        "IF DB_ID('sluice_t081') IS NOT NULL BEGIN ALTER DATABASE sluice_t081 SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE sluice_t081; END;
         CREATE DATABASE sluice_t081;",
    )
    .await;
    exec(
        &d,
        "EXEC sluice_t081.sys.sp_executesql N'CREATE SCHEMA app';
         EXEC sluice_t081.sys.sp_executesql N'CREATE TABLE app.orders (id int PRIMARY KEY, note nvarchar(50) NULL, total decimal(19,4) NOT NULL)';
         EXEC sluice_t081.sys.sp_executesql N'CREATE VIEW app.big AS SELECT id FROM app.orders WHERE total > 100';",
    )
    .await;

    let roots = d.navigator_children(None).await.unwrap();
    let dbs = names(&roots, "database");
    assert!(dbs.contains(&"sluice_t081".to_string()), "{dbs:?}");
    assert!(!dbs.contains(&"tempdb".to_string()), "system databases are hidden: {dbs:?}");

    let db = NodeId { kind: "database".into(), path: vec!["sluice_t081".into()] };
    let schemas = names(&d.navigator_children(Some(db)).await.unwrap(), "schema");
    assert!(schemas.contains(&"app".to_string()) && schemas.contains(&"dbo".to_string()), "{schemas:?}");
    assert!(!schemas.iter().any(|s| s == "sys" || s.starts_with("db_")), "{schemas:?}");

    let schema = NodeId { kind: "schema".into(), path: vec!["sluice_t081".into(), "app".into()] };
    let rels = d.navigator_children(Some(schema.clone())).await.unwrap();
    assert_eq!(names(&rels, "table"), vec!["orders"]);
    assert_eq!(names(&rels, "view"), vec!["big"]);

    let table = rels.iter().find(|n| n.label == "orders").unwrap().id.clone();
    assert_eq!(table.path, vec!["sluice_t081", "app", "orders"]);
    let parts = d.navigator_children(Some(table)).await.unwrap();
    let cols = names(&parts, "column");
    assert_eq!(cols, vec!["id  int · not null", "note  nvarchar(50)", "total  decimal(19,4) · not null"], "{cols:?}");
    let idx = names(&parts, "index");
    assert_eq!(idx.len(), 1, "{idx:?}");
    assert!(idx[0].contains("primary key"), "{idx:?}");

    // Lazy and cached: external DDL is invisible until an explicit refresh.
    exec(&d, "EXEC sluice_t081.sys.sp_executesql N'CREATE TABLE app.later (x int)';").await;
    assert_eq!(names(&d.navigator_children(Some(schema.clone())).await.unwrap(), "table"), vec!["orders"]);
    d.refresh_catalog();
    assert_eq!(names(&d.navigator_children(Some(schema)).await.unwrap(), "table"), vec!["later", "orders"]);

    exec(&d, "ALTER DATABASE sluice_t081 SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE sluice_t081;").await;
}

/// D-016: a database the login cannot open is not listed (and so cannot fail
/// on expand); a vanished relation is a note in place, not an error.
#[tokio::test]
async fn a_vanished_relation_is_a_note_not_an_error() {
    let Some(d) = connect(2).await else { return };
    let gone = NodeId { kind: "table".into(), path: vec!["master".into(), "dbo".into(), "no_such_table_t081".into()] };
    let nodes = d.navigator_children(Some(gone)).await.unwrap();
    assert_eq!(nodes.len(), 1);
    assert!(navigator::is_notice(&nodes[0]) && nodes[0].label.contains("refresh"), "{nodes:?}");
}

// ---- T-082: structured browse ------------------------------------------------

use sluice_core::{BrowseBatch, BrowseRequest, FilterTree, PageInfo, Predicate, SortKey, WireValue};

/// A fresh database for one test, so tests running in parallel never share
/// tables. Statements run on a dirty lease, so `USE` never leaks.
async fn fresh_db(d: &SqlServerDriver, db: &str, ddl: &str) {
    exec(
        d,
        &format!(
            "IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END;
             CREATE DATABASE [{db}];"
        ),
    )
    .await;
    exec(d, &format!("USE [{db}]; {ddl}")).await;
}

async fn drop_db(d: &SqlServerDriver, db: &str) {
    exec(d, &format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}];")).await;
}

fn rel(db: &str, kind: &str, name: &str) -> NodeId {
    NodeId { kind: kind.into(), path: vec![db.into(), "dbo".into(), name.into()] }
}

fn req(relation: NodeId) -> BrowseRequest {
    BrowseRequest { relation, filter: None, sort: vec![], limit: 100, cursor: None, offset: None }
}

fn cell<'a>(r: &'a sluice_core::ExecutionResult, row: usize, col: &str) -> &'a WireValue {
    let (cols, rows) = r.first_rows().expect("one rows outcome");
    let i = cols.iter().position(|c| c.name == col).unwrap_or_else(|| panic!("no column {col}"));
    &rows[row][i]
}

/// T-082 acceptance: each SQL Server type reaches the wire losslessly —
/// MONEY as exact text, exact numerics as `Numeric`, the types that panic
/// inside tiberius (T-072) as `Unsupported`, oversized values as placeholders.
#[tokio::test]
async fn every_type_reaches_the_wire_losslessly() {
    let Some(d) = connect(2).await else { return };
    let db = "sluice_t082_types";
    fresh_db(
        &d,
        db,
        "CREATE TABLE dbo.types (
            id int PRIMARY KEY,
            m money, sm smallmoney, dec decimal(38,20), num numeric(10,0), big bigint, i int, si smallint, ti tinyint, b bit,
            f float, r real,
            nv nvarchar(50), vc varchar(50), nt ntext, tx text, x xml, vmax varchar(max),
            vb varbinary(16), img image, rv rowversion,
            u uniqueidentifier,
            d2 datetime2(7), dto datetimeoffset(7), dt date, tm time(7), dtm datetime, sdt smalldatetime,
            sv sql_variant, geo geography, gm geometry, h hierarchyid
         );
         INSERT INTO dbo.types (id, m, sm, dec, num, big, i, si, ti, b, f, r, nv, vc, nt, tx, x, vmax, vb, img, u,
                                d2, dto, dt, tm, dtm, sdt, sv, geo, gm, h)
         VALUES (1, 922337203685477.5807, 214748.3647, 1.00000000000000000001, 9999999999, 9223372036854775807,
                 -2147483648, -32768, 255, 1, 0.1, 0.5,
                 N'héllo 日本', 'plain', N'legacy n', 'legacy', N'<a b=\"1\">x</a>', REPLICATE(CAST('x' AS varchar(max)), 300000),
                 0x00FF00, 0x0102, '6F9619FF-8B86-D011-B42D-00C04FC964FF',
                 '2024-01-02T03:04:05.1234567', '2024-01-02T03:04:05.1234567+05:30', '2024-01-02', '03:04:05.1234567',
                 '2024-01-02T03:04:05.003', '2024-01-02T03:04:00',
                 CAST(42 AS int), geography::Point(47.65, -122.34, 4326), geometry::STGeomFromText('POINT (1 2)', 0),
                 hierarchyid::Parse('/1/2/'));
         INSERT INTO dbo.types (id) VALUES (2);",
    )
    .await;

    let r = d.browse(req(rel(db, "table", "types"))).await.expect("browse");
    assert_eq!(r.outcomes.len(), 1, "D-029: exactly one rows outcome");
    let text = |s: &str| WireValue::Text(s.into());
    let num = |s: &str| WireValue::Numeric(s.into());
    let ts = |s: &str| WireValue::Timestamp(s.into());
    let expect: Vec<(&str, WireValue)> = vec![
        ("id", WireValue::Int(1)),
        // T-072: tiberius's own decode gives 922337203685477.6.
        ("m", num("922337203685477.5807")),
        ("sm", num("214748.3647")),
        ("dec", num("1.00000000000000000001")),
        ("num", num("9999999999")),
        ("big", num("9223372036854775807")),
        ("i", WireValue::Int(-2147483648)),
        ("si", WireValue::Int(-32768)),
        ("ti", WireValue::Int(255)),
        ("b", WireValue::Bool(true)),
        ("f", WireValue::Float(0.1)),
        ("r", WireValue::Float(0.5)),
        ("nv", text("héllo 日本")),
        ("vc", text("plain")),
        ("nt", text("legacy n")),
        ("tx", text("legacy")),
        ("x", text("<a b=\"1\">x</a>")),
        ("vmax", WireValue::Oversized { type_name: "varchar".into(), byte_len: 300_000, handle: None }),
        ("vb", WireValue::Bytes("AP8A".into())),
        ("img", WireValue::Bytes("AQI=".into())),
        ("u", WireValue::Uuid("6F9619FF-8B86-D011-B42D-00C04FC964FF".into())),
        ("d2", ts("2024-01-02T03:04:05.1234567")),
        ("dto", ts("2024-01-02T03:04:05.1234567+05:30")),
        ("dt", ts("2024-01-02")),
        ("tm", ts("03:04:05.1234567")),
        ("dtm", ts("2024-01-02T03:04:05.003")),
        ("sdt", ts("2024-01-02T03:04:00")),
        ("sv", WireValue::Unsupported { type_name: "sql_variant".into(), rendered: "42".into() }),
        ("gm", WireValue::Unsupported { type_name: "geometry".into(), rendered: "POINT (1 2)".into() }),
        ("h", WireValue::Unsupported { type_name: "hierarchyid".into(), rendered: "/1/2/".into() }),
    ];
    for (col, want) in &expect {
        assert_eq!(cell(&r, 0, col), want, "{col}");
    }
    assert!(
        matches!(cell(&r, 0, "geo"), WireValue::Unsupported { rendered, .. } if rendered.starts_with("POINT")),
        "{:?}",
        cell(&r, 0, "geo")
    );
    assert!(matches!(cell(&r, 0, "rv"), WireValue::Bytes(b) if b.len() == 12), "rowversion is 8 bytes: {:?}", cell(&r, 0, "rv"));

    // Three cell states: a NULL in every type is NULL, never a placeholder.
    let (cols, rows) = r.first_rows().unwrap();
    for (c, v) in cols.iter().zip(&rows[1]) {
        if c.name != "id" && c.name != "rv" {
            assert_eq!(v, &WireValue::Null, "{}", c.name);
        }
    }
    let m = cols.iter().find(|c| c.name == "m").unwrap();
    assert_eq!(m.type_class, sluice_core::TypeClass::ExactNumeric);
    assert_eq!(m.exact, Some(sluice_core::ExactDomain::Decimal { precision: Some(19), scale: Some(4) }));
    assert!(matches!(r.page, Some(PageInfo::Keyset { ref key, .. }) if key == &["id".to_string()]));

    // A lower ceiling turns smaller values into placeholders too.
    let tight = d.clone().with_ceiling(4);
    let r = tight.browse(req(rel(db, "table", "types"))).await.unwrap();
    assert!(matches!(cell(&r, 0, "nv"), WireValue::Oversized { byte_len: 16, .. }), "{:?}", cell(&r, 0, "nv"));
    assert_eq!(cell(&r, 0, "vb"), &WireValue::Bytes("AP8A".into()), "3 bytes is under 4");

    // No panic reached tiberius; the pooled connection still works.
    assert_eq!(scalar(&d, false, "SELECT CAST(1 AS nvarchar(1))").await.as_deref(), Some("1"));
    drop_db(&d, db).await;
}

fn pred(column: &str, op: &str, value: WireValue) -> FilterTree {
    FilterTree::Pred(Predicate { case_insensitive: false, column: column.into(), op: op.into(), value })
}

async fn ids(d: &SqlServerDriver, relation: NodeId, filter: FilterTree) -> Vec<i64> {
    let mut r = req(relation);
    r.filter = Some(filter.clone());
    let res = d.browse(r).await.unwrap_or_else(|e| panic!("{filter:?}: {e}"));
    let (cols, rows) = res.first_rows().unwrap();
    let i = cols.iter().position(|c| c.name == "id").unwrap();
    rows.iter()
        .map(|row| match &row[i] {
            WireValue::Int(n) => *n,
            other => panic!("{other:?}"),
        })
        .collect()
}

/// D-005: every core operator runs against a real server with its values
/// bound — a quote or a wildcard in a value matches literally.
#[tokio::test]
async fn every_core_filter_operator_runs_with_bound_values() {
    let Some(d) = connect(2).await else { return };
    let db = "sluice_t082_filters";
    fresh_db(
        &d,
        db,
        "CREATE TABLE dbo.people (id int PRIMARY KEY, name nvarchar(50) NULL, code varchar(20) NOT NULL,
                                  price money NOT NULL, at datetime NOT NULL, big bigint NOT NULL, blob varbinary(8) NULL);
         INSERT INTO dbo.people VALUES
           (1, N'Alice', 'a-1', 1.2345, '2024-01-01T00:00:00.003', 9223372036854775807, 0x00FF00),
           (2, N'o''brien''); DROP TABLE people; --', 'b-2', 10.0000, '2024-02-01T00:00:00', 1, NULL),
           (3, N'50%_off', 'c-3', 922337203685477.5807, '2024-03-01T00:00:00', 2, NULL),
           (4, NULL, 'd-4', 0, '2024-04-01T00:00:00', 3, NULL),
           (5, N'ALICE', 'e-5', -5.5, '2024-05-01T00:00:00', 4, NULL);",
    )
    .await;
    let t = rel(db, "table", "people");
    let text = |s: &str| WireValue::Text(s.into());
    let list = WireValue::Array;
    let cases: Vec<(FilterTree, Vec<i64>)> = vec![
        // The container's default collation is case-insensitive.
        (pred("name", "eq", text("alice")), vec![1, 5]),
        (pred("name", "eq", text("o'brien'); DROP TABLE people; --")), vec![2]),
        (pred("id", "ne", WireValue::Int(1)), vec![2, 3, 4, 5]),
        (pred("id", "lt", WireValue::Int(2)), vec![1]),
        (pred("id", "lte", WireValue::Int(2)), vec![1, 2]),
        (pred("id", "gt", WireValue::Int(4)), vec![5]),
        (pred("id", "gte", WireValue::Int(4)), vec![4, 5]),
        (pred("id", "in", list(vec![WireValue::Int(1), WireValue::Int(3)])), vec![1, 3]),
        (pred("id", "in", list(vec![])), vec![]),
        (pred("id", "between", list(vec![WireValue::Int(2), WireValue::Int(4)])), vec![2, 3, 4]),
        (pred("name", "is_null", WireValue::Null), vec![4]),
        (pred("name", "is_not_null", WireValue::Null), vec![1, 2, 3, 5]),
        (pred("name", "contains", text("%_")), vec![3]),
        (pred("name", "starts_with", text("o'")), vec![2]),
        (pred("name", "ends_with", text("ice")), vec![1, 5]),
        // Exact numerics: compared exactly, never through a float.
        (pred("price", "eq", WireValue::Numeric("922337203685477.5807".into())), vec![3]),
        (pred("price", "gt", WireValue::Numeric("1.23449".into())), vec![1, 2, 3]),
        (pred("big", "eq", WireValue::Numeric("9223372036854775807".into())), vec![1]),
        // datetime's 1/300 s ticks: the value as read equals itself.
        (pred("at", "eq", WireValue::Timestamp("2024-01-01T00:00:00.003".into())), vec![1]),
        (pred("blob", "eq", WireValue::Bytes("AP8A".into())), vec![1]),
        (
            FilterTree::Or(vec![
                pred("id", "eq", WireValue::Int(4)),
                FilterTree::And(vec![pred("code", "starts_with", text("a")), pred("price", "lt", WireValue::Numeric("2".into()))]),
            ]),
            vec![1, 4],
        ),
    ];
    for (f, want) in cases {
        assert_eq!(ids(&d, t.clone(), f.clone()).await, want, "{f:?}");
    }
    // The table survived the hostile value.
    assert_eq!(ids(&d, t.clone(), pred("id", "gte", WireValue::Int(1))).await.len(), 5);
    drop_db(&d, db).await;
}

/// D-010: keyset pages on the primary key (a tiebreaker after the sort),
/// views declare OFFSET/FETCH, and both page through every row exactly once.
#[tokio::test]
async fn keyset_and_offset_paging_visit_every_row_once() {
    let Some(d) = connect(2).await else { return };
    let db = "sluice_t082_pages";
    fresh_db(
        &d,
        db,
        "CREATE TABLE dbo.events (id int PRIMARY KEY, at datetime NOT NULL, label varchar(20) NOT NULL);
         WITH n AS (SELECT TOP 25 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_objects)
         INSERT INTO dbo.events SELECT i, DATEADD(ms, (i % 4) * 3, '2024-01-01T00:00:00'), CONCAT('L', i % 3) FROM n;
         EXEC('CREATE VIEW dbo.recent AS SELECT id, label FROM dbo.events WHERE id > 5');",
    )
    .await;

    // Sort on a non-unique datetime (with 1/300 s ticks) descending: the
    // cursor carries `at` and `id` and must bind both back exactly.
    let mut seen = Vec::new();
    let mut r = req(rel(db, "table", "events"));
    r.sort = vec![SortKey { column: "at".into(), descending: true }];
    r.limit = 7;
    for _ in 0..10 {
        let res = d.browse(r.clone()).await.unwrap();
        seen.extend(res.first_rows().unwrap().1.iter().map(|row| format!("{:?}", row[0])));
        match res.page {
            Some(PageInfo::Keyset { key, next_cursor: Some(c) }) => {
                assert_eq!(key, vec!["at".to_string(), "id".to_string()]);
                r.cursor = Some(c);
            }
            Some(PageInfo::Keyset { next_cursor: None, .. }) => break,
            other => panic!("{other:?}"),
        }
    }
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!((seen.len(), unique.len()), (25, 25), "every row once: {seen:?}");

    // A varchar sort column pages in the column's own collation order.
    let mut seen = Vec::new();
    let mut r = req(rel(db, "table", "events"));
    r.sort = vec![SortKey { column: "label".into(), descending: false }];
    r.limit = 4;
    for _ in 0..10 {
        let res = d.browse(r.clone()).await.unwrap();
        seen.extend(res.first_rows().unwrap().1.iter().map(|row| format!("{:?}", row[0])));
        match res.page {
            Some(PageInfo::Keyset { next_cursor: Some(c), .. }) => r.cursor = Some(c),
            _ => break,
        }
    }
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!((seen.len(), unique.len()), (25, 25), "{seen:?}");

    // A view has no primary key: OFFSET/FETCH, declared with the reason.
    let mut seen = 0;
    let mut r = req(rel(db, "view", "recent"));
    r.limit = 8;
    for _ in 0..10 {
        let res = d.browse(r.clone()).await.unwrap();
        seen += res.first_rows().unwrap().1.len();
        match res.page {
            Some(PageInfo::Offset { reason, next_offset }) => {
                assert!(reason.contains("no primary key"), "{reason}");
                match next_offset {
                    Some(n) => r.offset = Some(n),
                    None => break,
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(seen, 20);
    drop_db(&d, db).await;
}

/// T-049 on SQL Server: columns first, then batches; a consumer that stops
/// mid-page gets its connection closed rather than left mid-stream.
#[tokio::test]
async fn streamed_browse_batches_and_stops_cleanly() {
    let Some(d) = connect(1).await else { return };
    let db = "sluice_t082_stream";
    fresh_db(
        &d,
        db,
        "CREATE TABLE dbo.n (id int PRIMARY KEY);
         WITH n AS (SELECT TOP 50 ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_objects)
         INSERT INTO dbo.n SELECT i FROM n;",
    )
    .await;
    let got = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink_got = got.clone();
    let end = d
        .browse_batches(
            req(rel(db, "table", "n")),
            20,
            sluice_core::sink(move |b: BrowseBatch| {
                let g = sink_got.clone();
                async move {
                    g.lock().unwrap().push(match b {
                        BrowseBatch::Columns(c, _) => format!("cols:{}", c.len()),
                        BrowseBatch::Rows(r) => format!("rows:{}", r.len()),
                    });
                    true
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(*got.lock().unwrap(), vec!["cols:1", "rows:20", "rows:20", "rows:10"]);
    assert_eq!(end.rows, 50);
    assert!(matches!(end.page, Some(PageInfo::Keyset { next_cursor: None, .. })));

    let before = d.pool_stats();
    let mut calls = 0;
    let end = d
        .browse_batches(
            req(rel(db, "table", "n")),
            5,
            sluice_core::sink(move |_b: BrowseBatch| {
                calls += 1;
                let go = calls < 3;
                async move { go }
            }),
        )
        .await
        .unwrap();
    assert!(end.stopped);
    assert_eq!(end.rows, 5);
    assert_eq!(d.pool_stats().reopens, before.reopens + 1, "a mid-stream stop closes the connection");
    // And the pool hands out a working connection afterwards.
    assert_eq!(d.browse(req(rel(db, "table", "n"))).await.unwrap().first_rows().unwrap().1.len(), 50);
    drop_db(&d, db).await;
}
