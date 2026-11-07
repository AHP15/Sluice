//! SQL Server driver (T-080 / D-002, D-022, D-028).
//!
//! D-002: `tiberius` directly. What T-072 measured about it shapes this crate:
//!
//! - **Auth**: SQL Server logins are what is built and tested. Integrated
//!   Kerberos (`integrated-auth-gssapi`) and Entra ID tokens exist in
//!   tiberius but are unvalidated against a real instance (T-072 is held on
//!   exactly that), so they are not offered yet.
//! - **TLS**: `rustls`, with real verification; `TlsMode` maps onto
//!   `EncryptionLevel` and `trust_cert` (see [`tiberius_config`]).
//! - **Reset**: no reachable primitive, so `reset: reopen_only`, and only
//!   dirtied connections are reopened (`pool`, T-040 / R-036).
//! - **Cancel**: tiberius has no Attention API. D-030's close-and-declare and
//!   verified `KILL` arrive with SQL Server cancellation; until then this
//!   driver declares `cancel: none`, which is what the UI words Stop from.
//! - **Panics**: three ordinary types panic inside tiberius. Every row read
//!   here uses `try_get`, and T-006's guard at the IPC boundary contains the
//!   rest; a lease dropped by an unwinding panic is closed, never reused.
//!
//! What this crate does today: connect, publish a capability descriptor, pool
//! within the profile's budget, and navigate the catalog (T-081). Browse, raw
//! scripts, staged edits and export are declared unavailable with a message
//! saying so, until their own work lands.

pub mod error_map;
pub mod navigator;
pub mod pool;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sluice_core::{
    BatchSink, BrowseBatch, BrowseRequest, BrowseStreamEnd, CancelSlot, CancelSupport, Capabilities, CatalogSupport,
    Driver, DriverError, EditPlan, EditRequest, EditorSession, ErrorKind, ExecBudget, ExecutionResult, ExportBatch,
    ExportEnd, NavNode, NodeId, PoolStatsSnapshot, RawPayloadKind, RawRequest, ReadOnlyGuarantee, Redactor,
    ResetSupport, Secret, SessionAffinity, SqlLiterals, TlsMode, TransactionSupport, WireValue,
};

use crate::navigator::{rows, text};
use crate::pool::{Conn, Lease, Manager, Pool, PoolStats};

/// How long a request waits for a connection inside the budget (T-042).
pub const BUDGET_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Everything needed to open a connection. The password arrives resolved
/// from the keychain (D-007), wrapped in `Secret` (T-022), so `Debug` is safe.
#[derive(Clone, Debug)]
pub struct ConnectSpec {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<Secret<String>>,
    pub database: String,
    pub tls: TlsMode,
    pub application_name: String,
}

impl Default for ConnectSpec {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 1433,
            user: "sa".into(),
            password: None,
            database: "master".into(),
            tls: TlsMode::default(),
            application_name: "sluice".into(),
        }
    }
}

/// D-007 / T-021 onto TDS: a TLS mode is *required*, never preferred.
///
/// - `Verify` → `Required`, certificate chain and hostname verified;
/// - `AcceptInvalidCerts` → `Required` + `trust_cert` (encrypted, unverified,
///   and the UI shows its warning);
/// - `Disabled` → `NotSupported`: no TLS at all, not even for the login
///   packet. A server that forces encryption refuses it, which is correct.
pub fn tiberius_config(spec: &ConnectSpec) -> tiberius::Config {
    let mut c = tiberius::Config::new();
    c.host(&spec.host);
    c.port(spec.port);
    c.database(&spec.database);
    c.application_name(&spec.application_name);
    // The one place the plaintext is needed: handing it to the wire protocol.
    let password = spec.password.as_ref().map(|p| p.expose().to_string()).unwrap_or_default();
    c.authentication(tiberius::AuthMethod::sql_server(&spec.user, password));
    match spec.tls {
        TlsMode::Verify => c.encryption(tiberius::EncryptionLevel::Required),
        TlsMode::AcceptInvalidCerts => {
            c.encryption(tiberius::EncryptionLevel::Required);
            c.trust_cert();
        }
        TlsMode::Disabled => c.encryption(tiberius::EncryptionLevel::NotSupported),
    }
    c
}

/// The server, as probed at connect.
#[derive(Clone, Debug, Default)]
pub struct ServerInfo {
    /// `SERVERPROPERTY('ProductVersion')`, e.g. `16.0.4135.4`.
    pub version: Option<String>,
    /// `SERVERPROPERTY('EngineEdition')`: 5 is Azure SQL Database, 8 Azure
    /// SQL Managed Instance, the rest are boxed SQL Server.
    pub engine_edition: Option<i32>,
    /// The database the login landed in.
    pub database: String,
    /// The server's default collation, for the filter bar (T-015).
    pub collation: Option<String>,
}

