//! MongoDB driver (T-087 / D-002, D-022, D-028).
//!
//! D-002: the official `mongodb` crate directly. What the T-073 / T-046 spikes
//! measured about it (`spikes/mongo-contract/`) shapes this crate:
//!
//! - **Pool**: the crate owns its connection pool and hands out no individual
//!   connections (R-011), so nothing is layered on top of it. D-028's one
//!   per-profile budget is applied as the pool's `max_pool_size` per server.
//!   The crate also keeps one monitoring socket per server outside that pool;
//!   polling monitoring is chosen so it is one, not two (streaming adds an RTT
//!   socket).
//! - **Affinity**: `session` (T-046's verdict). `Client::start_session()`
//!   yields an owned logical session that held a real interactive
//!   multi-document transaction with snapshot isolation; in 3.x it is passed as
//!   `.session(&mut s)` on each action builder, not the 2.x `..._with_session`
//!   variants R-039's text names.
//! - **Transactions**: only on a replica set or a sharded cluster — a
//!   standalone `mongod` cannot open one at all, so there `transactions` is
//!   declared absent. Where they exist the server aborts any transaction older
//!   than `transactionLifetimeLimitSeconds` (60 by default; read live at
//!   connect where the login may), and that ceiling is declared.
//! - **Reset**: session state lives in logical sessions, not on sockets, so a
//!   pooled connection has nothing to leak: `reset: not_applicable`.
//! - **Cancel**: `killOp` arrives with raw commands; until then `cancel: none`.
//! - **Panics**: the crate's types are all `Clone` handles with no lock of
//!   ours held across a call; the navigator cache is a `NonPoisonMutex`
//!   (R-022), and T-006's guard at the IPC boundary contains the rest.
//!
//! What this crate does today: connect, publish a capability descriptor, pool
//! within the profile's budget, and list databases and collections. Browse
//! (T-088), raw commands (T-089), staged edits and export are declared
//! unavailable with a message saying so, until their own work lands.

pub mod error_map;
pub mod navigator;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mongodb::bson::{doc, Bson, Document};
use mongodb::options::{ClientOptions, Credential, ServerAddress, ServerMonitoringMode, Tls, TlsOptions};
use mongodb::Client;
use sluice_core::{
    BatchSink, BrowseBatch, BrowseRequest, BrowseStreamEnd, CancelSlot, CancelSupport, Capabilities, CatalogSupport,
    Driver, DriverError, EditPlan, EditRequest, EditorSession, ErrorKind, ExecBudget, ExecutionResult, ExportBatch,
    ExportEnd, NavNode, NodeId, NonPoisonMutex, PoolStatsSnapshot, RawPayloadKind, RawRequest, ReadOnlyGuarantee,
    Redactor, ResetSupport, Secret, SessionAffinity, SqlLiterals, TlsMode, TransactionSupport, WireValue,
};

use crate::error_map::map_error;

/// T-046: the server default for `transactionLifetimeLimitSeconds`, declared
/// when the login may not read the live value.
pub const DEFAULT_TRANSACTION_LIFETIME_SECONDS: u32 = 60;

/// How long connecting waits for a reachable server before failing.
pub const SERVER_SELECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// D-028: idle pooled connections are closed after a few minutes.
pub const MAX_IDLE_TIME: Duration = Duration::from_secs(5 * 60);

/// Everything needed to open a client. The password arrives resolved from the
/// keychain (D-007), wrapped in `Secret` (T-022), so `Debug` is safe.
#[derive(Clone, Debug)]
pub struct ConnectSpec {
    pub host: String,
    pub port: u16,
    /// Empty: connect unauthenticated, as Mongo's own clients do (T-075).
    pub user: String,
    pub password: Option<Secret<String>>,
    /// The profile's database: where the tree opens and, as in a connection
    /// string's path, the database the user authenticates against.
    pub database: String,
    pub tls: TlsMode,
    pub application_name: String,
}

impl Default for ConnectSpec {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 27017,
            user: String::new(),
            password: None,
            database: "admin".into(),
            tls: TlsMode::default(),
            application_name: "sluice".into(),
        }
    }
}

