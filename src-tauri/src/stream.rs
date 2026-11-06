//! Credit-based backpressure for streamed row batches (T-049 / R-006, D-010).
//!
//! Tauri's channels are fire-and-forget: the core can emit faster than React
//! renders, and a fast local Postgres streaming a large table would queue
//! batches in the webview until it is killed. Backpressure is therefore built
//! here as an explicit protocol, not assumed from the transport:
//!
//! - the core sends a batch only against **credit**; it starts with
//!   [`MAX_IN_FLIGHT`] and every send spends one;
//! - the webview returns one credit per batch **after it has rendered it**, so
//!   the producer's rate is bound to the render rate by construction;
//! - credit can only be returned for batches actually in flight — a buggy or
//!   hostile webview cannot inflate it past the fixed bound;
//! - a producer blocked on credit stays **cancellable** and still hits its
//!   **deadline**: cancel closes the gate, which wakes the wait immediately, and
//!   the wait races the deadline. Stopping cancels the query server-side
//!   (`PgDriver::browse_batches`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use sluice_core::{ColumnDesc, DriverError, WireValue};
use tokio::sync::Semaphore;
use tokio::time::Instant;

/// Batches allowed in flight between core and webview.
pub const MAX_IN_FLIGHT: u32 = 4;

/// Rows per batch.
pub const BATCH_ROWS: usize = 100;

/// How long one stream may run, including time spent blocked on credit.
/// T-051 replaces this with the per-profile statement timeout D-010 calls for;
/// what T-049 guarantees is that the deadline is honoured *while blocked*.
pub const STREAM_DEADLINE: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Cancelled,
    TimedOut,
}

pub struct CreditGate {
    credit: Semaphore,
    /// Sent and not yet returned.
    in_flight: AtomicU32,
}

impl CreditGate {
    pub fn new(max_in_flight: u32) -> Self {
        Self { credit: Semaphore::new(max_in_flight as usize), in_flight: AtomicU32::new(0) }
    }

    /// Spend one credit, waiting for it if necessary — but never past
    /// `deadline`, and never once cancelled.
    pub async fn acquire(&self, deadline: Instant) -> Result<(), Stop> {
        if Instant::now() >= deadline {
            return Err(Stop::TimedOut);
        }
        tokio::select! {
            biased;
            permit = self.credit.acquire() => match permit {
                Ok(p) => {
                    // Not returned on drop: only the webview's `grant` returns it.
                    p.forget();
                    self.in_flight.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }
                // Closed by `cancel`.
                Err(_) => Err(Stop::Cancelled),
            },
            _ = tokio::time::sleep_until(deadline) => Err(Stop::TimedOut),
        }
    }

    /// Return credit for `batches` the webview has retired. Only batches in
    /// flight can be returned; the rest are ignored. Returns how many counted.
    pub fn grant(&self, batches: u32) -> u32 {
        let mut accepted = 0;
        for _ in 0..batches {
            let ok = self
                .in_flight
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_sub(1))
                .is_ok();
            if !ok {
                break;
            }
            accepted += 1;
        }
        self.credit.add_permits(accepted as usize);
        accepted
    }

    /// Stop the stream: a waiting `acquire` returns `Cancelled` at once.
    pub fn cancel(&self) {
        self.credit.close();
    }

    pub fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::SeqCst)
    }
}

/// What the webview receives on a stream's channel.
#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamEvent {
    Columns { columns: Vec<ColumnDesc> },
    Rows { seq: u64, rows: Vec<Vec<WireValue>> },
    Done { rows: usize, truncated: bool },
    Cancelled,
    TimedOut,
    Error { error: DriverError },
}

/// Live streams' gates, by stream id. Cloneable so a stream's task can remove
/// itself when it ends.
#[derive(Clone, Default)]
pub struct Streams(Arc<Mutex<HashMap<String, Arc<CreditGate>>>>);