impl ServerInfo {
    pub fn flavour(&self) -> &'static str {
        match self.engine_edition {
            Some(5) => "Azure SQL Database",
            Some(8) => "Azure SQL Managed Instance",
            Some(6) | Some(11) => "Azure Synapse",
            _ => "SQL Server",
        }
    }
}

type NavCache = Arc<std::sync::Mutex<HashMap<Option<NodeId>, Vec<NavNode>>>>;

pub struct SqlServerDriver {
    pool: Pool,
    stats: Arc<PoolStats>,
    budget_size: usize,
    server: ServerInfo,
    read_only: ReadOnlyGuarantee,
    catalog: CatalogSupport,
    max_connections: Option<i64>,
    redactor: Redactor,
    nav_cache: NavCache,
    /// T-051: set on a per-call copy (`with_cancel`), never the shared driver.
    cancel: Option<CancelSlot>,
    /// T-050: what one raw execution may retain for display.
    exec_budget: ExecBudget,
}

/// The named refusal for a capability this driver does not have yet.
fn not_yet(what: &str) -> DriverError {
    DriverError::new(ErrorKind::Unknown, format!("{what} is not available on SQL Server in this build of Sluice yet"))
}

impl SqlServerDriver {
    /// Connect and probe, with the profile's connection budget as the pool
    /// ceiling (T-042 / D-028), pooled and pinned together.
    pub async fn connect(spec: ConnectSpec, budget: usize) -> Result<Arc<Self>, DriverError> {
        let redactor = Redactor::new(spec.password.clone());
        let manager = Manager { config: tiberius_config(&spec) };
        let pool = Pool::builder(manager)
            .max_size(budget.max(1))
            .runtime(deadpool::Runtime::Tokio1)
            .wait_timeout(Some(BUDGET_WAIT))
            .build()
            .map_err(|e| redactor.error(DriverError::new(ErrorKind::ConnectionFailed, e.to_string())))?;

        let mut driver = Self {
            pool,
            stats: Arc::new(PoolStats::default()),
            budget_size: budget.max(1),
            server: ServerInfo::default(),
            read_only: ReadOnlyGuarantee::default(),
            catalog: CatalogSupport::default(),
            max_connections: None,
            redactor,
            nav_cache: Arc::default(),
            cancel: None,
            exec_budget: ExecBudget::default(),
        };

        // Prove the credentials now rather than on first expand, and learn
        // what the server is while the connection is open. Every probe is a
        // read-only SELECT, so the lease returns clean.
        let (server, read_only, catalog, max_connections) = driver
            .with_lease(false, async |c: &mut Conn| {
                let server = probe_server(c).await?;
                let read_only = probe_read_only(c).await;
                let catalog = navigator::probe(c).await;
                let max = rows(c, "SELECT CAST(@@MAX_CONNECTIONS AS int)", &[])
                    .await
                    .ok()
                    .and_then(|r| r.first().and_then(|r| navigator::int(r, 0).ok().flatten()))
                    .map(i64::from);
                Ok((server, read_only, catalog, max))
            })
            .await?;
        driver.server = server;
        driver.read_only = read_only;
        driver.catalog = catalog;
        driver.max_connections = max_connections;
        Ok(Arc::new(driver))
    }

    pub fn server(&self) -> &ServerInfo {
        &self.server
    }

    /// Run `f` on a leased connection, then release it — returned as-is when
    /// `dirty` is false, closed when it may have changed session state (D-028
    /// `reopen`, T-040). Every error leaving here is redacted (T-024).
    pub async fn with_lease<T>(
        &self,
        dirty: bool,
        f: impl AsyncFnOnce(&mut Conn) -> Result<T, DriverError>,
    ) -> Result<T, DriverError> {
        let out = async {
            let mut lease = Lease::acquire(&self.pool, self.stats.clone()).await?;
            if dirty {
                lease.mark_dirty();
            }
            let out = f(lease.conn()).await;
            // A failed call may have left the TDS stream mid-message; a
            // connection in that state must not be handed to the next caller.
            if out.as_ref().is_err_and(|e| e.kind == ErrorKind::ConnectionFailed) {
                lease.mark_dirty();
            }
            lease.release().await;
            out
        }
        .await;
        out.map_err(|e| self.redactor.error(e))
    }

    fn copy(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            stats: self.stats.clone(),
            budget_size: self.budget_size,
            server: self.server.clone(),
            read_only: self.read_only.clone(),
            catalog: self.catalog.clone(),
            max_connections: self.max_connections,
            redactor: self.redactor.clone(),
            nav_cache: self.nav_cache.clone(),
            cancel: self.cancel.clone(),
            exec_budget: self.exec_budget,
        }
    }
}

