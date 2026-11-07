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
//! within the profile's budget, navigate the catalog (T-081), and run
//! structured browse — compiled to parameterised T-SQL with keyset or
//! declared `OFFSET … FETCH` paging and lossless per-type encoding (T-082,
//! see [`browse`]). Raw scripts, staged edits and export are declared
//! unavailable with a message saying so, until their own work lands.

pub mod browse;
pub mod error_map;
pub mod navigator;
pub mod pool;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sluice_core::{
    BatchSink, BrowseBatch, BrowseRequest, BrowseStreamEnd, CancelSlot, CancelSupport, Capabilities, CatalogSupport,
    Driver, DriverError, EditGate, EditPlan, EditRequest, EditorSession, ErrorKind, ExecBudget, ExecutionResult,
    ExportBatch, ExportEnd, FilterArity, FilterOpDecl, NavNode, NodeId, PoolStatsSnapshot, RawPayloadKind, RawRequest,
    ReadOnlyGuarantee, Redactor, ResetSupport, Secret, SessionAffinity, SqlLiterals, TlsMode, TransactionSupport,
    TypeClass, WireValue,
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
    /// T-018 / R-008: the per-value byte ceiling browse applies in SQL.
    ceiling: i64,
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
            ceiling: browse::DEFAULT_VALUE_CEILING,
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
            ceiling: self.ceiling,
        }
    }

    /// A copy with a different per-value ceiling (T-018), sharing the pool.
    pub fn with_ceiling(self: Arc<Self>, ceiling: i64) -> Arc<Self> {
        let mut copy = self.copy();
        copy.ceiling = ceiling;
        Arc::new(copy)
    }

    /// Read the relation's declared shape from the catalog and compile the
    /// request against it (T-082). Never `SELECT *`: the projection is
    /// written per declared type, so no column reaches tiberius's decoder in
    /// a form it would round or panic on (T-072).
    pub async fn compile_browse(&self, c: &mut Conn, request: &BrowseRequest) -> Result<browse::CompiledBrowse, DriverError> {
        if !navigator::is_relation_kind(&request.relation.kind) {
            return Err(DriverError::new(
                ErrorKind::NotFound,
                format!("a `{}` node cannot be browsed", request.relation.kind),
            ));
        }
        let (db, schema, rel) = browse::qualified(&request.relation)?;
        let found = rows(c, &browse::shape_sql(&db), &[&schema, &rel]).await?;
        let shape = browse::shape_from_rows(&found)?;
        browse::compile(&request.relation, &shape, request, self.ceiling)
    }

    /// Structured browse, streamed in batches (T-049 / D-010).
    ///
    /// Rows are read from the TDS stream as `emit` grants credit. When the
    /// consumer stops mid-page, the rest of the page is still on the wire and
    /// tiberius has no Attention to stop it (T-072), so the connection is
    /// closed rather than reused — the server abandons the request on
    /// disconnect.
    async fn stream_browse(
        &self,
        request: &BrowseRequest,
        batch_rows: usize,
        emit: &mut BatchSink<BrowseBatch>,
    ) -> Result<BrowseStreamEnd, DriverError> {
        use futures_util::TryStreamExt as _;

        let batch_rows = batch_rows.max(1);
        let stopped = |rows| BrowseStreamEnd { rows, truncated: false, stopped: true, page: None };
        let out = async {
            let mut lease = Lease::acquire(&self.pool, self.stats.clone()).await?;
            // (end, whether the connection was abandoned mid-stream)
            let result: Result<(BrowseStreamEnd, bool), DriverError> = async {
                let c = lease.conn();
                let compiled = self.compile_browse(c, request).await?;
                if !emit(BrowseBatch::Columns(compiled.column_descs(), read_only_gate())).await {
                    return Ok((stopped(0), false));
                }
                let params = compiled.sql_params();
                let mut stream = c
                    .query(compiled.sql.as_str(), &params)
                    .await
                    .map_err(|e| error_map::map_error(&e))?
                    .into_row_stream();
                let mut sent = 0usize;
                let mut last: Option<Vec<WireValue>> = None;
                let mut batch: Vec<Vec<WireValue>> = Vec::with_capacity(batch_rows);
                while let Some(row) = stream.try_next().await.map_err(|e| error_map::map_error(&e))? {
                    let decoded = browse::decode_row(&compiled, &row);
                    last = Some(decoded.clone());
                    batch.push(decoded);
                    if batch.len() == batch_rows {
                        let n = batch.len();
                        if !emit(BrowseBatch::Rows(std::mem::take(&mut batch))).await {
                            return Ok((stopped(sent), true));
                        }
                        sent += n;
                    }
                }
                drop(stream);
                if !batch.is_empty() {
                    let n = batch.len();
                    if !emit(BrowseBatch::Rows(batch)).await {
                        return Ok((stopped(sent), false));
                    }
                    sent += n;
                }
                let page = compiled.paging.page_info(&compiled.columns, last.as_deref(), sent, compiled.limit);
                Ok((
                    BrowseStreamEnd { rows: sent, truncated: sent as u32 >= compiled.limit, stopped: false, page: Some(page) },
                    false,
                ))
            }
            .await;
            match &result {
                Ok((_, true)) => lease.mark_dirty(),
                Err(e) if e.kind == ErrorKind::ConnectionFailed => lease.mark_dirty(),
                _ => {}
            }
            lease.release().await;
            result.map(|(end, _)| end)
        }
        .await;
        out.map_err(|e| self.redactor.error(e))
    }
}

