//! Panic containment for in-process drivers (T-006, R-022, D-003).
//!
//! D-003 chose in-tree drivers and put the consequence on the record: a driver
//! panic can take down the whole window, which out-of-process plugins would have
//! isolated for free. `catch_unwind` at the IPC boundary is the accepted price.
//!
//! R-022's objection is that `catch_unwind` alone is not enough — it survives the
//! panic but can leave poisoned locks and leaked connections. So this module
//! covers all three: catching the unwind, locks that do not poison, and
//! connections that come back via RAII during unwinding rather than leaking.
//!
//! T-072 turned this from defensive to mandatory by finding a concrete trigger:
//! `tiberius` panics through an unreached `todo!()` on `sql_variant`,
//! `geography` and `hierarchyid` columns — ordinary user data, not exotic input.

use std::any::Any;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::FutureExt;

use crate::error::{DriverError, ErrorKind};

/// Identifies the driver session a call belongs to, so a caught panic can name
/// what died instead of reporting an anonymous failure.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        SessionId(s.to_string())
    }
}

/// Best-effort text from a panic payload.
fn describe_panic(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// Run a driver call so a panic inside it becomes a `DriverBug` error naming the
/// session, instead of unwinding into the Tauri command handler.
///
/// This is the only sanctioned way to invoke driver code from an IPC command.
pub async fn guard_driver_call<F, T>(session: &SessionId, fut: F) -> Result<T, DriverError>
where
    F: Future<Output = Result<T, DriverError>>,
{
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => Err(DriverError::new(
            ErrorKind::DriverBug,
            format!("driver bug: panic in session `{session}`"),
        )
        .with_detail(crate::redact::redact_shapes(&describe_panic(payload.as_ref())))),
    }
}

/// Same containment for synchronous driver code.
pub fn guard_driver_call_blocking<F, T>(session: &SessionId, f: F) -> Result<T, DriverError>
where
    F: FnOnce() -> Result<T, DriverError>,
{
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => Err(DriverError::new(
            ErrorKind::DriverBug,
            format!("driver bug: panic in session `{session}`"),
        )
        .with_detail(crate::redact::redact_shapes(&describe_panic(payload.as_ref())))),
    }
}

/// A mutex that never hands out a poison error.
///
/// R-022 names poisoned locks as a failure mode of bare `catch_unwind`: once any
/// thread panics holding a std `Mutex`, every later `lock()` returns `Err` and
/// the application wedges even though the panic was caught.
///
/// The tradeoff is explicit and deliberate: recovering the guard means the data
/// may have been observed mid-update by the panicking thread. Only guard state
/// that is safe to see partially written — counters, registries, caches. State
/// with a cross-field invariant does not belong here.
#[derive(Debug, Default)]
pub struct NonPoisonMutex<T> {
    inner: Mutex<T>,
}

impl<T> NonPoisonMutex<T> {
    pub fn new(value: T) -> Self {
        Self { inner: Mutex::new(value) }
    }

    /// Locks, recovering explicitly from poison rather than propagating it.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Whether a panic has previously occurred while this lock was held.
    /// Useful for surfacing "state may be inconsistent" in diagnostics (T-023).
    pub fn was_poisoned(&self) -> bool {
        self.inner.is_poisoned()
    }

