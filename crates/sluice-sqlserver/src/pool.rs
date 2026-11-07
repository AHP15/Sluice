//! Pooled connections with D-028's `reopen` reset (T-080 / R-012, R-036).
//!
//! T-072 measured that SQL Server's reset primitive, `sp_reset_connection`, is
//! a TDS header bit tiberius cannot set, so there is no way to clean a used
//! session in place. This driver therefore declares `reset: reopen_only` and
//! the pool honours it literally: a connection that **may** have been dirtied
//! is closed on release and the pool opens a fresh one.
//!
//! T-040 / R-036 is what keeps that from being connect-per-query: only a lease
//! marked dirty is reopened. Structured paths (navigator, catalog probes,
//! browse) issue parameterized `SELECT`s that change no session state, so
//! their connections go back as they are. Raw passthrough is the path that
//! carries `SET`, `#temp` DDL and a dangling `BEGIN TRAN`, and marks dirty.
//!
//! A lease dropped without [`Lease::release`] — an early `?`, an unwinding
//! panic (T-006; T-072 found three types that panic inside tiberius) — cannot
//! know what state it left, so it takes the safe path and is closed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use deadpool::managed::{self, Metrics, Object, RecycleResult};
use sluice_core::{DriverError, ErrorKind, PoolStatsSnapshot};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

/// One SQL Server connection.
pub type Conn = tiberius::Client<Compat<TcpStream>>;

/// Opens connections from a fixed tiberius config.
pub struct Manager {
    pub(crate) config: tiberius::Config,
}

pub(crate) async fn open(config: tiberius::Config) -> Result<Conn, tiberius::error::Error> {
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    tiberius::Client::connect(config, tcp.compat_write()).await
}

impl managed::Manager for Manager {
    type Type = Conn;
    type Error = tiberius::error::Error;

    async fn create(&self) -> Result<Conn, Self::Error> {
        open(self.config.clone()).await
    }

    async fn recycle(&self, _conn: &mut Conn, _: &Metrics) -> RecycleResult<Self::Error> {
        // Only clean connections ever come back (dirty ones are detached on
        // release), so there is nothing to reset. A connection that died while
        // idle fails its next query, which the caller sees as an error.
        Ok(())
    }
}

pub type Pool = managed::Pool<Manager>;

/// Release counters (T-040: "reopen rate is instrumented").
#[derive(Default, Debug)]
pub struct PoolStats {
    releases: AtomicU64,
    reopens: AtomicU64,
    clean_returns: AtomicU64,
}

impl PoolStats {
    pub fn snapshot(&self) -> PoolStatsSnapshot {
        PoolStatsSnapshot {
            releases: self.releases.load(Ordering::Relaxed),
            // No native reset exists on this engine (T-072).
            resets: 0,
            reopens: self.reopens.load(Ordering::Relaxed),
            clean_returns: self.clean_returns.load(Ordering::Relaxed),
        }
    }
}

/// One request's hold on a pooled connection.
pub struct Lease {
    obj: Option<Object<Manager>>,
    dirty: bool,
    stats: Arc<PoolStats>,
}

impl Lease {
    pub async fn acquire(pool: &Pool, stats: Arc<PoolStats>) -> Result<Lease, DriverError> {
        let obj = pool.get().await.map_err(|e| match e {
            managed::PoolError::Timeout(_) => DriverError::new(
                ErrorKind::Timeout,
                "every connection in this profile's budget is in use; try again when a query finishes, or raise the profile's connection budget",
            ),
            managed::PoolError::Backend(e) => crate::error_map::map_error(&e),
            other => DriverError::new(ErrorKind::ConnectionFailed, other.to_string()),
        })?;
        Ok(Lease { obj: Some(obj), dirty: false, stats })
    }

    pub fn conn(&mut self) -> &mut Conn {
        self.obj.as_mut().expect("lease used after release")
    }

    /// This lease may have changed session state; it must not be reused.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Return the connection: as-is if clean, closed (and so reopened by the
    /// pool on demand) if dirty — D-028's `reopen`.
    pub async fn release(mut self) {
        self.stats.releases.fetch_add(1, Ordering::Relaxed);
        if let Some(obj) = self.obj.take() {
            if self.dirty {
                self.stats.reopens.fetch_add(1, Ordering::Relaxed);
                // Detached from the pool and dropped, which closes the socket;
                // the server rolls back anything left open.
                drop(Object::take(obj));
            } else {
                self.stats.clean_returns.fetch_add(1, Ordering::Relaxed);
                drop(obj);
            }
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(obj) = self.obj.take() {
            // Not released: the state it left is unknown, so it is closed.
            self.stats.releases.fetch_add(1, Ordering::Relaxed);
            self.stats.reopens.fetch_add(1, Ordering::Relaxed);
            drop(Object::take(obj));
        }
    }
}
