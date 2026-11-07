//! Pinned editor sessions (T-041 / D-028).
//!
//! On an engine that declares `affinity: connection`, a SQL editor tab gets
//! one dedicated server session for its lifetime — the `psql`-like model D-028
//! keeps from D-015. What this module guarantees:
//!
//! - **Pinned.** Every statement the tab runs goes to the same connection, so
//!   `SET`, temp tables, prepared statements and an explicit transaction all
//!   persist between runs. The connection is a pool [`Lease`] held for the
//!   tab's lifetime and marked dirty, so when the tab ends it goes back through
//!   the declared reset (`ROLLBACK`, then `DISCARD ALL`), never as-is.
//! - **Opt-in transactions, never implicitly committed.** A transaction exists
//!   only when the user begins one (the Begin control, or their own `BEGIN`).
//!   The session probes its transaction state after every run, so the UI's
//!   indicator reflects what the *server* says, not what the app assumed.
//! - **Rollback, never commit, on the way out.** Closing the tab, closing the
//!   window, or idling past the timeout with a transaction open all roll back.
//! - **Loss is surfaced.** If the connection drops, the session is marked lost
//!   and refuses to run anything: reconnecting would hand the user a fresh
//!   session without the state they believe they have (R-037). Only an
//!   explicit reconnect — a new session — continues.

use std::time::{Duration, Instant};

use sluice_core::{DriverError, ErrorKind, ExecutionResult, TxDisposition, TxMode};

use crate::error_map;
use crate::lease::Lease;

// T-077: the session vocabulary is engine-neutral and lives in core; the
// re-exports keep this module's paths stable.
pub use sluice_core::{lost_message, SessionHeld, SessionStatus, TxState, IDLE_TRANSACTION_TIMEOUT};

const HELD: &str = "SELECT \
    (SELECT string_agg(name, ',' ORDER BY name) FROM pg_settings WHERE source = 'session'), \
    (SELECT count(*) FROM pg_class WHERE relpersistence = 't' AND relnamespace = pg_my_temp_schema()), \
    (SELECT count(*) FROM pg_prepared_statements WHERE from_sql), \
    (SELECT count(*) FROM pg_cursors WHERE name <> ''), \
    (SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND pid = pg_backend_pid()), \
    (SELECT count(*) FROM pg_listening_channels())";

pub struct PinnedSession {
    lease: Option<Lease>,
    tx: TxState,
    /// The open transaction's xid, once it has one (a write assigned it). Used
    /// to learn how a transaction the *script* ended actually ended.
    xid: Option<i64>,
    last_used: Instant,
    lost: Option<String>,
    /// T-043: what the server said the session held after the last run.
    held: SessionHeld,
    /// For the cancel handle: the protocol cancel uses the session's TLS mode.
    tls: crate::tls::TlsMode,
}

const PROBE: &str = "SELECT statement_timestamp() <> transaction_timestamp(), txid_current_if_assigned()";

impl PinnedSession {
    pub(crate) fn new(mut lease: Lease, tls: crate::tls::TlsMode) -> Self {
        // Whatever the tab does is session state by definition.
        lease.mark_dirty();
        Self {
            lease: Some(lease),
            tx: TxState::Idle,
            xid: None,
            last_used: Instant::now(),
            lost: None,
            held: SessionHeld::default(),
            tls,
        }
    }

    pub fn tx(&self) -> TxState {
        self.tx
    }

    pub fn status(&self) -> SessionStatus {
        SessionStatus {
            tx: self.tx,
            tx_idle_secs: (self.tx != TxState::Idle).then(|| self.last_used.elapsed().as_secs()),
            idle_timeout_secs: IDLE_TRANSACTION_TIMEOUT.as_secs(),
            lost: self.lost.clone(),
            holds: self.held.describe(),
        }
    }

    pub fn idle_for(&self) -> Duration {
        self.last_used.elapsed()
    }

    pub(crate) fn client(&self) -> Result<&tokio_postgres::Client, DriverError> {
        if let Some(why) = &self.lost {
            return Err(DriverError::new(ErrorKind::ConnectionFailed, why.clone()));
        }
        let lease = self.lease.as_ref().ok_or_else(|| DriverError::new(ErrorKind::ConnectionFailed, "this editor session is closed"))?;
        Ok(lease.client())
    }

    pub fn cancel_token(&self) -> Option<tokio_postgres::CancelToken> {
        self.client().ok().map(|c| c.cancel_token())
    }

