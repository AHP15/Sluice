//! T-047 / D-010: walking every page returns every row exactly once, in order.
//!
//! The case that breaks naive keyset paging is ties: sort by a column where
//! many rows share a value, page with an odd size, and a page boundary falls
//! inside a run of equal values. Without the primary-key tiebreaker rows are
//! skipped or repeated; with it the walk is exact.
//!
//! Skipped (not failed) without a server, like `live.rs`; CI runs it.

use std::sync::Arc;

use sluice_core::{
    BrowseRequest, Driver, NodeId, OutcomeBody, PageInfo, RawRequest, Secret, SortKey, WireValue,
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
        application_name: "sluice-paging".into(),
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

fn int(v: &WireValue) -> i64 {
    match v {
        WireValue::Int(n) => *n,
        other => panic!("{other:?}"),
    }
}

/// Walk all pages of `kind`/`name` with `sort`, returning ids in order and
/// the strategy reported on the first page.
async fn walk(d: &PgDriver, kind: &str, name: &str, sort: Vec<SortKey>) -> (Vec<i64>, PageInfo) {
    let mut ids = Vec::new();
    let mut cursor = None;
    let mut offset = None;
    let mut first = None;
    for _ in 0..200 {
        let r = d
            .browse(BrowseRequest {
                relation: NodeId { kind: kind.into(), path: vec!["public".into(), name.into()] },
                filter: None,
                sort: sort.clone(),
                limit: 37,
                cursor: cursor.clone(),
                offset,
            })
            .await
            .expect("browse");
        let (cols, rows) = r.first_rows().unwrap();
        let i = cols.iter().position(|c| c.name == "id").unwrap();
        ids.extend(rows.iter().map(|row| int(&row[i])));
        let page = r.page.clone().expect("browse declares its paging");
        first.get_or_insert(page.clone());
        match page {
            PageInfo::Keyset { next_cursor: Some(c), .. } => cursor = Some(c),
            PageInfo::Offset { next_offset: Some(o), .. } => offset = Some(o),
            _ => return (ids, first.unwrap()),
        }
    }
    panic!("paging did not terminate");
}

#[tokio::test]
async fn every_page_walk_returns_each_row_exactly_once_in_order() {
    let Some(d) = driver().await else { return };
    let setup = "
        drop view if exists paging_v;
        drop table if exists paging_t;
        create table paging_t(id int primary key, grp int not null, label text);
        insert into paging_t select g, g % 7, case when g % 3 = 0 then null else 'x' end
          from generate_series(1, 1000) g;
        create view paging_v as select id, grp from paging_t;";
    let r = d.raw(RawRequest::Script(setup.into())).await.unwrap();
    assert!(r.outcomes.iter().all(|o| !matches!(o.body, OutcomeBody::Error { .. })));

    // Keyset by primary key alone.
    let (ids, page) = walk(&d, "table", "paging_t", vec![]).await;
    assert!(matches!(page, PageInfo::Keyset { ref key, .. } if key == &["id".to_string()]), "{page:?}");
    assert_eq!(ids, (1..=1000).collect::<Vec<_>>());

    // Keyset by a heavily tied column, descending, with the pk tiebreaker:
    // the expected order is exactly ORDER BY grp DESC, id ASC.
    let (ids, page) = walk(&d, "table", "paging_t", vec![SortKey { column: "grp".into(), descending: true }]).await;
    assert!(matches!(page, PageInfo::Keyset { ref key, .. } if key == &["grp".to_string(), "id".to_string()]), "{page:?}");
    let mut expected: Vec<i64> = (1..=1000).collect();
    expected.sort_by_key(|id| (std::cmp::Reverse(id % 7), *id));
    assert_eq!(ids, expected, "ties straddling page boundaries were skipped or repeated");

    // A view has no key: offset, declared with the reason, still exact.
    let (ids, page) = walk(&d, "view", "paging_v", vec![SortKey { column: "id".into(), descending: false }]).await;
    assert!(matches!(page, PageInfo::Offset { ref reason, .. } if reason.contains("no primary key")), "{page:?}");
    assert_eq!(ids, (1..=1000).collect::<Vec<_>>());

    // A nullable sort column falls back to offset, saying which column.
    let (_, page) = walk(&d, "table", "paging_t", vec![SortKey { column: "label".into(), descending: false }]).await;
    assert!(matches!(page, PageInfo::Offset { ref reason, .. } if reason.contains("`label`")), "{page:?}");

    // The streamed browse declares the same paging.
    let end = d
        .browse_batches(
            BrowseRequest {
                relation: NodeId { kind: "table".into(), path: vec!["public".into(), "paging_t".into()] },
                filter: None,
                sort: vec![],
                limit: 100,
                cursor: None,
                offset: None,
            },
            25,
            |_| async { true },
        )
        .await
        .unwrap();
    match end.page {
        Some(PageInfo::Keyset { next_cursor: Some(c), .. }) => assert_eq!(c, vec![WireValue::Int(100)]),
        other => panic!("{other:?}"),
    }
}