async fn probe_server(c: &mut Conn) -> Result<ServerInfo, DriverError> {
    // CAST everything: SERVERPROPERTY returns sql_variant, one of the three
    // types that panic inside tiberius (T-072).
    const SQL: &str = "SELECT
        CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)),
        CAST(SERVERPROPERTY('EngineEdition') AS int),
        CAST(DB_NAME() AS nvarchar(128)),
        CAST(SERVERPROPERTY('Collation') AS nvarchar(128))";
    let r = rows(c, SQL, &[]).await?;
    let row = r.first().ok_or_else(|| DriverError::new(ErrorKind::DriverBug, "server probe returned no row"))?;
    Ok(ServerInfo {
        version: text(row, 0)?,
        engine_edition: navigator::int(row, 1)?,
        database: text(row, 2)?.unwrap_or_default(),
        collation: text(row, 3)?,
    })
}

/// T-061 / D-027 on SQL Server: only what the **database** enforces counts.
///
/// A read-only database refuses every write, so that is `Enforced`. Holding a
/// database-wide write permission is `NotEnforced`. Anything else is
/// `Unknown`: SQL Server grants writes per object too, and claiming a
/// guarantee from the absence of a database-wide grant would over-claim.
async fn probe_read_only(c: &mut Conn) -> ReadOnlyGuarantee {
    const SQL: &str = "SELECT
        CAST(DATABASEPROPERTYEX(DB_NAME(), 'Updateability') AS nvarchar(32)),
        HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'INSERT'),
        HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'UPDATE'),
        HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'DELETE'),
        HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'ALTER')";
    let Ok(r) = rows(c, SQL, &[]).await else {
        return ReadOnlyGuarantee::Unknown { reason: "the permission probe could not run".into() };
    };
    let Some(row) = r.first() else {
        return ReadOnlyGuarantee::Unknown { reason: "the permission probe returned nothing".into() };
    };
    if text(row, 0).ok().flatten().as_deref() == Some("READ_ONLY") {
        return ReadOnlyGuarantee::Enforced { basis: "the database is read-only".into() };
    }
    let mut reasons = Vec::new();
    for (i, what) in [(1, "INSERT"), (2, "UPDATE"), (3, "DELETE"), (4, "ALTER")] {
        if navigator::int(row, i).ok().flatten() == Some(1) {
            reasons.push(format!("this login holds {what} on the database"));
        }
    }
    if reasons.is_empty() {
        ReadOnlyGuarantee::Unknown {
            reason: "no database-wide write permission, but SQL Server can grant writes per table, which is not checked".into(),
        }
    } else {
        ReadOnlyGuarantee::NotEnforced { reasons }
    }
}