    /// If the connection has dropped, record it and say what was lost.
    fn check_lost(&mut self) -> bool {
        let closed = self.lease.as_ref().is_some_and(|l| l.client().is_closed());
        if closed && self.lost.is_none() {
            // T-043: name what was lost, from the last server report.
            let mut held = self.held.clone();
            held.transaction = self.tx != TxState::Idle;
            self.lost = Some(lost_message("The editor's connection was lost.", &held));
            self.tx = TxState::Idle;
        }
        closed
    }

    /// Ask the server what transaction state the session is in.
    async fn probe(&mut self) -> Result<(), DriverError> {
        let client = self.client()?;
        // Simple protocol on purpose: one Query message, so outside a block
        // the probe is the first command of its own implicit transaction and
        // the two timestamps are equal by definition. Under the extended
        // protocol the transaction can start at an earlier message (Parse /
        // Bind) than the one `statement_timestamp` records, and every idle
        // session read as "open".
        match client.simple_query(PROBE).await {
            Ok(msgs) => {
                let row = msgs.iter().find_map(|m| match m {
                    tokio_postgres::SimpleQueryMessage::Row(r) => Some(r),
                    _ => None,
                });
                let Some(row) = row else {
                    return Err(DriverError::new(ErrorKind::DriverBug, "transaction probe returned no row"));
                };
                let open = row.get(0) == Some("t");
                let xid: Option<i64> = row.get(1).and_then(|x| x.parse().ok());
                self.tx = if open { TxState::Open } else { TxState::Idle };
                self.xid = if open { xid.or(self.xid) } else { None };
                Ok(())
            }
            // 25P02: in a failed transaction block — the probe is refused too.
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::IN_FAILED_SQL_TRANSACTION) => {
                self.tx = TxState::Failed;
                Ok(())
            }
            Err(e) => {
                if self.check_lost() {
                    return Err(DriverError::new(ErrorKind::ConnectionFailed, self.lost.clone().unwrap_or_default()));
                }
                Err(error_map::map_error(&e, Some(PROBE)))
            }
        }
    }

    /// T-043: ask the server what session state this connection holds now.
    ///
    /// In a failed transaction block the server answers nothing; that is
    /// reported as holding a transaction, which is true and is what makes the
    /// session ineligible for release.
    pub async fn held(&mut self) -> Result<SessionHeld, DriverError> {
        if self.tx == TxState::Failed {
            let mut h = self.held.clone();
            h.transaction = true;
            return Ok(h);
        }
        let client = self.client()?;
        let msgs = match client.simple_query(HELD).await {
            Ok(m) => m,
            Err(e) => {
                self.check_lost();
                return Err(error_map::map_error(&e, Some(HELD)));
            }
        };
        let row = msgs
            .iter()
            .find_map(|m| match m {
                tokio_postgres::SimpleQueryMessage::Row(r) => Some(r),
                _ => None,
            })
            .ok_or_else(|| DriverError::new(ErrorKind::DriverBug, "session-state query returned no row"))?;
        let int = |i: usize| row.get(i).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        let held = SessionHeld {
            settings: row.get(0).map(|v| v.split(',').map(str::to_string).collect()).unwrap_or_default(),
            temp_tables: int(1),
            prepared: int(2),
            cursors: int(3),
            advisory_locks: int(4),
            listening: int(5),
            transaction: self.tx != TxState::Idle,
        };
        self.held = held.clone();
        Ok(held)
    }

    /// How a transaction that was open before a run and is gone after it ended.
    async fn ended_how(&self, xid: Option<i64>) -> TxDisposition {
        let Some(xid) = xid else { return TxDisposition::Unknown };
        let Ok(client) = self.client() else { return TxDisposition::Unknown };
        match client.query_one("SELECT txid_status($1)", &[&xid]).await {
            Ok(row) => match row.get::<_, Option<String>>(0).as_deref() {
                Some("committed") => TxDisposition::Committed,
                Some("aborted") => TxDisposition::RolledBack { rolled_back: vec![] },
                _ => TxDisposition::Unknown,
            },
            Err(_) => TxDisposition::Unknown,
        }
    }

    /// Run a raw script on the pinned connection.
    pub async fn run(&mut self, script: &str, budget: sluice_core::ExecBudget) -> Result<ExecutionResult, DriverError> {
        let before = self.tx;
        let xid_before = self.xid;
        let client = self.client()?;
        let mut result = crate::run_script(client, script, budget).await?;
        self.last_used = Instant::now();

        if self.check_lost() {
            return Err(DriverError::new(ErrorKind::ConnectionFailed, self.lost.clone().unwrap_or_default()));
        }
        self.probe().await?;
        // T-043: keep what the session holds current, for the indicator and
        // for naming what a loss would take with it.
        let _ = self.held().await;

        // T-041 / R-041: correct the raw path's implicit-transaction reading
        // for a session where an explicit transaction was or is in force.
        match (before, self.tx) {
            (TxState::Idle, TxState::Idle) => {}
            (_, TxState::Open | TxState::Failed) => {
                result.tx.mode = TxMode::ExplicitUser;
                result.tx.disposition = TxDisposition::Open;
            }
            (TxState::Open | TxState::Failed, TxState::Idle) => {
                result.tx.mode = TxMode::ExplicitUser;
                result.tx.disposition = if before == TxState::Failed {
                    // An aborted transaction can only end in rollback, even
                    // if the script said COMMIT.
                    TxDisposition::RolledBack { rolled_back: vec![] }
                } else {
                    self.ended_how(xid_before).await
                };
            }
        }
        Ok(result)
    }

    async fn control(&mut self, sql: &str) -> Result<SessionStatus, DriverError> {
        let client = self.client()?;
        let r = client.batch_execute(sql).await;
        self.last_used = Instant::now();
        if let Err(e) = r {
            if self.check_lost() {
                return Err(DriverError::new(ErrorKind::ConnectionFailed, self.lost.clone().unwrap_or_default()));
            }
            return Err(error_map::map_error(&e, Some(sql)));
        }
        self.probe().await?;
        let _ = self.held().await;
        Ok(self.status())
    }

    /// The opt-in: begin an explicit transaction.
    pub async fn begin(&mut self) -> Result<SessionStatus, DriverError> {
        if self.tx != TxState::Idle {
            return Err(DriverError::new(ErrorKind::Conflict, "a transaction is already open"));
        }
        self.control("BEGIN").await
    }

    pub async fn commit(&mut self) -> Result<SessionStatus, DriverError> {
        match self.tx {
            TxState::Open => self.control("COMMIT").await,
            TxState::Failed => Err(DriverError::new(
                ErrorKind::Conflict,
                "the transaction failed; it can only be rolled back",
            )),
            TxState::Idle => Err(DriverError::new(ErrorKind::Conflict, "no transaction is open")),
        }
    }

    pub async fn rollback(&mut self) -> Result<SessionStatus, DriverError> {
        self.control("ROLLBACK").await
    }

    /// End the session: roll back anything open (never commit) and return
    /// the connection through the declared reset.
    pub async fn close(mut self) {
        if let Some(lease) = self.lease.take() {
            if !lease.client().is_closed() {
                let _ = lease.client().batch_execute("ROLLBACK").await;
            }
            lease.release().await;
        }
    }
}