/// The client options for a spec, with the pool sized from the budget.
///
/// - **One host, connected directly** (`directConnection`, what `mongosh`
///   does for a single-host URL): the profile names one server, and a replica
///   set member advertising an internal hostname must not send discovery to
///   an address this machine cannot reach.
/// - **TLS** (D-007 / T-021): `Verify` and `AcceptInvalidCerts` both
///   *require* TLS; `Disabled` turns it off. Never "prefer".
/// - **Credentials**: SCRAM against the profile's database, as a connection
///   string's path means; the mechanism is negotiated with the server.
pub fn client_options(spec: &ConnectSpec, budget: usize) -> ClientOptions {
    let mut o = ClientOptions::default();
    o.hosts = vec![ServerAddress::Tcp { host: spec.host.clone(), port: Some(spec.port) }];
    o.direct_connection = Some(true);
    o.app_name = Some(spec.application_name.clone());
    o.max_pool_size = Some(u32::try_from(budget.max(1)).unwrap_or(u32::MAX));
    o.min_pool_size = Some(0);
    o.max_idle_time = Some(MAX_IDLE_TIME);
    o.server_selection_timeout = Some(SERVER_SELECTION_TIMEOUT);
    o.server_monitoring_mode = Some(ServerMonitoringMode::Poll);
    o.tls = Some(match spec.tls {
        TlsMode::Verify => Tls::Enabled(TlsOptions::default()),
        TlsMode::AcceptInvalidCerts => {
            let mut t = TlsOptions::default();
            t.allow_invalid_certificates = Some(true);
            Tls::Enabled(t)
        }
        TlsMode::Disabled => Tls::Disabled,
    });
    if !spec.user.is_empty() {
        let mut c = Credential::default();
        c.username = Some(spec.user.clone());
        // The one place the plaintext is needed: handing it to the driver,
        // whose `Credential` Debug prints only `REDACTED`.
        c.password = spec.password.as_ref().map(|p| p.expose().to_string());
        c.source = Some(if spec.database.is_empty() { "admin".into() } else { spec.database.clone() });
        o.credential = Some(c);
    }
    o
}

/// What kind of deployment the profile reached — the thing that decides
/// whether transactions exist at all (T-046).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum Deployment {
    #[default]
    Standalone,
    ReplicaSet { name: String },
    Sharded,
}

/// The server, as probed at connect.
#[derive(Clone, Debug, Default)]
pub struct ServerInfo {
    /// `buildInfo.version`, e.g. `8.0.4`.
    pub version: Option<String>,
    pub deployment: Deployment,
    /// The database the profile opened.
    pub database: String,
    /// The live `transactionLifetimeLimitSeconds`, if the login may read it.
    pub transaction_lifetime_seconds: Option<u32>,
}

impl ServerInfo {
    pub fn flavour(&self) -> String {
        match &self.deployment {
            Deployment::Standalone => "MongoDB (standalone)".into(),
            Deployment::ReplicaSet { name } => format!("MongoDB (replica set {name})"),
            Deployment::Sharded => "MongoDB (sharded cluster)".into(),
        }
    }

    /// T-046: interactive, application-bounded transactions on a replica set
    /// or sharded cluster, killed by the server past their lifetime limit;
    /// none at all on a standalone server.
    pub fn transactions(&self) -> Option<TransactionSupport> {
        match self.deployment {
            Deployment::Standalone => None,
            Deployment::ReplicaSet { .. } | Deployment::Sharded => Some(TransactionSupport {
                interactive: true,
                max_idle_seconds: Some(self.transaction_lifetime_seconds.unwrap_or(DEFAULT_TRANSACTION_LIFETIME_SECONDS)),
            }),
        }
    }
}

type NavCache = Arc<NonPoisonMutex<HashMap<Option<NodeId>, Vec<NavNode>>>>;

#[derive(Clone)]
pub struct MongoDriver {
    client: Client,
    budget_size: usize,
    server: ServerInfo,
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
    DriverError::new(ErrorKind::Unknown, format!("{what} is not available on MongoDB in this build of Sluice yet"))
}

fn as_i64(b: Option<&Bson>) -> Option<i64> {
    match b? {
        Bson::Int32(v) => Some(i64::from(*v)),
        Bson::Int64(v) => Some(*v),
        Bson::Double(v) if v.fract() == 0.0 => Some(*v as i64),
        _ => None,
    }
}

impl MongoDriver {
    /// Connect and probe, with the profile's connection budget as the pool's
    /// ceiling (T-042 / D-028). Every error leaving here is redacted (T-024).
    pub async fn connect(spec: ConnectSpec, budget: usize) -> Result<Arc<Self>, DriverError> {
        let redactor = Redactor::new(spec.password.clone());
        Self::open(&spec, budget).await.map(Arc::new).map_err(|e| redactor.error(e))
    }

