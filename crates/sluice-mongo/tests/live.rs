//! T-087 against a real mongod.
//!
//! Skipped (not failed) when no server is reachable, like the Postgres and
//! SQL Server live tests. It must be a replica set — a standalone mongod
//! cannot open a transaction at all (T-046). Locally, the spike's container:
//!
//! ```sh
//! docker run -d --name sluice-spike-mongo -p 27018:27017 mongo:8 --replSet rs0 --bind_ip_all
//! docker exec sluice-spike-mongo mongosh --quiet --eval \
//!   'rs.initiate({_id:"rs0",members:[{_id:0,host:"localhost:27017"}]})'
//! ```
//!
//! (`SLUICE_TEST_MONGO_HOST` / `SLUICE_TEST_MONGO_PORT` point elsewhere.)

use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use mongodb::bson::{doc, Document};
use sluice_core::{
    Driver, ErrorKind, NavNode, NodeId, RawPayloadKind, ResetSupport, Secret, SessionAffinity, TlsMode,
};
use sluice_mongo::{ConnectSpec, Deployment, MongoDriver};

fn spec(database: &str) -> ConnectSpec {
    ConnectSpec {
        host: std::env::var("SLUICE_TEST_MONGO_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: std::env::var("SLUICE_TEST_MONGO_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(27018),
        user: String::new(),
        password: None,
        database: database.into(),
        tls: TlsMode::Disabled,
        application_name: "sluice-test".into(),
    }
}

async fn connect_to(s: ConnectSpec, budget: usize) -> Option<Arc<MongoDriver>> {
    match MongoDriver::connect(s, budget).await {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("SKIP: no mongod reachable ({e})");
            None
        }
    }
}

async fn connect(database: &str, budget: usize) -> Option<Arc<MongoDriver>> {
    connect_to(spec(database), budget).await
}

#[tokio::test]
async fn connects_and_publishes_its_capabilities_per_t046() {
    let Some(d) = connect("admin", 5).await else { return };
    let caps = d.capabilities();
    assert_eq!(caps.engine, "mongodb");
    assert!(caps.server_version.as_deref().is_some_and(|v| v.split('.').count() >= 2), "{:?}", caps.server_version);
    assert!(matches!(d.server().deployment, Deployment::ReplicaSet { .. }), "{:?}", d.server().deployment);
    assert!(caps.flavour.as_deref().is_some_and(|f| f.contains("replica set")), "{:?}", caps.flavour);

    // T-046: a logical session, with interactive transactions bounded by the
    // server's live lifetime limit.
    assert_eq!(caps.affinity, SessionAffinity::Session);
    let live: i64 = {
        let r = d
            .client()
            .database("admin")
            .run_command(doc! { "getParameter": 1, "transactionLifetimeLimitSeconds": 1 })
            .await
            .unwrap();
        r.get_i32("transactionLifetimeLimitSeconds").map(i64::from).or_else(|_| r.get_i64("transactionLifetimeLimitSeconds")).unwrap()
    };
    let tx = caps.transactions.expect("a replica set transacts");
    assert!(tx.interactive);
    assert_eq!(tx.max_idle_seconds.map(i64::from), Some(live));

    assert_eq!(caps.reset, ResetSupport::NotApplicable);
    assert_eq!(caps.raw_payload, RawPayloadKind::CommandDocument);
    assert!(!caps.supports_script && !caps.fixed_schema);
    assert_eq!(d.connection_budget(), 5);
    assert!(d.max_connections().is_some_and(|m| m > 0));
    assert!(caps.catalog.features.iter().all(|f| f.available), "{:?}", caps.catalog);
}

/// T-046's verdict, re-run on this driver's client: an interactive
/// multi-document transaction across two collections, held open across an
/// await point the application controls, with snapshot isolation — through
/// the 3.x `.session(&mut s)` builder.
#[tokio::test]
async fn an_interactive_transaction_spans_collections_with_snapshot_isolation() {
    let Some(d) = connect("sluice_t087_tx", 3).await else { return };
    let db = d.client().database("sluice_t087_tx");
    db.drop().await.unwrap();
    let a = db.collection::<Document>("a");
    let b = db.collection::<Document>("b");
    // Collections must exist before a transaction writes to them on older
    // servers; create them up front.
    a.insert_one(doc! { "seed": true }).await.unwrap();
    b.insert_one(doc! { "seed": true }).await.unwrap();

    let mut s = d.client().start_session().await.unwrap();
    s.start_transaction().await.unwrap();
    a.insert_one(doc! { "k": 1 }).session(&mut s).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    b.insert_one(doc! { "k": 1 }).session(&mut s).await.unwrap();

    let outside = a.count_documents(doc! { "k": 1 }).await.unwrap() + b.count_documents(doc! { "k": 1 }).await.unwrap();
    let inside = a.count_documents(doc! { "k": 1 }).session(&mut s).await.unwrap();
    assert_eq!((outside, inside), (0, 1), "uncommitted writes are invisible outside the session");
    s.commit_transaction().await.unwrap();
    assert_eq!(b.count_documents(doc! { "k": 1 }).await.unwrap(), 1, "visible after commit");

    s.start_transaction().await.unwrap();
    a.insert_one(doc! { "k": 2 }).session(&mut s).await.unwrap();
    s.abort_transaction().await.unwrap();
    assert_eq!(a.count_documents(doc! { "k": 2 }).await.unwrap(), 0, "nothing survives an abort");

    db.drop().await.unwrap();
}

