//! T-050 / R-007, R-043: one execution's result budget, against a real server.
//!
//! Skipped (not failed) without a server, like `live.rs`; CI runs it.

use std::sync::Arc;

use sluice_core::{BudgetLimit, Driver, ExecBudget, OutcomeBody, RawRequest, Secret};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

async fn driver() -> Option<Arc<PgDriver>> {
    let spec = ConnectSpec {
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: "postgres".into(),
        password: Some(Secret::new("sluice_dev".into())),
        dbname: "sluice_dev".into(),
        tls: TlsMode::Disabled,
        application_name: "sluice-budget".into(),
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

/// (rows retained, truncated) for each `rows` outcome.
fn rows_outcomes(r: &sluice_core::ExecutionResult) -> Vec<(usize, bool)> {
    r.outcomes
        .iter()
        .filter_map(|o| match &o.body {
            OutcomeBody::Rows { rows, truncated, .. } => Some((rows.len(), *truncated)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_budget_is_per_execution_in_order_and_never_stops_execution() {
    let Some(d) = driver().await else { return };
    d.raw(RawRequest::Script("drop table if exists budget_t; create table budget_t(a int)".into()))
        .await
        .unwrap();

    // R-043: three 6,000-row results against D-010's 10,000-row budget. A
    // per-statement cap would keep 18,000 rows; per execution keeps 10,000.
    let script = "
        select g from generate_series(1, 6000) g;
        select g from generate_series(1, 6000) g;
        select g from generate_series(1, 6000) g;
        insert into budget_t values (1);";
    let r = d.raw(RawRequest::Script(script.into())).await.unwrap();
    assert_eq!(rows_outcomes(&r), vec![(6000, false), (4000, true), (0, true)]);
    let b = r.budget.expect("raw reports its budget");
    assert_eq!((b.used_rows, b.exhausted_by, b.exhausted_at), (10_000, Some(BudgetLimit::Rows), Some(1)));
    assert!(r.summary().contains("display limit of 10000 rows reached at result 2"), "{}", r.summary());

    // The budget limits what is shown, never what runs: the INSERT after the
    // exhausted SELECTs executed.
    let count = d.raw(RawRequest::Script("select count(*) from budget_t".into())).await.unwrap();
    match &count.outcomes[0].body {
        OutcomeBody::Rows { rows, .. } => assert_eq!(rows[0][0], sluice_core::WireValue::Text("1".into())),
        other => panic!("{other:?}"),
    }

    // R-007: wide rows trip the byte budget long before the row budget.
    let tight = d.clone().with_budget(ExecBudget { max_rows: 10_000, max_bytes: 1024 * 1024 });
    let r = tight
        .raw(RawRequest::Script("select repeat('x', 100000) from generate_series(1, 500)".into()))
        .await
        .unwrap();
    let (kept, truncated) = rows_outcomes(&r)[0];
    assert!(truncated && kept < 15, "kept {kept} rows of ~100 KB within a 1 MiB budget");
    let b = r.budget.unwrap();
    assert_eq!(b.exhausted_by, Some(BudgetLimit::Bytes));
    assert!(b.used_bytes <= 1024 * 1024);
    assert!(r.summary().contains("display limit of 1 MiB reached"), "{}", r.summary());

    // A script inside the budget reports it unspent.
    let r = d.raw(RawRequest::Script("select 1".into())).await.unwrap();
    assert_eq!(r.budget.unwrap().exhausted_by, None);
    assert!(!r.summary().contains("display limit"));
}
