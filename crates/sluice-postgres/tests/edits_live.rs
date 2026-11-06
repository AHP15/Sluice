//! T-056 / D-027: staged edits land in one transaction behind a unique-key gate.
//!
//! Skipped (not failed) without a server, like `live.rs`; CI runs it.

use std::sync::Arc;

use sluice_core::{
    BrowseRequest, Driver, EditGate, EditRequest, ErrorKind, NodeId, Original, OutcomeBody,
    RawRequest, Secret, TxDisposition, WireValue,
};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

async fn driver() -> Option<Arc<PgDriver>> {
    let spec = ConnectSpec {
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: "postgres".into(),
        password: Some(Secret::new("sluice_dev".into())),
        dbname: "sluice_dev".into(),
        tls: TlsMode::Disabled,
        application_name: "sluice-edits".into(),
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

async fn sql(d: &PgDriver, s: &str) {
    let r = d.raw(RawRequest::Script(s.into())).await.expect("setup");
    assert!(!r.outcomes.iter().any(|o| o.is_failure()), "setup failed: {:?}", r.outcomes);
}

fn rel(kind: &str, name: &str) -> NodeId {
    NodeId { kind: kind.into(), path: vec!["public".into(), name.into()] }
}

async fn gate(d: &PgDriver, kind: &str, name: &str) -> EditGate {
    d.browse(BrowseRequest { relation: rel(kind, name), filter: None, sort: vec![], limit: 1, cursor: None, offset: None })
        .await
        .expect("browse")
        .edit
        .expect("a structured browse reports its edit gate")
}

async fn names(d: &PgDriver, table: &str) -> Vec<WireValue> {
    let r = d
        .raw(RawRequest::Script(format!("SELECT name FROM public.{table} ORDER BY id")))
        .await
        .unwrap();
    r.first_rows().unwrap().1.iter().map(|row| row[0].clone()).collect()
}

fn o(c: &str, v: WireValue) -> Original {
    Original { column: c.into(), value: v }
}

fn set_name(table: &str, id: i64, name: WireValue) -> EditRequest {
    EditRequest {
        relation: rel("table", table),
        key: vec![o("id", WireValue::Int(id))],
        set: vec![o("name", name)],
        originals: vec![],
    }
}

#[tokio::test]
async fn the_gate_follows_the_catalog() {
    let Some(d) = driver().await else { return };
    sql(&d, "
        DROP TABLE IF EXISTS public.t056_pk, public.t056_uk, public.t056_nullable_uk, public.t056_keyless CASCADE;
        DROP VIEW IF EXISTS public.t056_view;
        CREATE TABLE public.t056_pk (id int8 PRIMARY KEY, name text);
        CREATE TABLE public.t056_uk (code text NOT NULL UNIQUE, name text);
        CREATE TABLE public.t056_nullable_uk (email text UNIQUE, name text);
        CREATE TABLE public.t056_keyless (name text);
        CREATE VIEW public.t056_view AS SELECT * FROM public.t056_pk;
    ").await;

    assert_eq!(gate(&d, "table", "t056_pk").await, EditGate::Editable { key: vec!["id".into()] });
    assert_eq!(gate(&d, "table", "t056_uk").await, EditGate::Editable { key: vec!["code".into()] });
    for (kind, name) in [("table", "t056_nullable_uk"), ("table", "t056_keyless"), ("view", "t056_view")] {
        assert!(matches!(gate(&d, kind, name).await, EditGate::ReadOnly { .. }), "{name}");
    }

    // The driver re-derives the gate on apply; a keyless table cannot be
    // written through the edit path whatever the caller sends.
    let e = d
        .apply_edits(vec![EditRequest {
            relation: rel("table", "t056_keyless"),
            key: vec![o("name", WireValue::Text("a".into()))],
            set: vec![o("name", WireValue::Text("b".into()))],
            originals: vec![],
        }], None)
        .await
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::PermissionDenied);
}

#[tokio::test]
async fn all_edits_commit_together() {
    let Some(d) = driver().await else { return };
    sql(&d, "
        DROP TABLE IF EXISTS public.t056_commit;
        CREATE TABLE public.t056_commit (id int8 PRIMARY KEY, name text);
        INSERT INTO public.t056_commit VALUES (1, 'a'), (2, 'b'), (3, 'c');
    ").await;

    let r = d
        .apply_edits(vec![
            set_name("t056_commit", 1, WireValue::Text("A".into())),
            set_name("t056_commit", 3, WireValue::Null),
        ], None)
        .await
        .unwrap();
    assert_eq!(r.tx.disposition, TxDisposition::Committed);
    assert_eq!(r.outcomes.len(), 2);
    assert!(r.outcomes.iter().all(|o| matches!(o.body, OutcomeBody::Affected { count: 1, .. })));
    assert_eq!(names(&d, "t056_commit").await, vec![WireValue::Text("A".into()), WireValue::Text("b".into()), WireValue::Null]);
}

#[tokio::test]
async fn a_missing_row_rolls_back_every_edit() {
    let Some(d) = driver().await else { return };
    sql(&d, "
        DROP TABLE IF EXISTS public.t056_conflict;
        CREATE TABLE public.t056_conflict (id int8 PRIMARY KEY, name text);
        INSERT INTO public.t056_conflict VALUES (1, 'a'), (2, 'b');
    ").await;

    let r = d
        .apply_edits(vec![
            set_name("t056_conflict", 1, WireValue::Text("changed".into())),
            set_name("t056_conflict", 99, WireValue::Text("ghost".into())),
        ], None)
        .await
        .unwrap();
    assert_eq!(r.tx.disposition, TxDisposition::RolledBack { rolled_back: vec![0] });
    match &r.outcomes.last().unwrap().body {
        OutcomeBody::Error { error } => assert_eq!(error.kind, ErrorKind::Conflict),
        other => panic!("{other:?}"),
    }
    // Edit 1 succeeded inside the transaction and was undone with it.
    assert_eq!(names(&d, "t056_conflict").await, vec![WireValue::Text("a".into()), WireValue::Text("b".into())]);
}

#[tokio::test]
async fn a_constraint_violation_rolls_back_every_edit() {
    let Some(d) = driver().await else { return };
    sql(&d, "
        DROP TABLE IF EXISTS public.t056_check;
        CREATE TABLE public.t056_check (id int8 PRIMARY KEY, name text CHECK (name <> 'bad'));
        INSERT INTO public.t056_check VALUES (1, 'a'), (2, 'b');
    ").await;

    let r = d
        .apply_edits(vec![
            set_name("t056_check", 1, WireValue::Text("ok".into())),
            set_name("t056_check", 2, WireValue::Text("bad".into())),
        ], None)
        .await
        .unwrap();
    assert!(matches!(r.tx.disposition, TxDisposition::RolledBack { .. }));
    assert_eq!(names(&d, "t056_check").await, vec![WireValue::Text("a".into()), WireValue::Text("b".into())]);

    // The connection went back to the pool clean: the next edit works.
    let r = d.apply_edits(vec![set_name("t056_check", 2, WireValue::Text("fine".into()))], None).await.unwrap();
    assert_eq!(r.tx.disposition, TxDisposition::Committed);
}

/// T-058 / R-004: the cases the bare-`=` predicate got wrong.
#[tokio::test]
async fn concurrency_is_null_safe_and_names_what_it_cannot_check() {
    let Some(d) = driver().await else { return };
    sql(&d, "
        DROP TABLE IF EXISTS public.t058;
        CREATE TABLE public.t058 (id int8 PRIMARY KEY, name text, doc json);
        INSERT INTO public.t058 VALUES (1, NULL, '{}'), (2, 'b', '[]');
    ").await;

    let edit = |id: i64, set: Vec<Original>, originals: Vec<Original>| EditRequest {
        relation: rel("table", "t058"),
        key: vec![o("id", WireValue::Int(id))],
        set,
        originals,
    };

    // A NULL cell as read: `name = NULL` would match nothing and report a
    // spurious conflict. `IS NOT DISTINCT FROM` matches.
    let r = d
        .apply_edits(vec![edit(1, vec![o("name", WireValue::Text("now set".into()))], vec![o("name", WireValue::Null)])], None)
        .await
        .unwrap();
    assert_eq!(r.tx.disposition, TxDisposition::Committed, "{:?}", r.outcomes);

    // Someone else changed the row after it was read: a real conflict, and it
    // says the row changed rather than that it vanished.
    sql(&d, "UPDATE public.t058 SET name = 'theirs' WHERE id = 2").await;
    let r = d
        .apply_edits(vec![edit(2, vec![o("name", WireValue::Text("mine".into()))], vec![o("name", WireValue::Text("b".into()))])], None)
        .await
        .unwrap();
    assert!(matches!(r.tx.disposition, TxDisposition::RolledBack { .. }));
    match &r.outcomes.last().unwrap().body {
        OutcomeBody::Error { error } => {
            assert_eq!(error.kind, ErrorKind::Conflict);
            assert!(error.message.contains("changed since it was read"), "{}", error.message);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(names(&d, "t058").await[1], WireValue::Text("theirs".into()));

    // json has no equality operator: comparing it would raise
    // `operator does not exist: json = json`. It is left out, the edit goes
    // through on the key alone, and the outcome says so.
    let r = d
        .apply_edits(vec![edit(
            2,
            vec![o("doc", WireValue::Text("{\"a\":1}".into()))],
            vec![o("doc", WireValue::Text("[]".into()))],
        )], None)
        .await
        .unwrap();
    assert_eq!(r.tx.disposition, TxDisposition::Committed, "{:?}", r.outcomes);
    match &r.outcomes[0].body {
        OutcomeBody::Affected { concurrency_omitted, .. } => assert_eq!(concurrency_omitted, &vec!["doc".to_string()]),
        other => panic!("{other:?}"),
    }
}

/// T-058: every browsed column declares whether it is concurrency-comparable.
#[tokio::test]
async fn columns_declare_concurrency_comparability() {
    let Some(d) = driver().await else { return };
    sql(&d, "
        DROP TABLE IF EXISTS public.t058_decl;
        CREATE TABLE public.t058_decl (id int8 PRIMARY KEY, name text, doc json, x xml, f float8);
    ").await;
    let r = d
        .browse(BrowseRequest { relation: rel("table", "t058_decl"), filter: None, sort: vec![], limit: 1, cursor: None, offset: None })
        .await
        .unwrap();
    let cols = r.first_rows().unwrap().0;
    let decl: Vec<(&str, bool)> = cols.iter().map(|c| (c.name.as_str(), c.concurrency_comparable)).collect();
    assert_eq!(decl, vec![("id", true), ("name", true), ("doc", false), ("x", false), ("f", false)]);
}

/// T-057 / R-003: what the preview shows is what runs. For generated edit
/// sets, running the previewed inline text and running the real apply leave
/// the row in the same state — across NULL, quoting, coercion and order.
#[tokio::test]
async fn the_inline_preview_does_what_apply_does() {
    let Some(d) = driver().await else { return };
    sql(&d, r#"
        DROP TABLE IF EXISTS public.t057;
        CREATE TABLE public.t057 (id int8 PRIMARY KEY, "it's" text, "$1" text, price numeric, flag bool, at timestamptz);
    "#).await;
    let reset = r#"DELETE FROM public.t057; INSERT INTO public.t057 VALUES (1, 'orig', 'o', 1.5, true, '2026-01-01 00:00:00+00');"#;
    let snapshot = "SELECT row_to_json(t)::text FROM public.t057 t";

    let read = |d: Arc<PgDriver>| async move {
        let r = d.raw(RawRequest::Script(snapshot.into())).await.unwrap();
        format!("{:?}", r.first_rows().unwrap().1)
    };

    let texts: &[Option<&str>] = &[None, Some(""), Some("o'brien"), Some("''"), Some("$1"), Some("😀 ﷽"), Some("semi;colon --x"), Some("orig")];
    let nums: &[Option<&str>] = &[None, Some("0"), Some("-1.50"), Some("99999999999999999999.0001"), Some("1.5")];
    let mut seed: u64 = 0x0057_0057;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let text_v = |x: Option<&str>| x.map_or(WireValue::Null, |s| WireValue::Text(s.into()));

    let mut compared = 0;
    for _ in 0..60 {
        let mut set = vec![];
        let mut originals = vec![];
        // Column order varies too.
        let mut cols = vec!["it's", "$1", "price", "flag"];
        if next() % 2 == 0 {
            cols.reverse();
        }
        for c in cols.iter().take(1 + (next() % 4) as usize) {
            let (v, orig) = match *c {
                "price" => (
                    nums[(next() % nums.len() as u64) as usize].map_or(WireValue::Null, |s| WireValue::Numeric(s.into())),
                    WireValue::Numeric("1.5".into()),
                ),
                "flag" => (if next() % 3 == 0 { WireValue::Null } else { WireValue::Bool(next() % 2 == 0) }, WireValue::Bool(true)),
                "it's" => (text_v(texts[(next() % texts.len() as u64) as usize]), WireValue::Text("orig".into())),
                _ => (text_v(texts[(next() % texts.len() as u64) as usize]), WireValue::Text("o".into())),
            };
            set.push(o(c, v));
            // Mostly the true original; sometimes a stale one, so both paths
            // must also agree on "this updates nothing".
            let stale = match *c {
                "price" => WireValue::Numeric("7".into()),
                "flag" => WireValue::Bool(false),
                _ => WireValue::Text("stale".into()),
            };
            originals.push(o(c, if next() % 5 == 0 { stale } else { orig }));
        }
        let edits = vec![EditRequest { relation: rel("table", "t057"), key: vec![o("id", WireValue::Int(1))], set, originals }];

        sql(&d, reset).await;
        let plan = d.preview_edits(edits.clone()).await.unwrap();
        let Some(inline) = plan.statements[0].inline.clone() else { continue };
        let _ = d.apply_edits(edits, Some(plan.digest.clone())).await.unwrap();
        let applied = read(d.clone()).await;

        sql(&d, reset).await;
        let r = d.raw(RawRequest::Script(inline.clone())).await.unwrap();
        assert!(!r.outcomes.iter().any(|o| o.is_failure()), "inline preview failed to run: {inline}\n{:?}", r.outcomes);
        let previewed = read(d.clone()).await;

        assert_eq!(applied, previewed, "preview and apply diverged for:\n{inline}");
        compared += 1;
    }
    assert!(compared >= 40, "only {compared} inline cases compared");
}

/// T-057: apply runs only what was previewed.
#[tokio::test]
async fn apply_refuses_a_plan_that_changed_since_preview() {
    let Some(d) = driver().await else { return };
    sql(&d, "
        DROP TABLE IF EXISTS public.t057_drift;
        CREATE TABLE public.t057_drift (id int8 PRIMARY KEY, name text);
        INSERT INTO public.t057_drift VALUES (1, 'a');
    ").await;
    let edits = vec![set_name("t057_drift", 1, WireValue::Text("b".into()))];
    let plan = d.preview_edits(edits.clone()).await.unwrap();
    assert!(plan.statements[0].statement.starts_with("UPDATE"));

    // The column's type changes between preview and apply, so the cast in the
    // statement that would run is not the one the user reviewed.
    sql(&d, "ALTER TABLE public.t057_drift ALTER COLUMN name TYPE varchar(10)").await;
    let e = d.apply_edits(edits.clone(), Some(plan.digest)).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Conflict);
    assert!(e.message.contains("previewed"), "{}", e.message);
    assert_eq!(names(&d, "t057_drift").await, vec![WireValue::Text("a".into())]);

    // Re-previewing yields the new plan, which then applies.
    let plan = d.preview_edits(edits.clone()).await.unwrap();
    assert!(plan.statements[0].statement.contains("::varchar"), "{}", plan.statements[0].statement);
    let r = d.apply_edits(edits, Some(plan.digest)).await.unwrap();
    assert_eq!(r.tx.disposition, TxDisposition::Committed);
}