    async fn open(spec: &ConnectSpec, budget: usize) -> Result<Self, DriverError> {
        let client = Client::with_options(client_options(spec, budget)).map_err(|e| map_error(&e))?;
        let admin = client.database("admin");

        // Prove reachability and credentials now rather than on first expand.
        admin.run_command(doc! { "ping": 1 }).await.map_err(|e| map_error(&e))?;

        let hello = admin.run_command(doc! { "hello": 1 }).await.map_err(|e| map_error(&e))?;
        let deployment = deployment_of(&hello);
        let version = admin
            .run_command(doc! { "buildInfo": 1 })
            .await
            .ok()
            .and_then(|d| d.get_str("version").ok().map(str::to_string));
        let transaction_lifetime_seconds = admin
            .run_command(doc! { "getParameter": 1, "transactionLifetimeLimitSeconds": 1 })
            .await
            .ok()
            .and_then(|d| as_i64(d.get("transactionLifetimeLimitSeconds")))
            .and_then(|v| u32::try_from(v).ok());
        // `serverStatus` needs clusterMonitor; without it the ceiling is unknown.
        let max_connections = admin.run_command(doc! { "serverStatus": 1 }).await.ok().and_then(|d| {
            let c = d.get_document("connections").ok()?;
            Some(as_i64(c.get("current"))? + as_i64(c.get("available"))?)
        });

        let catalog = navigator::probe(&client, &spec.database).await;
        Ok(Self {
            client,
            budget_size: budget.max(1),
            server: ServerInfo { version, deployment, database: spec.database.clone(), transaction_lifetime_seconds },
            catalog,
            max_connections,
            redactor: Redactor::new(spec.password.clone()),
            nav_cache: Arc::default(),
            cancel: None,
            exec_budget: ExecBudget::default(),
        })
    }

    pub fn server(&self) -> &ServerInfo {
        &self.server
    }

    /// The underlying client, for the work that follows this crate (T-088,
    /// T-089) and for live tests.
    pub fn client(&self) -> &Client {
        &self.client
    }
}

/// `hello`: `setName` on a replica set member, `msg: "isdbgrid"` on a mongos.
pub fn deployment_of(hello: &Document) -> Deployment {
    if hello.get_str("msg").ok() == Some("isdbgrid") {
        Deployment::Sharded
    } else if let Ok(name) = hello.get_str("setName") {
        Deployment::ReplicaSet { name: name.to_string() }
    } else {
        Deployment::Standalone
    }
}

