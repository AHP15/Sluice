//! T-051 / D-010: real cancellation and the statement timeout, against a real
//! server. "Real" means the query stops **on the server** — checked in
//! `pg_stat_activity`, not inferred from the client returning.
//!
//! Skipped (not failed) without a server, like `live.rs`; CI runs it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sluice_core::{Driver, ErrorKind, OutcomeBody, RawRequest, Secret};
use sluice_postgres::{tls::TlsMode, CancelSlot, ConnectSpec, PgDriver};

fn spec(app: &str, timeout: Option<Duration>) -> ConnectSpec {
    ConnectSpec {
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: "postgres".into(),
        password: Some(Secret::new("sluice_dev".into())),
        dbname: "sluice_dev".into(),
        tls: TlsMode::Disabled,
        application_name: app.into(),
        statement_timeout: timeout,
    }
}

async fn connect(app: &str, timeout: Option<Duration>) -> Option<Arc<PgDriver>> {
    match PgDriver::connect_with_pool_size(spec(app, timeout), 1).await {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("SKIP: no Postgres reachable ({e})");
            None
        }
    }
}

/// The error kind of a script's first error outcome.
fn error_kind(r: &sluice_core::ExecutionResult) -> Option<ErrorKind> {
    r.outcomes.iter().find_map(|o| match &o.body {
        OutcomeBody::Error { error } => Some(error.kind),
        _ => None,
    })
}

async fn running_sleeps(observer: &PgDriver, app: &str) -> i64 {
    let r = observer
        .raw(RawRequest::Script(format!(
            "select count(*) from pg_stat_activity where application_name = '{app}' \
             and state = 'active' and query like '%pg_sleep%'"
        )))
        .await
        .unwrap();
    match &r.outcomes[0].body {
        OutcomeBody::Rows { rows, .. } => match &rows[0][0] {
            sluice_core::WireValue::Text(s) => s.parse().unwrap(),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn cancel_stops_the_query_on_the_server() {
    let app = "sluice-cancel-live";
    let (Some(d), Some(observer)) = (connect(app, None).await, connect("sluice-observer", None).await) else {
        return;
    };
    let slot = CancelSlot::default();
    let running = {
        let d = d.clone().with_cancel(slot.clone());
        tokio::spawn(async move { d.raw(RawRequest::Script("select pg_sleep(30)".into())).await })
    };

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(running_sleeps(&observer, app).await, 1, "the sleep should be running on the server");

    let started = Instant::now();
    assert!(d.cancel(&slot).await, "a cancel request was sent");
    let r = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("cancel returned promptly")
        .unwrap()
        .expect("raw returns the cancel as an outcome");
    assert_eq!(error_kind(&r), Some(ErrorKind::Cancelled));
    assert!(started.elapsed() < Duration::from_secs(3));

    // Stopped on the server — not merely abandoned by the client.
    assert_eq!(running_sleeps(&observer, app).await, 0, "the query is still running server-side");

    // The (single) pooled connection is reset and usable.
    let ok = d.raw(RawRequest::Script("select 1".into())).await.unwrap();
    assert_eq!(error_kind(&ok), None);
}

#[tokio::test]
async fn a_slot_cancelled_before_start_refuses_to_run() {
    let Some(d) = connect("sluice-cancel-early", None).await else { return };
    let slot = CancelSlot::default();
    assert!(!d.cancel(&slot).await, "nothing was running, so no request was sent");
    let e = d.clone().with_cancel(slot).raw(RawRequest::Script("select 1".into())).await.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cancelled);
}

#[tokio::test]
async fn the_profile_statement_timeout_applies_and_survives_the_pool_reset() {
    let Some(d) = connect("sluice-timeout", Some(Duration::from_millis(500))).await else { return };

    let started = Instant::now();
    let r = d.raw(RawRequest::Script("select pg_sleep(3)".into())).await.unwrap();
    assert_eq!(error_kind(&r), Some(ErrorKind::Timeout), "{:?}", r.outcomes);
    assert!(started.elapsed() < Duration::from_secs(2));

    // A raw request dirties the lease, so the connection was just through
    // ROLLBACK + DISCARD ALL. The timeout is a startup option, so DISCARD ALL
    // resets to it rather than clearing it.
    let shown = d.raw(RawRequest::Script("show statement_timeout".into())).await.unwrap();
    match &shown.outcomes[0].body {
        OutcomeBody::Rows { rows, .. } => {
            assert_eq!(rows[0][0], sluice_core::WireValue::Text("500ms".into()), "{rows:?}")
        }
        other => panic!("{other:?}"),
    }
    let again = d.raw(RawRequest::Script("select pg_sleep(3)".into())).await.unwrap();
    assert_eq!(error_kind(&again), Some(ErrorKind::Timeout), "the timeout must survive DISCARD ALL");
}
