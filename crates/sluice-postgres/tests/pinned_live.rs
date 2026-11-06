//! T-041 / D-028: a pinned editor session keeps its server session, makes
//! transactions opt-in, rolls back rather than commits, and surfaces loss.
//!
//! Skipped (not failed) without a server, like `live.rs`; CI runs it.

use std::sync::Arc;

use sluice_core::{Driver, OutcomeBody, RawRequest, Secret, TxDisposition, TxMode, WireValue};
use sluice_postgres::pinned::TxState;
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

async fn driver(name: &str) -> Option<Arc<PgDriver>> {
    let spec = ConnectSpec {
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: "postgres".into(),
        password: Some(Secret::new("sluice_dev".into())),
        dbname: "sluice_dev".into(),
        tls: TlsMode::Disabled,
        application_name: name.into(),
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

fn first_cell(r: &sluice_core::ExecutionResult) -> WireValue {
    r.first_rows().map(|(_, rows)| rows[0][0].clone()).unwrap_or(WireValue::Missing)
}

async fn count(d: &PgDriver, table: &str) -> String {
    let r = d.raw(RawRequest::Script(format!("SELECT count(*) FROM public.{table}"))).await.unwrap();
    match first_cell(&r) {
        WireValue::Text(s) => s,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn session_state_persists_across_runs() {
    let Some(d) = driver("sluice-pinned-1").await else { return };
    let mut s = d.open_pinned().await.unwrap();
    d.run_pinned(&mut s, "SET search_path = pg_catalog; CREATE TEMP TABLE t041_tmp (x int); INSERT INTO t041_tmp VALUES (7)")
        .await
        .unwrap();
    // A pooled raw run would see none of this; the pinned one sees all of it.
    let r = d.run_pinned(&mut s, "SELECT current_setting('search_path'), (SELECT x FROM t041_tmp)").await.unwrap();
    let row = &r.first_rows().unwrap().1[0];
    assert_eq!(row, &vec![WireValue::Text("pg_catalog".into()), WireValue::Text("7".into())]);
    assert_eq!(s.tx(), TxState::Idle);
    s.close().await;
}

#[tokio::test]
async fn transactions_are_opt_in_reported_open_and_never_committed_on_close() {
    let Some(d) = driver("sluice-pinned-2").await else { return };
    sql(&d, "DROP TABLE IF EXISTS public.t041_tx; CREATE TABLE public.t041_tx (x int)").await;

    let mut s = d.open_pinned().await.unwrap();
    // Without opting in, a write commits as it runs.
    d.run_pinned(&mut s, "INSERT INTO public.t041_tx VALUES (1)").await.unwrap();
    assert_eq!(count(&d, "t041_tx").await, "1");

    // Opt in with the control; the write is not visible outside.
    let st = s.begin().await.unwrap();
    assert_eq!(st.tx, TxState::Open);
    let r = d.run_pinned(&mut s, "INSERT INTO public.t041_tx VALUES (2)").await.unwrap();
    assert_eq!(r.tx.mode, TxMode::ExplicitUser);
    assert_eq!(r.tx.disposition, TxDisposition::Open);
    assert!(r.summary().contains("transaction still open"), "{}", r.summary());
    assert_eq!(count(&d, "t041_tx").await, "1");

    // Closing the tab rolls back; it never commits.
    s.close().await;
    assert_eq!(count(&d, "t041_tx").await, "1");

    // A typed BEGIN is the same opt-in, seen by the probe.
    let mut s = d.open_pinned().await.unwrap();
    d.run_pinned(&mut s, "BEGIN; INSERT INTO public.t041_tx VALUES (3)").await.unwrap();
    assert_eq!(s.tx(), TxState::Open);
    let st = s.commit().await.unwrap();
    assert_eq!(st.tx, TxState::Idle);
    assert_eq!(count(&d, "t041_tx").await, "2");
    s.close().await;
}

#[tokio::test]
async fn a_script_that_ends_the_transaction_reports_how_it_ended() {
    let Some(d) = driver("sluice-pinned-3").await else { return };
    sql(&d, "DROP TABLE IF EXISTS public.t041_end; CREATE TABLE public.t041_end (x int)").await;
    let mut s = d.open_pinned().await.unwrap();

    s.begin().await.unwrap();
    d.run_pinned(&mut s, "INSERT INTO public.t041_end VALUES (1)").await.unwrap();
    let r = d.run_pinned(&mut s, "COMMIT").await.unwrap();
    assert_eq!(r.tx.disposition, TxDisposition::Committed);

    s.begin().await.unwrap();
    d.run_pinned(&mut s, "INSERT INTO public.t041_end VALUES (2)").await.unwrap();
    let r = d.run_pinned(&mut s, "ROLLBACK").await.unwrap();
    assert!(matches!(r.tx.disposition, TxDisposition::RolledBack { .. }), "{:?}", r.tx);

    // Nothing written yet, so there is no xid to ask about: say so.
    s.begin().await.unwrap();
    let r = d.run_pinned(&mut s, "INSERT INTO public.t041_end VALUES (3); COMMIT").await.unwrap();
    assert_eq!(r.tx.disposition, TxDisposition::Unknown, "{:?}", r.tx);
    assert!(r.summary().contains("could not be determined"));
    s.close().await;
}

#[tokio::test]
async fn a_failed_transaction_can_only_be_rolled_back() {
    let Some(d) = driver("sluice-pinned-4").await else { return };
    let mut s = d.open_pinned().await.unwrap();
    s.begin().await.unwrap();
    let r = d.run_pinned(&mut s, "SELECT 1/0").await.unwrap();
    assert!(matches!(r.outcomes[0].body, OutcomeBody::Error { .. }));
    assert_eq!(s.tx(), TxState::Failed);
    assert_eq!(r.tx.disposition, TxDisposition::Open);
    assert!(s.commit().await.is_err());
    assert_eq!(s.rollback().await.unwrap().tx, TxState::Idle);
    s.close().await;
}

#[tokio::test]
async fn connection_loss_is_surfaced_and_never_silently_reconnected() {
    let Some(d) = driver("sluice-pinned-5").await else { return };
    let mut s = d.open_pinned().await.unwrap();
    s.begin().await.unwrap();
    let r = d.run_pinned(&mut s, "SELECT pg_backend_pid()").await.unwrap();
    let pid = match first_cell(&r) {
        WireValue::Text(p) => p,
        other => panic!("{other:?}"),
    };
    // Someone else kills the session.
    sql(&d, &format!("SELECT pg_terminate_backend({pid})")).await;

    let e = d.run_pinned(&mut s, "SELECT 1").await.unwrap_err();
    assert_eq!(e.kind, sluice_core::ErrorKind::ConnectionFailed);
    assert!(e.message.contains("lost") && e.message.contains("Reconnect"), "{}", e.message);
    // And it stays refused: no quiet retry onto a fresh session.
    let again = d.run_pinned(&mut s, "SELECT 1").await.unwrap_err();
    assert!(again.message.contains("Reconnect"));
    assert!(s.status().lost.is_some());
    s.close().await;
}

/// T-043 / R-037: the server reports each kind of session state — including
/// state set without any `SET` keyword — and a fresh session holds none.
#[tokio::test]
async fn every_kind_of_session_state_is_seen_by_the_server_check() {
    let Some(d) = driver("sluice-pinned-6").await else { return };
    let cases: &[(&str, &str)] = &[
        ("SET search_path = pg_catalog", "SET search_path"),
        // No SET keyword anywhere: a statement scan would call this stateless.
        ("SELECT set_config('work_mem', '8MB', false)", "SET work_mem"),
        ("CREATE TEMP TABLE t043 (x int)", "1 temp table"),
        ("PREPARE t043p AS SELECT 1", "1 prepared statement"),
        ("BEGIN; DECLARE t043c CURSOR WITH HOLD FOR SELECT 1; COMMIT", "1 open cursor"),
        ("SELECT pg_advisory_lock(43)", "1 advisory lock"),
        ("LISTEN t043", "1 LISTEN channel"),
    ];
    for (stmt, want) in cases {
        let mut s = d.open_pinned().await.unwrap();
        assert!(s.held().await.unwrap().is_empty(), "a fresh session holds nothing");
        let r = d.run_pinned(&mut s, stmt).await.unwrap();
        assert!(!r.outcomes.iter().any(|o| o.is_failure()), "{stmt}: {:?}", r.outcomes);
        let held = s.held().await.unwrap();
        assert!(!held.is_empty(), "{stmt} left no visible state");
        assert!(held.describe().iter().any(|h| h == want), "{stmt}: {:?}", held.describe());
        // The indicator shows it too.
        assert!(s.status().holds.iter().any(|h| h == want));
        s.close().await;
    }

    // A transaction counts, and a plain SELECT leaves nothing.
    let mut s = d.open_pinned().await.unwrap();
    d.run_pinned(&mut s, "SELECT 1").await.unwrap();
    assert!(s.held().await.unwrap().is_empty());
    s.begin().await.unwrap();
    assert!(s.held().await.unwrap().describe().contains(&"an open transaction".to_string()));
    s.close().await;
}

/// T-043: a lost session's message names what was lost.
#[tokio::test]
async fn a_lost_session_names_what_it_lost() {
    let Some(d) = driver("sluice-pinned-7").await else { return };
    let mut s = d.open_pinned().await.unwrap();
    let r = d.run_pinned(&mut s, "SET search_path = pg_catalog; CREATE TEMP TABLE t043l (x int); SELECT pg_backend_pid()").await.unwrap();
    let pid = match &r.first_rows().unwrap().1[0][0] {
        WireValue::Text(p) => p.clone(),
        other => panic!("{other:?}"),
    };
    sql(&d, &format!("SELECT pg_terminate_backend({pid})")).await;
    let e = d.run_pinned(&mut s, "SELECT 1").await.unwrap_err();
    assert!(e.message.contains("SET search_path") && e.message.contains("1 temp table"), "{}", e.message);
    s.close().await;
}