#[async_trait]
impl Driver for MongoDriver {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            engine: "mongodb".into(),
            server_version: self.server.version.clone(),
            flavour: Some(self.server.flavour()),
            // T-046: a logical session, not a socket, is what can be pinned.
            affinity: SessionAffinity::Session,
            reset: ResetSupport::NotApplicable,
            cancel: CancelSupport::None,
            transactions: self.server.transactions(),
            // T-073 F-5: one command document, never a script.
            raw_payload: RawPayloadKind::CommandDocument,
            node_kinds: navigator::node_kinds(),
            // Filter compilation arrives with browse (T-088).
            filter_ops: Vec::new(),
            // T-073 F-1 / D-018: columns are derived per page.
            fixed_schema: false,
            supports_script: false,
            // Collation is per collection or per operation; the server's
            // default comparison is binary ("simple"), with no name to show.
            string_collation: None,
            read_only: ReadOnlyGuarantee::Unknown {
                reason: "MongoDB write permissions are not probed in this build yet".into(),
            },
            catalog: self.catalog.clone(),
        }
    }

    async fn navigator_children(&self, parent: Option<NodeId>) -> Result<Vec<NavNode>, DriverError> {
        if let Some(hit) = self.nav_cache.lock().get(&parent) {
            return Ok(hit.clone());
        }
        let key = parent.clone();
        let nodes = navigator::children(&self.client, parent, &self.server.database, &self.catalog)
            .await
            .map_err(|e| self.redactor.error(e))?;
        // A notice is not catalog content: never cached, so the next expand
        // tries again (D-016).
        if !nodes.iter().any(navigator::is_notice) {
            self.nav_cache.lock().insert(key, nodes.clone());
        }
        Ok(nodes)
    }

    async fn browse(&self, _request: BrowseRequest) -> Result<ExecutionResult, DriverError> {
        Err(not_yet("Browsing a collection"))
    }

    async fn preview_edits(&self, _edits: Vec<EditRequest>) -> Result<EditPlan, DriverError> {
        Err(not_yet("Editing documents"))
    }

    async fn apply_edits(&self, _edits: Vec<EditRequest>, _digest: Option<String>) -> Result<ExecutionResult, DriverError> {
        Err(not_yet("Editing documents"))
    }

    async fn raw(&self, _request: RawRequest) -> Result<ExecutionResult, DriverError> {
        Err(not_yet("Running commands"))
    }

    async fn browse_batches(
        &self,
        _request: BrowseRequest,
        _batch_rows: usize,
        _emit: BatchSink<BrowseBatch>,
    ) -> Result<BrowseStreamEnd, DriverError> {
        Err(not_yet("Browsing a collection"))
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
        let mut copy = (*self).clone();
        copy.cancel = Some(slot);
        Arc::new(copy)
    }

    fn with_budget(self: Arc<Self>, budget: ExecBudget) -> Arc<dyn Driver> {
        let mut copy = (*self).clone();
        copy.exec_budget = budget;
        Arc::new(copy)
    }

    /// The pinned editor session is a socket pin (`affinity: connection`);
    /// Mongo's logical-session tab arrives with raw commands (T-089).
    async fn open_pinned(&self) -> Result<Box<dyn EditorSession>, DriverError> {
        Err(not_yet("An editor session"))
    }

    async fn run_pinned(&self, _session: &mut dyn EditorSession, _script: String) -> Result<ExecutionResult, DriverError> {
        Err(not_yet("Running commands"))
    }

    fn refresh_catalog(&self) {
        self.nav_cache.lock().clear();
    }

    /// The crate owns its pool and reports no per-lease events (R-011), so
    /// there are no releases, resets or reopens of ours to count.
    fn pool_stats(&self) -> PoolStatsSnapshot {
        PoolStatsSnapshot::default()
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
    fn the_pool_is_sized_from_the_budget_and_never_zero() {
        let o = client_options(&ConnectSpec::default(), 5);
        assert_eq!(o.max_pool_size, Some(5));
        assert_eq!(o.min_pool_size, Some(0));
        assert_eq!(client_options(&ConnectSpec::default(), 0).max_pool_size, Some(1));
        assert_eq!(o.direct_connection, Some(true));
    }

    #[test]
    fn tls_modes_never_fall_back_to_plaintext() {
        let tls = |mode| client_options(&ConnectSpec { tls: mode, ..Default::default() }, 1).tls;
        match tls(TlsMode::Verify) {
            Some(Tls::Enabled(t)) => assert_ne!(t.allow_invalid_certificates, Some(true)),
            other => panic!("{other:?}"),
        }
        match tls(TlsMode::AcceptInvalidCerts) {
            Some(Tls::Enabled(t)) => assert_eq!(t.allow_invalid_certificates, Some(true)),
            other => panic!("{other:?}"),
        }
        assert!(matches!(tls(TlsMode::Disabled), Some(Tls::Disabled)));
    }

    #[test]
    fn credentials_authenticate_against_the_profile_database_and_never_print() {
        let s = ConnectSpec {
            user: "app".into(),
            password: Some(Secret::new("hunter2".into())),
            database: "shop".into(),
            ..Default::default()
        };
        let o = client_options(&s, 1);
        let c = o.credential.as_ref().expect("credential");
        assert_eq!(c.source.as_deref(), Some("shop"));
        assert!(!format!("{s:?}").contains("hunter2"));
        assert!(!format!("{o:?}").contains("hunter2"));
        // No user: unauthenticated, as Mongo's own clients do.
        assert!(client_options(&ConnectSpec::default(), 1).credential.is_none());
    }

    #[test]
    fn transactions_follow_the_deployment() {
        let info = |deployment| ServerInfo { deployment, ..Default::default() };
        assert_eq!(info(Deployment::Standalone).transactions(), None, "T-046: a standalone mongod cannot transact");
        let rs = info(Deployment::ReplicaSet { name: "rs0".into() }).transactions().unwrap();
        assert!(rs.interactive);
        assert_eq!(rs.max_idle_seconds, Some(DEFAULT_TRANSACTION_LIFETIME_SECONDS));
        let live = ServerInfo { transaction_lifetime_seconds: Some(30), ..info(Deployment::Sharded) };
        assert_eq!(live.transactions().unwrap().max_idle_seconds, Some(30));
    }

    #[test]
    fn deployments_are_read_from_hello() {
        assert_eq!(deployment_of(&doc! { "isWritablePrimary": true }), Deployment::Standalone);
        assert_eq!(deployment_of(&doc! { "setName": "rs0" }), Deployment::ReplicaSet { name: "rs0".into() });
        assert_eq!(deployment_of(&doc! { "msg": "isdbgrid" }), Deployment::Sharded);
    }
}