/// Staged edits on SQL Server are T-085's; until then a browsed relation says
/// why it cannot be edited rather than offering an edit that would fail.
fn read_only_gate() -> EditGate {
    EditGate::ReadOnly { reason: "editing rows is not available on SQL Server in this build of Sluice yet".into() }
}

/// D-005's core operators, compiled by [`browse`]. Case sensitivity is the
/// server collation's (R-009): reported, not overridden; the per-predicate
/// toggle opts out with `LOWER()` on both sides.
fn filter_ops(strings_case_sensitive: bool) -> Vec<FilterOpDecl> {
    let all = vec![
        TypeClass::Boolean,
        TypeClass::Integer,
        TypeClass::Float,
        TypeClass::ExactNumeric,
        TypeClass::Text,
        TypeClass::Timestamp,
        TypeClass::Uuid,
    ];
    let op = |id: &str, label: &str, arity: FilterArity, classes: Vec<TypeClass>, strings: bool| FilterOpDecl {
        id: id.into(),
        label: label.into(),
        arity,
        type_classes: classes,
        case_sensitive: strings.then_some(strings_case_sensitive),
        case_insensitive_toggle: strings,
    };
    vec![
        op("eq", "equals", FilterArity::One, all.clone(), true),
        op("ne", "not equals", FilterArity::One, all.clone(), true),
        op("gt", "greater than", FilterArity::One, all.clone(), false),
        op("gte", "at least", FilterArity::One, all.clone(), false),
        op("lt", "less than", FilterArity::One, all.clone(), false),
        op("lte", "at most", FilterArity::One, all.clone(), false),
        op("contains", "contains", FilterArity::One, vec![TypeClass::Text], true),
        op("starts_with", "starts with", FilterArity::One, vec![TypeClass::Text], true),
        op("ends_with", "ends with", FilterArity::One, vec![TypeClass::Text], true),
        op("in", "is one of", FilterArity::List, all.clone(), true),
        op("between", "between", FilterArity::Two, all, false),
        op("is_null", "is null", FilterArity::None, vec![], false),
        op("is_not_null", "is not null", FilterArity::None, vec![], false),
    ]
}

/// A SQL Server collation distinguishes case when its name says `_CS`, or
/// when it is binary (`_BIN`, `_BIN2`). The installation default is `_CI`.
fn collation_case_sensitive(collation: Option<&str>) -> bool {
    collation.is_some_and(|c| {
        let c = c.to_ascii_uppercase();
        c.contains("_CS") || c.contains("_BIN")
    })
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
            // T-082 / D-005: the core set, compiled to bound T-SQL.
            filter_ops: filter_ops(collation_case_sensitive(self.server.collation.as_deref())),
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

    /// D-029: structured browse yields exactly one `rows` outcome, with the
    /// paging strategy declared (D-010).
    async fn browse(&self, request: BrowseRequest) -> Result<ExecutionResult, DriverError> {
        self.with_lease(false, async |c: &mut Conn| {
            let compiled = self.compile_browse(c, &request).await?;
            let params = compiled.sql_params();
            let found = rows(c, &compiled.sql, &params).await?;
            let out: Vec<Vec<WireValue>> = found.iter().map(|r| browse::decode_row(&compiled, r)).collect();
            let truncated = out.len() as u32 >= compiled.limit;
            let page = compiled.paging.page_info(&compiled.columns, out.last().map(Vec::as_slice), out.len(), compiled.limit);
            let mut result = ExecutionResult::single_rows(compiled.column_descs(), out, truncated);
            result.page = Some(page);
            result.edit = Some(read_only_gate());
            Ok(result)
        })
        .await
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
        request: BrowseRequest,
        batch_rows: usize,
        mut emit: BatchSink<BrowseBatch>,
    ) -> Result<BrowseStreamEnd, DriverError> {
        self.stream_browse(&request, batch_rows, &mut emit).await
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

    /// T-013: the conformance check every driver's tests run.
    #[test]
    fn every_core_filter_operator_is_declared() {
        assert!(sluice_core::missing_core_ops(&filter_ops(false)).is_empty());
        let contains = filter_ops(false).into_iter().find(|o| o.id == "contains").unwrap();
        assert_eq!(contains.case_sensitive, Some(false));
        assert!(contains.case_insensitive_toggle);
    }

    #[test]
    fn case_sensitivity_is_read_from_the_collation_name() {
        assert!(!collation_case_sensitive(Some("SQL_Latin1_General_CP1_CI_AS")));
        assert!(collation_case_sensitive(Some("Latin1_General_CS_AS")));
        assert!(collation_case_sensitive(Some("Latin1_General_BIN2")));
        assert!(!collation_case_sensitive(None));
    }

    #[test]
    fn flavours_come_from_the_probed_engine_edition() {
        let f = |e| ServerInfo { engine_edition: e, ..Default::default() }.flavour();
        assert_eq!(f(Some(3)), "SQL Server");
        assert_eq!(f(Some(5)), "Azure SQL Database");
        assert_eq!(f(None), "SQL Server");
    }
}