impl Streams {
    pub fn insert(&self, id: String, gate: Arc<CreditGate>) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(id, gate);
    }
    pub fn get(&self, id: &str) -> Option<Arc<CreditGate>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).get(id).cloned()
    }
    pub fn remove(&self, id: &str) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    /// The bound: with no credit returned, the producer gets exactly
    /// `MAX_IN_FLIGHT` batches out and then waits.
    #[tokio::test]
    async fn the_producer_blocks_after_the_in_flight_bound() {
        let g = CreditGate::new(MAX_IN_FLIGHT);
        for _ in 0..MAX_IN_FLIGHT {
            g.acquire(far()).await.unwrap();
        }
        assert_eq!(g.in_flight(), MAX_IN_FLIGHT);
        let blocked = tokio::time::timeout(Duration::from_millis(50), g.acquire(far())).await;
        assert!(blocked.is_err(), "a fifth batch went out without credit");
    }

    /// Credit comes back one batch at a time, as the webview retires them.
    #[tokio::test]
    async fn retiring_a_batch_releases_exactly_one_more() {
        let g = CreditGate::new(2);
        g.acquire(far()).await.unwrap();
        g.acquire(far()).await.unwrap();
        assert_eq!(g.grant(1), 1);
        g.acquire(far()).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(50), g.acquire(far())).await.is_err());
    }

    /// The webview cannot mint credit: returning more than is in flight
    /// counts only what is in flight.
    #[tokio::test]
    async fn credit_cannot_be_inflated_past_the_bound() {
        let g = CreditGate::new(2);
        g.acquire(far()).await.unwrap();
        assert_eq!(g.grant(100), 1);
        assert_eq!(g.grant(5), 0);
        // Still only two may be in flight.
        g.acquire(far()).await.unwrap();
        g.acquire(far()).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(50), g.acquire(far())).await.is_err());
    }

    /// R-006: cancellation stays reachable while blocked on credit.
    #[tokio::test]
    async fn cancel_wakes_a_producer_blocked_on_credit() {
        let g = Arc::new(CreditGate::new(1));
        g.acquire(far()).await.unwrap();
        let waiter = {
            let g = g.clone();
            tokio::spawn(async move { g.acquire(far()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        g.cancel();
        let r = tokio::time::timeout(Duration::from_secs(1), waiter).await.expect("woke").unwrap();
        assert_eq!(r, Err(Stop::Cancelled));
    }

    /// R-006: the statement deadline stays reachable while blocked on credit.
    #[tokio::test]
    async fn the_deadline_fires_while_blocked_on_credit() {
        let g = CreditGate::new(1);
        g.acquire(far()).await.unwrap();
        let started = Instant::now();
        let r = g.acquire(Instant::now() + Duration::from_millis(80)).await;
        assert_eq!(r, Err(Stop::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    /// And a stream past its deadline stops even when credit is plentiful.
    #[tokio::test]
    async fn a_passed_deadline_stops_even_with_credit_available() {
        let g = CreditGate::new(4);
        assert_eq!(g.acquire(Instant::now()).await, Err(Stop::TimedOut));
    }

    /// End to end against Postgres: the driver's producer, gated by this
    /// credit, cannot run ahead of a consumer that never returns credit, and
    /// cancelling it while blocked ends the query on the server.
    #[tokio::test]
    async fn a_blocked_live_stream_is_bounded_and_cancellable() {
        use sluice_core::{BrowseRequest, Driver, NodeId, RawRequest, Secret};
        use sluice_postgres::{tls::TlsMode, BrowseBatch, ConnectSpec, PgDriver};

        let spec = ConnectSpec {
            host: "localhost".into(),
            port: 55432,
            user: "postgres".into(),
            password: Some(Secret::new("sluice_dev".into())),
            dbname: "sluice_dev".into(),
            tls: TlsMode::Disabled,
            application_name: "sluice-stream-test".into(),
            statement_timeout: None,
        };
        let Ok(d) = PgDriver::connect(spec).await else {
            eprintln!("SKIP: no Postgres reachable");
            return;
        };
        d.raw(RawRequest::Script(
            "drop table if exists stream_t; create table stream_t as select g as id from generate_series(1, 5000) g; alter table stream_t add primary key (id);".into(),
        ))
        .await
        .unwrap();

        let gate = Arc::new(CreditGate::new(MAX_IN_FLIGHT));
        let sent = Arc::new(AtomicU32::new(0));
        let task = {
            let (d, gate, sent) = (d.clone(), gate.clone(), sent.clone());
            tokio::spawn(async move {
                let deadline = Instant::now() + Duration::from_secs(30);
                d.browse_batches(
                    BrowseRequest {
                        relation: NodeId { kind: "table".into(), path: vec!["public".into(), "stream_t".into()] },
                        filter: None,
                        sort: vec![],
                        limit: 5000,
                        cursor: None,
                    },
                    BATCH_ROWS,
                    move |b| {
                        let (gate, sent) = (gate.clone(), sent.clone());
                        async move {
                            match b {
                                BrowseBatch::Columns(_) => true,
                                BrowseBatch::Rows(_) => {
                                    let ok = gate.acquire(deadline).await.is_ok();
                                    if ok {
                                        sent.fetch_add(1, Ordering::SeqCst);
                                    }
                                    ok
                                }
                            }
                        }
                    },
                )
                .await
            })
        };

        // The consumer never returns credit. The producer must stall at the bound.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(sent.load(Ordering::SeqCst), MAX_IN_FLIGHT, "producer ran past its credit");
        assert!(!task.is_finished(), "stream should be blocked on credit, not finished");

        gate.cancel();
        let end = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("cancel reached the blocked stream")
            .unwrap()
            .expect("stream result");
        assert!(end.stopped);
        assert_eq!(end.rows, MAX_IN_FLIGHT as usize * BATCH_ROWS);

        // The pool connection came back usable.
        let again = d.browse(BrowseRequest {
            relation: NodeId { kind: "table".into(), path: vec!["public".into(), "stream_t".into()] },
            filter: None,
            sort: vec![],
            limit: 3,
            cursor: None,
        });
        assert_eq!(again.await.unwrap().first_rows().unwrap().1.len(), 3);
    }
}
