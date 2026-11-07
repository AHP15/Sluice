//! Cancellable, time-limited query execution (T-051 / D-010).
//!
//! D-010: "Every query gets a real cancel handle; where an engine cannot cancel
//! cleanly the fallback is dropping the connection, and that limit is declared
//! in the capability descriptor. Default per-profile statement timeout."
//!
//! Each call runs as its own task with its own [`CancelSlot`]:
//!
//! 1. **Cancel** sends the engine's protocol cancel (the slot holds the leased
//!    connection's token), which stops the query *on the server*.
//! 2. **Fallback**: if the call has not finished [`CANCEL_GRACE`] later — the
//!    cancel was not delivered, or the engine ignored it — the task is aborted.
//!    Dropping it drops the lease, and a lease dropped without release closes
//!    its connection (T-039), so the connection is discarded, never reused.
//! 3. **Timeout**: the profile's statement timeout is enforced by the server
//!    (a startup option). A client watchdog backs it up at timeout + grace, for
//!    servers or proxies that do not honour it.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sluice_core::{CancelSlot, Driver, DriverError, ErrorKind};
use tokio::task::AbortHandle;

/// How long a cancelled query gets to stop before its connection is dropped.
pub const CANCEL_GRACE: Duration = Duration::from_secs(3);

/// D-010's default per-profile statement timeout, for profiles that set none.
pub const DEFAULT_STATEMENT_TIMEOUT: Duration = Duration::from_secs(300);

/// Resolve a profile's setting: unset → the default; `0` → no timeout.
pub fn statement_timeout(secs: Option<u32>) -> Option<Duration> {
    match secs {
        None => Some(DEFAULT_STATEMENT_TIMEOUT),
        Some(0) => None,
        Some(s) => Some(Duration::from_secs(u64::from(s))),
    }
}

struct Running {
    driver: Arc<dyn Driver>,
    slot: CancelSlot,
    abort: AbortHandle,
}

/// In-flight queries the UI can cancel, by its query id.
#[derive(Default)]
pub struct Queries(Mutex<HashMap<String, Running>>);

impl Queries {
    fn insert(&self, id: String, r: Running) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(id, r);
    }
    fn remove(&self, id: &str) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
    }

    /// Cancel a running query: protocol cancel now, connection drop after the
    /// grace period if it is still running. Returns whether it was found.
    pub async fn cancel(&self, id: &str) -> bool {
        let found = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .map(|r| (r.driver.clone(), r.slot.clone(), r.abort.clone()));
        let Some((driver, slot, abort)) = found else { return false };
        driver.cancel(&slot).await;
        tokio::spawn(async move {
            tokio::time::sleep(CANCEL_GRACE).await;
            abort.abort(); // no-op if it already finished
        });
        true
    }
}

/// Run one driver call, cancellable via `query_id` and bounded by `limit`.
pub async fn run<T, F, Fut>(
    queries: &Queries,
    query_id: Option<String>,
    driver: Arc<dyn Driver>,
    limit: Option<Duration>,
    call: F,
) -> Result<T, DriverError>
where
    T: Send + 'static,
    F: FnOnce(Arc<dyn Driver>) -> Fut,
    Fut: Future<Output = Result<T, DriverError>> + Send + 'static,
{
    let slot = CancelSlot::default();
    let mut task = tokio::spawn(call(driver.clone().with_cancel(slot.clone())));
    if let Some(id) = &query_id {
        queries.insert(id.clone(), Running { driver: driver.clone(), slot: slot.clone(), abort: task.abort_handle() });
    }

    let joined = match limit {
        Some(l) => match tokio::time::timeout(l + CANCEL_GRACE, &mut task).await {
            Ok(j) => Some(j),
            Err(_) => {
                // The server's own timeout did not end it: cancel, then drop.
                driver.cancel(&slot).await;
                task.abort();
                None
            }
        },
        None => Some((&mut task).await),
    };
    if let Some(id) = &query_id {
        queries.remove(id);
    }

    match joined {
        None => Err(DriverError::new(
            ErrorKind::Timeout,
            "the query ran past its statement timeout and was stopped; its connection was closed",
        )),
        Some(Ok(result)) => result,
        Some(Err(e)) if e.is_cancelled() => Err(DriverError::new(
            ErrorKind::Cancelled,
            "the server did not acknowledge the cancel in time, so its connection was closed",
        )),
        Some(Err(e)) => Err(DriverError::new(ErrorKind::DriverBug, format!("driver task failed: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn driver() -> Option<Arc<dyn Driver>> {
        crate::engines::test_postgres(sluice_core::DEFAULT_CONNECTION_BUDGET).await
    }

    /// D-010's fallback: a call the engine's cancel cannot stop is dropped
    /// after the grace period instead of running on.
    #[tokio::test]
    async fn a_call_that_ignores_cancel_is_dropped_after_the_grace_period() {
        let Some(d) = driver().await else { return };
        let queries = Arc::new(Queries::default());
        let started = std::time::Instant::now();
        let call = {
            let q = queries.clone();
            tokio::spawn(async move {
                run(&q, Some("q1".into()), d, None, |_d| async move {
                    // Holds no lease, so the protocol cancel has nothing to hit.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok::<(), DriverError>(())
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(queries.cancel("q1").await, "the query was registered");
        let e = call.await.unwrap().unwrap_err();
        assert_eq!(e.kind, ErrorKind::Cancelled);
        assert!(e.message.contains("connection was closed"), "{}", e.message);
        let took = started.elapsed();
        assert!(took >= CANCEL_GRACE && took < CANCEL_GRACE + Duration::from_secs(2), "{took:?}");
        assert!(!queries.cancel("q1").await, "a finished query is unregistered");
    }

    /// The client watchdog backs up the server's statement timeout.
    #[tokio::test]
    async fn the_watchdog_stops_a_call_past_its_timeout() {
        let Some(d) = driver().await else { return };
        let e = run(&Queries::default(), None, d, Some(Duration::from_millis(100)), |_d| async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok::<(), DriverError>(())
        })
        .await
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::Timeout);
    }

    #[test]
    fn profile_timeouts_resolve_to_the_default_off_or_a_value() {
        assert_eq!(statement_timeout(None), Some(DEFAULT_STATEMENT_TIMEOUT));
        assert_eq!(statement_timeout(Some(0)), None);
        assert_eq!(statement_timeout(Some(30)), Some(Duration::from_secs(30)));
    }
}