    pub fn into_inner(self) -> T {
        match self.inner.into_inner() {
            Ok(v) => v,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Why a connection was destroyed rather than returned to its pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscardReason {
    /// A panic unwound through the lease. The connection's protocol state is
    /// unknown — T-072 measured a tiberius decode panic leaving the TDS stream
    /// mid-packet — so it must never be reused.
    Panic,
    /// The driver reported the connection unusable.
    Broken,
}

/// Where a lease returns its connection. T-039's pool implements this; keeping
/// it a trait lets panic containment be built and tested before the pool exists.
pub trait ConnectionHome<C>: Send + Sync {
    /// Normal return. `dirty` drives the reset decision (T-040).
    fn release(&self, conn: C, dirty: bool);
    /// Destroy rather than reuse.
    fn discard(&self, conn: C, reason: DiscardReason);
}

/// RAII lease over a pooled connection.
///
/// The whole point is `Drop`: it runs during unwinding, so a panic anywhere in
/// driver code returns or destroys the connection instead of leaking it. R-022
/// names leaked connections as the other half of what bare `catch_unwind` misses.
pub struct ConnectionLease<C: Send + 'static> {
    conn: Option<C>,
    home: Arc<dyn ConnectionHome<C>>,
    session: SessionId,
    dirty: bool,
}

impl<C: Send + 'static> ConnectionLease<C> {
    pub fn new(conn: C, home: Arc<dyn ConnectionHome<C>>, session: SessionId) -> Self {
        Self { conn: Some(conn), home, session, dirty: false }
    }

    pub fn get(&self) -> &C {
        self.conn.as_ref().expect("lease used after drop")
    }

    pub fn get_mut(&mut self) -> &mut C {
        self.conn.as_mut().expect("lease used after drop")
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// Mark that this connection mutated session state, so the pool resets it on
    /// release instead of resetting unconditionally (T-040/R-036).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

impl<C: Send + 'static> Drop for ConnectionLease<C> {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else { return };

        // The load-bearing line: if we are unwinding, the connection's protocol
        // state is unknown and it must be torn down, never returned.
        if std::thread::panicking() {
            self.home.discard(conn, DiscardReason::Panic);
        } else {
            self.home.release(conn, self.dirty);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct RecordingHome {
        released: AtomicUsize,
        discarded: AtomicUsize,
        last_reason: NonPoisonMutex<Option<DiscardReason>>,
        last_dirty: NonPoisonMutex<Option<bool>>,
    }

    impl ConnectionHome<u32> for RecordingHome {
        fn release(&self, _conn: u32, dirty: bool) {
            self.released.fetch_add(1, Ordering::SeqCst);
            *self.last_dirty.lock() = Some(dirty);
        }
        fn discard(&self, _conn: u32, reason: DiscardReason) {
            self.discarded.fetch_add(1, Ordering::SeqCst);
            *self.last_reason.lock() = Some(reason);
        }
    }

    fn quiet<T>(f: impl FnOnce() -> T) -> T {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let out = f();
        std::panic::set_hook(prev);
        out
    }

    #[test]
    fn normal_drop_releases_to_the_pool() {
        let home = Arc::new(RecordingHome::default());
        {
            let mut lease = ConnectionLease::new(7u32, home.clone(), "s1".into());
            lease.mark_dirty();
            assert_eq!(*lease.get(), 7);
        }
        assert_eq!(home.released.load(Ordering::SeqCst), 1);
        assert_eq!(home.discarded.load(Ordering::SeqCst), 0);
        assert_eq!(*home.last_dirty.lock(), Some(true));
    }

    #[test]
    fn panic_unwinding_discards_instead_of_releasing() {
        let home = Arc::new(RecordingHome::default());
        let h = home.clone();

        let result = quiet(|| {
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                let _lease = ConnectionLease::new(9u32, h.clone(), "s2".into());
                panic!("driver exploded mid-decode");
            }))
        });

        assert!(result.is_err(), "panic should have been caught");
        // The connection did not leak, and was NOT handed back for reuse.
        assert_eq!(home.discarded.load(Ordering::SeqCst), 1);
        assert_eq!(home.released.load(Ordering::SeqCst), 0);
        assert_eq!(*home.last_reason.lock(), Some(DiscardReason::Panic));
    }

    #[test]
    fn caught_panic_becomes_a_driver_bug_error_naming_the_session() {
        let err = quiet(|| {
            guard_driver_call_blocking::<_, ()>(&"session-42".into(), || {
                panic!("todo!() in type_info.rs");
            })
        })
        .expect_err("panic must surface as an error");

        assert_eq!(err.kind, ErrorKind::DriverBug);
        assert!(err.message.contains("session-42"), "message must name the session: {}", err.message);
        assert!(err.message.contains("driver bug"), "message: {}", err.message);
        assert_eq!(err.payload.message.as_deref(), Some("todo!() in type_info.rs"));
    }

    #[tokio::test]
    async fn async_driver_panics_are_contained_too() {
        let err = {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let out = guard_driver_call::<_, ()>(&"async-session".into(), async {
                panic!("panic across an await");
            })
            .await;
            std::panic::set_hook(prev);
            out
        }
        .expect_err("async panic must surface as an error");

        assert_eq!(err.kind, ErrorKind::DriverBug);
        assert!(err.message.contains("async-session"));
    }

    #[test]
    fn successful_calls_pass_through_untouched() {
        let ok = guard_driver_call_blocking(&"s".into(), || Ok(41 + 1)).unwrap();
        assert_eq!(ok, 42);
    }

    #[test]
    fn mutex_stays_usable_after_a_panic_while_held() {
        let lock = Arc::new(NonPoisonMutex::new(vec![1u8, 2, 3]));
        let l = lock.clone();

        let _ = quiet(|| {
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                let mut guard = l.lock();
                guard.push(4);
                panic!("panic while holding the lock");
            }))
        });

        // std::sync::Mutex would return Err here forever and wedge the app.
        assert!(lock.was_poisoned(), "std considers it poisoned");
        let guard = lock.lock();
        assert_eq!(*guard, vec![1, 2, 3, 4], "state recovered, not lost");
    }
}
