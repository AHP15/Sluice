//! Pooled-connection leases with capability-declared reset (T-039 / D-028).
//!
//! The pool hands a connection to one request, and the next request must not
//! inherit anything the first left behind: a `SET search_path`, an open
//! transaction, a temp table, a prepared statement. D-028 makes *how* that is
//! guaranteed a declared driver capability:
//!
//! - [`ResetSupport::Native`] — run the engine's primitive (`DISCARD ALL`).
//! - [`ResetSupport::ReopenOnly`] — no reachable primitive, so the connection is
//!   **closed** on release and the pool opens a fresh one. Correctness is bought
//!   with a reconnect rather than assumed.
//! - [`ResetSupport::NotApplicable`] — no session state to leak.
//!
//! Two ordering rules, both D-028's: a connection reaching release
//! mid-transaction is **rolled back first**, and `DISCARD ALL` is **never issued
//! inside a transaction** (it would fail there — and a failed reset must not be
//! mistaken for a clean connection). Both follow from sending `ROLLBACK` as its
//! own statement immediately before `DISCARD ALL` on the same connection.
//!
//! A lease that is dropped without [`Lease::release`] — an early `?`, a panic
//! unwinding through T-006's guard — cannot run async cleanup, so it takes the
//! safe path: the connection is detached from the pool and closed. Forgetting
//! to release costs a reconnect, never a leak.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use deadpool_postgres::{Object, Pool};
use sluice_core::{DriverError, ErrorKind, ResetSupport};

/// Release counters (T-040: "reopen rate is instrumented").
#[derive(Default, Debug)]
pub struct PoolStats {
    releases: AtomicU64,
    resets: AtomicU64,
    reopens: AtomicU64,
    clean_returns: AtomicU64,
}

/// A point-in-time copy of [`PoolStats`] — the engine-neutral counters (T-077).
pub use sluice_core::PoolStatsSnapshot;

impl PoolStats {
    pub fn snapshot(&self) -> PoolStatsSnapshot {
        PoolStatsSnapshot {
            releases: self.releases.load(Ordering::Relaxed),
            resets: self.resets.load(Ordering::Relaxed),
            reopens: self.reopens.load(Ordering::Relaxed),
            clean_returns: self.clean_returns.load(Ordering::Relaxed),
        }
    }
}

/// One request's hold on a pooled connection.
pub struct Lease {
    obj: Option<Object>,
    reset: ResetSupport,
    dirty: bool,
    stats: Arc<PoolStats>,
}

impl Lease {
    pub async fn acquire(
        pool: &Pool,
        reset: ResetSupport,
        stats: Arc<PoolStats>,
    ) -> Result<Self, DriverError> {
        let obj = pool.get().await.map_err(|e| match e {
            // The server's own error (a wrong password, a missing database, a
            // TLS refusal) is inside the pool's wrapper. Surfacing the wrapper's
            // text — "Error occurred while creating a new object: db error" —
            // reported every one of them as a bare connection failure with no
            // detail; the real error goes through the normal mapping instead.
            deadpool_postgres::PoolError::Backend(inner) => crate::error_map::map_error(&inner, None),
            // T-042: the budget is spent and nothing came back in time.
            deadpool_postgres::PoolError::Timeout(_) => DriverError::new(
                ErrorKind::Timeout,
                format!(
                    "all {} connections in this profile's budget are in use (open editor sessions count towards it); \
                     close an editor tab or wait for a running query to finish",
                    pool.status().max_size
                ),
            ),
            other => DriverError::new(ErrorKind::ConnectionFailed, other.to_string()),
        })?;
        // T-040: clean until something session-mutating crosses it. Callers
        // mark it; the default is the cheap path, so a structured request
        // never triggers a reset or a reopen.
        Ok(Self { obj: Some(obj), reset, dirty: false, stats })
    }

    pub fn client(&self) -> &tokio_postgres::Client {
        self.obj.as_ref().expect("lease used after release")
    }

    /// Record that something session-mutating crossed this connection.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// End the lease, leaving the connection clean for the next one.
    pub async fn release(mut self) {
        let Some(obj) = self.obj.take() else { return };
        self.stats.releases.fetch_add(1, Ordering::Relaxed);

        if !self.dirty {
            self.stats.clean_returns.fetch_add(1, Ordering::Relaxed);
            return; // dropped `obj` returns to the pool
        }

        match self.reset {
            ResetSupport::Native => match native_reset(&obj).await {
                Ok(()) => {
                    // Server-side statements are gone; drop any cached handles.
                    obj.statement_cache.clear();
                    self.stats.resets.fetch_add(1, Ordering::Relaxed);
                }
                // A connection that cannot be reset is not clean: close it.
                Err(_) => self.detach(obj),
            },
            ResetSupport::ReopenOnly => self.detach(obj),
            ResetSupport::NotApplicable => {
                // Nothing session-scoped to leak — but a transaction is still a
                // transaction, so end it rather than hand it on.
                if rollback(&obj).await.is_err() {
                    self.detach(obj);
                }
            }
        }
    }

    fn detach(&self, obj: Object) {
        self.stats.reopens.fetch_add(1, Ordering::Relaxed);
        // Removed from the pool; dropping the client closes the socket, which
        // also ends any open transaction server-side.
        drop(Object::take(obj));
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(obj) = self.obj.take() {
            self.stats.releases.fetch_add(1, Ordering::Relaxed);
            self.detach(obj);
        }
    }
}

/// `ROLLBACK`, then `DISCARD ALL`, as separate statements. Sent together in one
/// simple query they would run as one implicit transaction block, which is
/// exactly where `DISCARD ALL` refuses to run.
async fn native_reset(client: &tokio_postgres::Client) -> Result<(), tokio_postgres::Error> {
    rollback(client).await?;
    client.simple_query("DISCARD ALL").await?;
    Ok(())
}

/// Ends any open or aborted transaction. Outside one, Postgres only warns.
async fn rollback(client: &tokio_postgres::Client) -> Result<(), tokio_postgres::Error> {
    client.simple_query("ROLLBACK").await.map(|_| ())
}

/// T-051 / D-010: the engine-neutral cancel slot (T-077 moved it to core).
pub use sluice_core::CancelSlot;

/// Postgres's [`sluice_core::CancelHandle`]: the protocol cancel request, sent
/// over a separate connection with the session's own TLS mode.
pub struct PgCancel {
    pub token: tokio_postgres::CancelToken,
    pub tls: crate::tls::TlsMode,
}

impl PgCancel {
    pub fn new(token: tokio_postgres::CancelToken, tls: crate::tls::TlsMode) -> Arc<Self> {
        Arc::new(Self { token, tls })
    }
}

#[async_trait::async_trait]
impl sluice_core::CancelHandle for PgCancel {
    async fn cancel(&self) -> bool {
        use crate::tls::TlsMode;
        let r = if self.tls == TlsMode::Disabled {
            self.token.cancel_query(tokio_postgres::NoTls).await
        } else {
            match crate::tls::client_config(self.tls) {
                Ok(cfg) => self.token.cancel_query(tokio_postgres_rustls::MakeRustlsConnect::new(cfg)).await,
                Err(_) => return false,
            }
        };
        r.is_ok()
    }
}
