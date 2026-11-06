//! D-028's reset contract, against a real server (T-039, T-040).
//!
//! Every test uses a pool of **one**, so consecutive requests provably run on
//! the same connection — otherwise "the next request saw clean state" could
//! just mean "the next request got a different connection".
//!
//! Skipped (not failed) when no server is reachable, like `live.rs`.

use std::sync::Arc;

use sluice_core::{
    BrowseRequest, Driver, NodeId, OutcomeBody, RawRequest, ResetSupport, Secret, WireValue,
};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

fn spec() -> ConnectSpec {
    ConnectSpec {
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: "postgres".into(),
        password: Some(Secret::new("sluice_dev".into())),
        dbname: "sluice_dev".into(),
        tls: TlsMode::Disabled,
        application_name: "sluice-pool-tests".into(),
        statement_timeout: None,
    }
}

async fn one_connection() -> Option<Arc<PgDriver>> {
    match PgDriver::connect_with_pool_size(spec(), 1).await {
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

/// Run a script; return the first cell of the last rows outcome as text.
async fn scalar(d: &PgDriver, sql: &str) -> Option<String> {
    let r = d.raw(RawRequest::Script(sql.into())).await.expect("raw");
    r.outcomes.iter().rev().find_map(|o| match &o.body {
        OutcomeBody::Rows { rows, .. } => rows.first().and_then(|r| r.first()).map(|v| match v {
            WireValue::Text(s) | WireValue::Numeric(s) => s.clone(),
            WireValue::Null => "NULL".into(),
            other => format!("{other:?}"),
        }),
        _ => None,
    })
}

async fn run(d: &PgDriver, sql: &str) {
    let _ = d.raw(RawRequest::Script(sql.into())).await;
}

async fn pid(d: &PgDriver) -> String {
    scalar(d, "select pg_backend_pid()::text").await.expect("pid")
}

#[tokio::test]
async fn session_settings_do_not_leak_into_the_next_request() {
    let d = skip_unless!(one_connection().await);
    let before = pid(&d).await;

    run(&d, "SET application_name = 'leaked'; SET search_path TO pg_catalog").await;

    assert_eq!(scalar(&d, "SHOW application_name").await.as_deref(), Some("sluice-pool-tests"));
    assert_eq!(scalar(&d, "SHOW search_path").await.as_deref(), Some("\"$user\", public"));
    // Native reset, not a reconnect: the same backend served every request.
    assert_eq!(pid(&d).await, before, "Native reset must not reopen the connection");
}

/// D-028: a connection reaching release mid-transaction is rolled back first,
/// and `DISCARD ALL` is never issued inside a transaction. If it were, it would
/// fail, the reset would count as a reopen, and the pid would change.
#[tokio::test]
async fn an_open_transaction_is_rolled_back_and_the_reset_still_succeeds() {
    let d = skip_unless!(one_connection().await);
    run(&d, "drop table if exists pool_probe; create table pool_probe(x int)").await;
    let before_pid = pid(&d).await;
    let before = d.pool_stats();

    // Leaves an explicit transaction open when the request ends.
    run(&d, "BEGIN; insert into pool_probe values (1)").await;

    assert_eq!(scalar(&d, "select count(*)::text from pool_probe").await.as_deref(), Some("0"));
    let after = d.pool_stats();
    assert_eq!(after.reopens, before.reopens, "the reset failed, so DISCARD ALL ran inside the transaction");
    assert!(after.resets > before.resets);
    assert_eq!(pid(&d).await, before_pid);
}

#[tokio::test]
async fn an_aborted_transaction_is_recovered_by_the_rollback() {
    let d = skip_unless!(one_connection().await);
    let before = d.pool_stats();

    // Errors mid-transaction, leaving it aborted ("commands ignored until end").
    run(&d, "BEGIN; select 1/0").await;

    assert_eq!(scalar(&d, "select 'ok'").await.as_deref(), Some("ok"));
    assert_eq!(d.pool_stats().reopens, before.reopens);
}

#[tokio::test]
async fn temp_tables_and_prepared_statements_are_discarded() {
    let d = skip_unless!(one_connection().await);

    run(&d, "create temp table pool_tmp(x int); prepare pool_stmt as select 1").await;

    assert_eq!(
        scalar(&d, "select coalesce(to_regclass('pg_temp.pool_tmp')::text, 'NULL')").await.as_deref(),
        Some("NULL")
    );
    assert_eq!(
        scalar(&d, "select count(*)::text from pg_prepared_statements").await.as_deref(),
        Some("0")
    );
}

/// D-028's `reopen` path: with no primitive, the connection is closed on
/// release rather than pretending it was reset.
#[tokio::test]
async fn reopen_only_closes_the_connection_instead_of_resetting_it() {
    let d = skip_unless!(one_connection().await).with_reset(ResetSupport::ReopenOnly);
    let before = d.pool_stats();

    let a = pid(&d).await;
    let b = pid(&d).await;

    assert_ne!(a, b, "ReopenOnly must hand out a fresh connection after a dirty release");
    assert!(d.pool_stats().reopens >= before.reopens + 2);
    assert_eq!(d.capabilities().reset, ResetSupport::ReopenOnly);
}

/// Browse still works through the lease path, and its result is unaffected.
#[tokio::test]
async fn browse_runs_through_the_lease() {
    let d = skip_unless!(one_connection().await);
    run(&d, "drop table if exists pool_browse; create table pool_browse(id int primary key); insert into pool_browse values (1),(2)").await;

    let r = d
        .browse(BrowseRequest {
            relation: NodeId { kind: "table".into(), path: vec!["public".into(), "pool_browse".into()] },
            filter: None,
            sort: vec![],
            limit: 10,
            cursor: None,
        })
        .await
        .expect("browse");
    assert_eq!(r.first_rows().unwrap().1.len(), 2);
}

// ---------------------------------------------------------------- T-040

fn browse_req(table: &str) -> BrowseRequest {
    BrowseRequest {
        relation: NodeId { kind: "table".into(), path: vec!["public".into(), table.into()] },
        filter: None,
        sort: vec![],
        limit: 10,
        cursor: None,
    }
}

/// R-036's property. Under `ReopenOnly` — SQL Server's real capability — a
/// dirty release costs a reconnect, so structured traffic must never be dirty:
/// a grid page, a navigator expand and a filter change must each return the
/// connection as-is.
#[tokio::test]
async fn structured_traffic_never_dirties_a_connection_so_never_reopens() {
    let d = skip_unless!(one_connection().await);
    run(&d, "drop table if exists pool_clean; create table pool_clean(id int primary key); insert into pool_clean values (1)").await;
    let d = d.with_reset(ResetSupport::ReopenOnly);
    let before = d.pool_stats();

    for _ in 0..5 {
        d.browse(browse_req("pool_clean")).await.expect("browse");
    }
    d.navigator_children(None).await.expect("navigator");

    let after = d.pool_stats();
    assert_eq!(after.reopens, before.reopens, "structured traffic triggered a reopen: {after:?}");
    assert_eq!(after.resets, before.resets, "structured traffic triggered a reset: {after:?}");
    assert_eq!(after.clean_returns, before.clean_returns + 6, "{after:?}");
}

/// Raw passthrough is the one path that can mutate session state, so it is
/// the one that pays: under `ReopenOnly`, exactly one reopen per raw request.
#[tokio::test]
async fn raw_passthrough_is_what_dirties_and_the_reopen_rate_is_counted() {
    let d = skip_unless!(one_connection().await).with_reset(ResetSupport::ReopenOnly);
    let before = d.pool_stats();

    run(&d, "SET work_mem = '64MB'").await;
    run(&d, "select 1").await;

    let after = d.pool_stats();
    assert_eq!(after.reopens, before.reopens + 2, "{after:?}");
    assert_eq!(after.releases, before.releases + 2, "{after:?}");
}

/// A clean return skips `DISCARD ALL`, so a structured path must leave nothing
/// behind for it — including the named statements tokio-postgres prepares.
#[tokio::test]
async fn a_clean_return_leaves_no_prepared_statements_behind() {
    let d = skip_unless!(one_connection().await);
    run(&d, "drop table if exists pool_stmt_t; create table pool_stmt_t(id int primary key)").await;

    d.browse(browse_req("pool_stmt_t")).await.expect("browse");
    d.navigator_children(None).await.expect("navigator");

    // The same single connection, now via raw: nothing prepared survives.
    assert_eq!(
        scalar(&d, "select count(*)::text from pg_prepared_statements").await.as_deref(),
        Some("0")
    );
}