#[async_trait]
impl Driver for SqlServerDriver {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            engine: "sqlserver".into(),
            server_version: self.server.version.clone(),
            flavour: Some(self.server.flavour().to_string()),
            // A TDS connection can be pinned for a tab's lifetime (D-028).
            affinity: SessionAffinity::Connection,
            // T-072: no reachable `sp_reset_connection`.
            reset: ResetSupport::ReopenOnly,
            // T-072 / D-030: no Attention API; KILL only once verified.
            cancel: CancelSupport::None,
            transactions: Some(TransactionSupport { interactive: true, max_idle_seconds: None }),
            raw_payload: RawPayloadKind::Script,
            node_kinds: navigator::node_kinds(),
            // Filter compilation arrives with browse; none are offered until
            // a browse exists to apply them to.
            filter_ops: Vec::new(),
            fixed_schema: true,
            supports_script: true,
            string_collation: self.server.collation.clone(),
            read_only: self.read_only.clone(),
            catalog: self.catalog.clone(),
        }
    }

    async fn navigator_children(&self, parent: Option<NodeId>) -> Result<Vec<NavNode>, DriverError> {
        if let Some(hit) = self.nav_cache.lock().unwrap_or_else(|p| p.into_inner()).get(&parent) {
            return Ok(hit.clone());
        }
        let key = parent.clone();
        let current = self.server.database.clone();
        let catalog = self.catalog.clone();
        let nodes = self
            .with_lease(false, async |c: &mut Conn| navigator::children(c, parent, &current, &catalog).await)
            .await?;
        // A notice is not catalog content: never cached, so the next expand
        // tries again (D-016).
        if !nodes.iter().any(navigator::is_notice) {
            self.nav_cache.lock().unwrap_or_else(|p| p.into_inner()).insert(key, nodes.clone());
        }
        Ok(nodes)
    }

    async fn browse(&self, _request: BrowseRequest) -> Result<ExecutionResult, DriverError> {
        Err(not_yet("Browsing a table"))
    }

    async fn preview_edits(&self, _edits: Vec<EditRequest>) -> Result<EditPlan, DriverError> {
        Err(not_yet("Editing rows"))
    }

    async fn apply_edits(&self, _edits: Vec<EditRequest>, _digest: Option<String>) -> Result<ExecutionResult, DriverError> {
        Err(not_yet("Editing rows"))
    }

    async fn raw(&self, _request: RawRequest) -> Result<ExecutionResult, DriverError> {
        Err(not_yet("Running scripts"))
    }

    async fn browse_batches(
        &self,
        _request: BrowseRequest,
        _batch_rows: usize,
        _emit: BatchSink<BrowseBatch>,
    ) -> Result<BrowseStreamEnd, DriverError> {
        Err(not_yet("Browsing a table"))
    }

    async fn export_browse(
        &self,
        _request: BrowseRequest,
        _batch_rows: usize,
        _emit: BatchSink<ExportBatch>,
    ) -> Result<ExportEnd, DriverError> {
        Err(not_yet("Export"))
    }

    async fn export_query(&self, _statement: String, _batch_rows: usize, _emit: BatchSink<ExportBatch>) -> Result<ExportEnd, DriverError> {
        Err(not_yet("Export"))
    }

    fn export_literals(&self, _relation: Option<NodeId>, _placeholder: String) -> Result<Box<dyn SqlLiterals>, DriverError> {
        Err(not_yet("SQL INSERT export"))
    }

    async fn fetch_value(&self, _handle: String) -> Result<WireValue, DriverError> {
        Err(not_yet("Fetching a full value"))
    }

    async fn stream_value_to_file(&self, _handle: String, _dest: std::path::PathBuf) -> Result<u64, DriverError> {
        Err(not_yet("Saving a value to a file"))
    }

    fn with_cancel(self: Arc<Self>, slot: CancelSlot) -> Arc<dyn Driver> {
        let mut copy = self.copy();
        copy.cancel = Some(slot);
        Arc::new(copy)
    }

    fn with_budget(self: Arc<Self>, budget: ExecBudget) -> Arc<dyn Driver> {
        let mut copy = self.copy();
        copy.exec_budget = budget;
        Arc::new(copy)
    }

    async fn open_pinned(&self) -> Result<Box<dyn EditorSession>, DriverError> {
        Err(not_yet("An editor session"))
    }

    async fn run_pinned(&self, _session: &mut dyn EditorSession, _script: String) -> Result<ExecutionResult, DriverError> {
        Err(not_yet("Running scripts"))
    }

    fn refresh_catalog(&self) {
        self.nav_cache.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    fn pool_stats(&self) -> PoolStatsSnapshot {
        self.stats.snapshot()
    }

    fn connection_budget(&self) -> usize {
        self.budget_size
    }

    fn max_connections(&self) -> Option<i64> {
        self.max_connections
    }

    fn redactor(&self) -> &Redactor {
        &self.redactor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_modes_never_fall_back_to_plaintext() {
        // Verify and AcceptInvalidCerts both *require* encryption; tiberius's
        // config keeps its fields private, so this checks the Debug shape.
        for (mode, want) in [
            (TlsMode::Verify, "Required"),
            (TlsMode::AcceptInvalidCerts, "Required"),
            (TlsMode::Disabled, "NotSupported"),
        ] {
            let c = tiberius_config(&ConnectSpec { tls: mode, ..Default::default() });
            let dbg = format!("{c:?}");
            assert!(dbg.contains(want), "{mode:?}: {dbg}");
        }
        let trusting = format!("{:?}", tiberius_config(&ConnectSpec { tls: TlsMode::AcceptInvalidCerts, ..Default::default() }));
        let verifying = format!("{:?}", tiberius_config(&ConnectSpec { tls: TlsMode::Verify, ..Default::default() }));
        assert_ne!(trusting, verifying, "accept-invalid must differ from verify (trust_cert)");
    }

    #[test]
    fn neither_the_spec_nor_the_tiberius_config_debug_shows_the_password() {
        let s = ConnectSpec { password: Some(Secret::new("hunter2".into())), ..Default::default() };
        assert!(!format!("{s:?}").contains("hunter2"));
        assert!(!format!("{:?}", tiberius_config(&s)).contains("hunter2"));
    }

    #[test]
    fn flavours_come_from_the_probed_engine_edition() {
        let f = |e| ServerInfo { engine_edition: e, ..Default::default() }.flavour();
        assert_eq!(f(Some(3)), "SQL Server");
        assert_eq!(f(Some(5)), "Azure SQL Database");
        assert_eq!(f(None), "SQL Server");
    }
}