/// D-028: the crate's own pool is the budget. Saturate it with slow
/// operations and count, from a second client, the sockets this one holds.
#[tokio::test]
async fn the_pool_never_exceeds_the_profile_budget() {
    let app = "sluice-t087-budget";
    let Some(d) = connect_to(ConnectSpec { application_name: app.into(), ..spec("sluice_t087_pool") }, 2).await else {
        return;
    };
    let Some(observer) = connect("admin", 1).await else { return };
    let coll = d.client().database("sluice_t087_pool").collection::<Document>("slow");
    coll.drop().await.unwrap();
    coll.insert_one(doc! { "x": 1 }).await.unwrap();

    // Eight concurrent finds, each held server-side by `$where: sleep`.
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let c = coll.clone();
        tasks.push(tokio::spawn(async move {
            c.find(doc! { "$where": "sleep(400) || true" }).await.unwrap().try_collect::<Vec<_>>().await.unwrap()
        }));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let ops: Vec<Document> = observer
        .client()
        .database("admin")
        .aggregate(vec![
            doc! { "$currentOp": { "allUsers": true, "idleConnections": true } },
            doc! { "$match": { "appName": app } },
        ])
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    let busy = ops.iter().filter(|o| o.get_bool("active").unwrap_or(false) && o.get_str("op").ok() == Some("query")).count();
    let sockets: std::collections::BTreeSet<String> =
        ops.iter().filter_map(|o| o.get_str("client").ok().map(str::to_string)).collect();
    for t in tasks {
        t.await.unwrap();
    }
    assert_eq!(busy, 2, "the budget is saturated, not exceeded: {ops:#?}");
    // Pool (2) plus the one polling monitor socket.
    assert!(sockets.len() <= 3, "{sockets:?}");
    coll.drop().await.unwrap();
}

#[tokio::test]
async fn a_wrong_password_is_an_auth_failure_that_never_echoes_it() {
    if connect("admin", 1).await.is_none() {
        return;
    }
    let s = ConnectSpec {
        user: "nobody_t087".into(),
        password: Some(Secret::new("wrong-Hunter2!".into())),
        ..spec("admin")
    };
    let e = MongoDriver::connect(s, 1).await.err().expect("refused");
    assert_eq!(e.kind, ErrorKind::AuthFailed, "{e:?}");
    assert!(!format!("{e:?}").contains("wrong-Hunter2!"));
}

fn names(nodes: &[NavNode], kind: &str) -> Vec<String> {
    nodes.iter().filter(|n| n.id.kind == kind).map(|n| n.label.clone()).collect()
}

/// T-073 F-6: databases → collections (and views), one level per expand,
/// cached until an explicit refresh.
#[tokio::test]
async fn the_navigator_lists_databases_and_collections() {
    let Some(d) = connect("sluice_t087_nav", 2).await else { return };
    let db = d.client().database("sluice_t087_nav");
    db.drop().await.unwrap();
    db.collection::<Document>("orders").insert_one(doc! { "total": 1 }).await.unwrap();
    db.create_collection("customers").await.unwrap();
    db.run_command(doc! { "create": "big_orders", "viewOn": "orders", "pipeline": [{ "$match": { "total": { "$gt": 100 } } }] })
        .await
        .unwrap();

    let roots = d.navigator_children(None).await.unwrap();
    let dbs = names(&roots, "database");
    assert!(dbs.contains(&"sluice_t087_nav".to_string()), "{dbs:?}");
    for system in ["admin", "config", "local"] {
        assert!(!dbs.contains(&system.to_string()), "system databases are hidden: {dbs:?}");
    }

    let node = NodeId { kind: "database".into(), path: vec!["sluice_t087_nav".into()] };
    let kids = d.navigator_children(Some(node.clone())).await.unwrap();
    assert_eq!(names(&kids, "collection"), vec!["customers", "orders"]);
    assert_eq!(names(&kids, "view"), vec!["big_orders"]);
    assert!(!kids.iter().any(|n| n.label.starts_with("system.")), "{kids:?}");
    let orders = kids.iter().find(|n| n.label == "orders").unwrap();
    assert_eq!(orders.id.path, vec!["sluice_t087_nav", "orders"]);
    assert!(!orders.has_children);

    // A collection is a leaf; asking for its children is a named refusal.
    let e = d.navigator_children(Some(orders.id.clone())).await.err().expect("leaf");
    assert_eq!(e.kind, ErrorKind::NotFound);

    // Lazy and cached: an external create is invisible until a refresh.
    db.create_collection("later").await.unwrap();
    assert_eq!(names(&d.navigator_children(Some(node.clone())).await.unwrap(), "collection"), vec!["customers", "orders"]);
    d.refresh_catalog();
    assert_eq!(
        names(&d.navigator_children(Some(node)).await.unwrap(), "collection"),
        vec!["customers", "later", "orders"]
    );

    db.drop().await.unwrap();
}
