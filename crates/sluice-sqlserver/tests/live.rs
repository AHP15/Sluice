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
