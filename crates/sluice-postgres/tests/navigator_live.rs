//! T-053 / D-016: lazy native-catalog navigation with a per-connection cache,
//! against a real server.
//!
//! A second, independent connection plays "someone else": its DDL must stay
//! invisible to the cached tree until an explicit refresh (external DDL is
//! undetectable without polling — refresh is the contract), while DDL the app
//! itself runs invalidates the cache on its own.
//!
//! Skipped (not failed) without a server, like `live.rs`; CI runs it.

use std::sync::Arc;

use sluice_core::{Driver, NavNode, NodeId, OutcomeBody, RawRequest, Secret};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

async fn connect() -> Option<Arc<PgDriver>> {
    let spec = ConnectSpec {
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: "postgres".into(),
        password: Some(Secret::new("sluice_dev".into())),
        dbname: "sluice_dev".into(),
        tls: TlsMode::Disabled,
        application_name: "sluice-navigator".into(),
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

async fn run(d: &PgDriver, sql: &str) {
    let r = d.raw(RawRequest::Script(sql.into())).await.expect("raw");
    for o in &r.outcomes {
        if let OutcomeBody::Error { error } = &o.body {
            panic!("{sql}: {error}");
        }
    }
}

fn schema() -> NodeId {
    NodeId { kind: "schema".into(), path: vec!["nav_t".into()] }
}

async fn labels(d: &PgDriver, parent: NodeId) -> Vec<String> {
    d.navigator_children(Some(parent)).await.expect("children").into_iter().map(|n: NavNode| n.label).collect()
}

#[tokio::test]
async fn the_tree_is_lazy_cached_and_invalidated_only_by_our_own_ddl() {
    let (Some(app), Some(other)) = (connect().await, connect().await) else { return };
    run(&other, "drop schema if exists nav_t cascade; create schema nav_t;").await;
    run(&other, "create table nav_t.orders(id int primary key, email text not null, note text); \
                 create unique index orders_email on nav_t.orders(email); \
                 create index orders_note on nav_t.orders using hash (note);").await;
    app.refresh_catalog();

    // Roots, then relations on expand.
    let roots = app.navigator_children(None).await.unwrap();
    assert!(roots.iter().any(|n| n.label == "nav_t"));
    assert_eq!(labels(&app, schema()).await, vec!["orders"]);

    // Columns and indexes on relation select — native catalog detail that
    // information_schema could not give (index method, primary key).
    let orders = NodeId { kind: "table".into(), path: vec!["nav_t".into(), "orders".into()] };
    let detail = app.navigator_children(Some(orders.clone())).await.unwrap();
    let cols: Vec<&str> = detail.iter().filter(|n| n.id.kind == "column").map(|n| n.label.as_str()).collect();
    let idx: Vec<&str> = detail.iter().filter(|n| n.id.kind == "index").map(|n| n.label.as_str()).collect();
    assert_eq!(cols, vec!["id  integer · not null", "email  text · not null", "note  text"]);
    assert_eq!(idx, vec!["orders_pkey  (btree, primary key)", "orders_email  (btree, unique)", "orders_note  (hash)"]);

    // Someone else's DDL: invisible to the cached tree…
    run(&other, "create table nav_t.external_t(a int)").await;
    assert_eq!(labels(&app, schema()).await, vec!["orders"], "the cache must not re-query on its own");
    // …until an explicit refresh.
    app.refresh_catalog();
    assert_eq!(labels(&app, schema()).await, vec!["external_t", "orders"]);

    // DML through the app keeps the cache (no catalog change)…
    run(&other, "create table nav_t.after_dml(a int)").await;
    run(&app, "insert into nav_t.orders values (1, 'a@b.c', null)").await;
    assert_eq!(labels(&app, schema()).await, vec!["external_t", "orders"], "DML must not invalidate");

    // …while DDL through the app invalidates it automatically.
    run(&app, "create table nav_t.ours(a int)").await;
    assert_eq!(labels(&app, schema()).await, vec!["after_dml", "external_t", "orders", "ours"]);

    // A relation dropped by someone else is a missing catalog row: a notice
    // in place, not a failed navigation.
    run(&other, "drop table nav_t.external_t").await;
    let gone = NodeId { kind: "table".into(), path: vec!["nav_t".into(), "external_t".into()] };
    let out = app.navigator_children(Some(gone)).await.expect("a vanished relation is not an error");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id.kind, "notice");
    assert!(out[0].label.contains("refresh"), "{}", out[0].label);

    run(&other, "drop schema nav_t cascade").await;
}

/// T-054: connect parses `version()` into the descriptor's version and
/// flavour, and the gated features follow from them.
#[tokio::test]
async fn connect_records_the_parsed_version_and_flavour() {
    let Some(d) = connect().await else { return };
    let caps = d.capabilities();
    assert_eq!(caps.flavour.as_deref(), Some("postgres"));
    let version = caps.server_version.clone().expect("a version");
    assert!(version.chars().next().is_some_and(|c| c.is_ascii_digit()), "a clean version, not the banner: {version}");
    assert!(!version.contains("PostgreSQL"), "{version}");
    assert_eq!(caps.reset, sluice_core::ResetSupport::Native, "vanilla Postgres has DISCARD ALL");
    assert!(caps.string_collation.is_some(), "PG 12+ runs the gated collation probe");
    eprintln!("detected: {} {version}", caps.flavour.unwrap());
}