/// T-077: the engine-neutral face of a pinned Postgres session.
#[async_trait::async_trait]
impl sluice_core::EditorSession for PinnedSession {
    fn tx(&self) -> TxState {
        PinnedSession::tx(self)
    }
    fn status(&self) -> SessionStatus {
        PinnedSession::status(self)
    }
    fn idle_for(&self) -> Duration {
        PinnedSession::idle_for(self)
    }
    fn cancel_handle(&self) -> Option<std::sync::Arc<dyn sluice_core::CancelHandle>> {
        self.cancel_token().map(|t| crate::lease::PgCancel::new(t, self.tls) as std::sync::Arc<dyn sluice_core::CancelHandle>)
    }
    async fn held(&mut self) -> Result<SessionHeld, DriverError> {
        PinnedSession::held(self).await
    }
    async fn run(&mut self, script: &str, budget: sluice_core::ExecBudget) -> Result<ExecutionResult, DriverError> {
        PinnedSession::run(self, script, budget).await
    }
    async fn begin(&mut self) -> Result<SessionStatus, DriverError> {
        PinnedSession::begin(self).await
    }
    async fn commit(&mut self) -> Result<SessionStatus, DriverError> {
        PinnedSession::commit(self).await
    }
    async fn rollback(&mut self) -> Result<SessionStatus, DriverError> {
        PinnedSession::rollback(self).await
    }
    async fn close(self: Box<Self>) {
        PinnedSession::close(*self).await
    }
}
