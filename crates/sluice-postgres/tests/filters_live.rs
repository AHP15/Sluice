//! T-013 / D-005: every core filter operator returns the right rows from a
//! real server — including for values a quote or a wildcard could subvert if
//! they were interpolated rather than bound.
//!
//! Skipped (not failed) without a server, like `live.rs`; CI runs it.

use sluice_core::{
    BrowseRequest, Driver, FilterTree, NodeId, OutcomeBody, Predicate, RawRequest, Secret, SortKey,
    WireValue, CORE_FILTER_OPS,
};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

async fn driver() -> Option<std::sync::Arc<PgDriver>> {
    let spec = ConnectSpec {
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: "postgres".into(),
        password: Some(Secret::new("sluice_dev".into())),
        dbname: "sluice_dev".into(),
        tls: TlsMode::Disabled,
        application_name: "sluice-filters".into(),
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

fn pred(column: &str, op: &str, value: WireValue) -> FilterTree {
    FilterTree::Pred(Predicate { case_insensitive: false, column: column.into(), op: op.into(), value })
}

fn t(s: &str) -> WireValue {
    WireValue::Text(s.into())
}

/// The ids a filter returns, in id order.
async fn ids(d: &PgDriver, filter: FilterTree) -> Vec<i64> {
    let r = d
        .browse(BrowseRequest {
            relation: NodeId { kind: "table".into(), path: vec!["public".into(), "filter_t".into()] },
            filter: Some(filter),
            sort: vec![SortKey { column: "id".into(), descending: false }],
            limit: 100,
            cursor: None,
            offset: None,
        })
        .await
        .expect("browse");
    let (cols, rows) = r.first_rows().expect("rows");
    let i = cols.iter().position(|c| c.name == "id").unwrap();
    rows.iter()
        .map(|r| match &r[i] {
            WireValue::Int(n) => *n,
            other => panic!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn every_core_operator_returns_the_right_rows() {
    let Some(d) = driver().await else { return };
    let setup = r#"
        drop table if exists filter_t;
        create table filter_t(id int primary key, name text, n int8, at date);
        insert into filter_t values
          (1, 'alice',        10, '2026-01-01'),
          (2, 'o''brien',     20, '2026-02-01'),
          (3, '50% off_sale', 30, '2026-03-01'),
          (4, 'ALICE',        40, '2026-04-01'),
          (5, null,           null, null);
    "#;
    let r = d.raw(RawRequest::Script(setup.into())).await.unwrap();
    assert!(r.outcomes.iter().all(|o| !matches!(o.body, OutcomeBody::Error { .. })));

    let n = |v: i64| WireValue::Numeric(v.to_string());
    let cases: Vec<(&str, FilterTree, Vec<i64>)> = vec![
        ("eq", pred("name", "eq", t("o'brien")), vec![2]),
        ("ne", pred("n", "ne", n(20)), vec![1, 3, 4]),
        ("lt", pred("n", "lt", n(20)), vec![1]),
        ("lte", pred("n", "lte", n(20)), vec![1, 2]),
        ("gt", pred("n", "gt", n(30)), vec![4]),
        ("gte", pred("n", "gte", n(30)), vec![3, 4]),
        ("in", pred("n", "in", WireValue::Array(vec![n(10), n(40), n(99)])), vec![1, 4]),
        ("between", pred("at", "between", WireValue::Array(vec![
            WireValue::Timestamp("2026-02-01".into()),
            WireValue::Timestamp("2026-03-01".into()),
        ])), vec![2, 3]),
        ("is_null", pred("name", "is_null", WireValue::Null), vec![5]),
        ("is_not_null", pred("name", "is_not_null", WireValue::Null), vec![1, 2, 3, 4]),
        // A `%` and `_` in the needle match literally, not as wildcards.
        ("contains", pred("name", "contains", t("% off_")), vec![3]),
        ("starts_with", pred("name", "starts_with", t("o'")), vec![2]),
        // Engine semantics (R-009): LIKE under a deterministic collation
        // distinguishes case, so `ALICE` does not end with "ice".
        ("ends_with", pred("name", "ends_with", t("ice")), vec![1]),
    ];

    let covered: Vec<&str> = cases.iter().map(|(op, _, _)| *op).collect();
    for op in CORE_FILTER_OPS {
        assert!(covered.contains(op), "core operator `{op}` has no live case");
    }

    for (op, filter, want) in cases {
        assert_eq!(ids(&d, filter).await, want, "operator `{op}`");
    }

    // An empty `in` list matches nothing; an AND/OR tree keeps its grouping.
    assert!(ids(&d, pred("n", "in", WireValue::Array(vec![]))).await.is_empty());
    let tree = FilterTree::Or(vec![
        pred("id", "eq", WireValue::Int(5)),
        FilterTree::And(vec![pred("name", "contains", t("alice")), pred("n", "gt", n(10))]),
    ]);
    assert_eq!(ids(&d, tree).await, vec![5]);

    // T-015: the explicit case-insensitive toggle, per operator.
    let folded = |column: &str, op: &str, value: WireValue| {
        FilterTree::Pred(Predicate { case_insensitive: true, column: column.into(), op: op.into(), value })
    };
    assert_eq!(ids(&d, folded("name", "ends_with", t("ICE"))).await, vec![1, 4]);
    assert_eq!(ids(&d, folded("name", "eq", t("Alice"))).await, vec![1, 4]);
    assert_eq!(ids(&d, folded("name", "in", WireValue::Array(vec![t("ALICE"), t("O'BRIEN")]))).await, vec![1, 2, 4]);
    assert_eq!(ids(&d, folded("name", "contains", t("% OFF_"))).await, vec![3], "still literal when folded");

    // The descriptor reports the server's collation and what it implies.
    let caps = d.capabilities();
    assert!(caps.string_collation.as_deref().is_some_and(|c| !c.is_empty()), "{:?}", caps.string_collation);
    let eq = caps.filter_ops.iter().find(|o| o.id == "eq").unwrap();
    assert_eq!(eq.case_sensitive, Some(true), "the default collation is deterministic");
    assert!(eq.case_insensitive_toggle);
    eprintln!("server collation: {:?}", caps.string_collation);
}

/// T-014 / D-005: the driver's extension operators are declared with their
/// arity and type classes, outside the core set, and return the right rows.
#[tokio::test]
async fn declared_extension_operators_return_the_right_rows() {
    use sluice_core::{is_extension_op, FilterArity, TypeClass};
    let Some(d) = driver().await else { return };
    let setup = r#"
        drop table if exists filter_x;
        create table filter_x(id int primary key, name text, n int8);
        insert into filter_x values (1, 'alice', 10), (2, 'Alicia', 20), (3, 'bob', 30), (4, null, null);
    "#;
    d.raw(RawRequest::Script(setup.into())).await.unwrap();

    let caps = d.capabilities();
    let ext: Vec<_> = caps.filter_ops.iter().filter(|o| is_extension_op(&o.id)).collect();
    let regex = ext.iter().find(|o| o.id == "matches_regex").expect("declared");
    assert_eq!((regex.arity, regex.type_classes.as_slice()), (FilterArity::One, &[TypeClass::Text][..]));
    let not_in = ext.iter().find(|o| o.id == "not_in").expect("declared");
    assert_eq!(not_in.arity, FilterArity::List);
    // Every core op carries its arity too.
    let between = caps.filter_ops.iter().find(|o| o.id == "between").unwrap();
    assert_eq!(between.arity, FilterArity::Two);

    let ids_x = |f: FilterTree| {
        let d = d.clone();
        async move {
            let r = d
                .browse(BrowseRequest {
                    relation: NodeId { kind: "table".into(), path: vec!["public".into(), "filter_x".into()] },
                    filter: Some(f),
                    sort: vec![SortKey { column: "id".into(), descending: false }],
                    limit: 100,
                    cursor: None,
                    offset: None,
                })
                .await
                .expect("browse");
            r.first_rows().unwrap().1.iter().map(|r| r[0].clone()).collect::<Vec<_>>()
        }
    };
    let i = |n: i64| WireValue::Int(n);

    assert_eq!(ids_x(pred("name", "matches_regex", t("^ali"))).await, vec![i(1)]);
    let mut folded = Predicate { case_insensitive: true, column: "name".into(), op: "matches_regex".into(), value: t("^ali") };
    assert_eq!(ids_x(FilterTree::Pred(folded.clone())).await, vec![i(1), i(2)]);
    // A pattern is a bound value: a quote in it is just a character.
    folded.value = t("o'; drop table filter_x; --");
    assert_eq!(ids_x(FilterTree::Pred(folded)).await, Vec::<WireValue>::new());

    let list = WireValue::Array(vec![WireValue::Numeric("10".into()), WireValue::Numeric("30".into())]);
    // Like `ne`, a NULL cell never matches.
    assert_eq!(ids_x(pred("n", "not_in", list)).await, vec![i(2)]);
}
