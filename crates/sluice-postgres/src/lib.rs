//! Postgres driver (T-007 / D-002, D-003).
//!
//! D-002: `tokio-postgres` directly, never sqlx — a database *client* has the
//! inverse requirements of an ORM, and needs the raw row plus type OID that
//! D-006's tagged encoding is built on.

pub mod browse;
pub mod catalog_probe;
pub mod edit;
pub mod encode;
pub mod error_map;
pub mod lease;
pub mod navigator;
pub mod pinned;
pub mod read_only;
pub mod tls;
pub mod version;

use std::sync::Arc;

use async_trait::async_trait;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use sluice_core::{
    Attribution, BrowseRequest, CancelSupport, Capabilities, Driver, DriverError, EditGate,
    EditPlan, EditRequest,
    ErrorKind, ExecutionResult, FilterOpDecl, NavNode, NodeId, NodeKind, Outcome, OutcomeBody,
    RawPayloadKind, RawRequest, ResetSupport, ScriptDialect, Secret, SessionAffinity,
    TransactionSupport, TxContext, TxDisposition, TxMode, TypeClass, WireValue,
};
use tokio_postgres::types::ToSql;
use tokio_postgres::SimpleQueryMessage;

use crate::encode::DEFAULT_VALUE_CEILING;
use crate::lease::{Lease, PoolStats, PoolStatsSnapshot};
pub use crate::lease::CancelSlot;
use crate::tls::TlsMode;

/// Everything needed to open a connection.
///
/// D-007: the core resolves a profile id to credentials at connect time. The
/// password arrives here already resolved from the keychain and never travels
/// back outward.
///
/// T-022/D-020: `Debug` is now **derived**. It used to be hand-written
/// specifically so it could not leak the password, which worked but was exactly
/// the "remembered at every call site" pattern D-020 rejects — the next struct
/// to hold a credential would have had to remember the same trick. With
/// `Secret<String>` the redaction rides on the type, so deriving is safe here
/// and everywhere downstream.
#[derive(Clone, Debug)]
pub struct ConnectSpec {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<Secret<String>>,
    pub dbname: String,
    pub tls: TlsMode,
    pub application_name: String,
    /// T-051 / D-010: the profile's statement timeout, enforced **by the
    /// server** for every statement on every pooled connection. Sent as a
    /// startup option, so `DISCARD ALL` (which resets to startup values)
    /// cannot clear it. `None` leaves the server's own setting.
    pub statement_timeout: Option<std::time::Duration>,
}

impl Default for ConnectSpec {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 5432,
            user: "postgres".into(),
            password: None,
            dbname: "postgres".into(),
            tls: TlsMode::default(),
            application_name: "sluice".into(),
            statement_timeout: None,
        }
    }
}

/// One item of a streamed browse (T-049).
#[derive(Debug, Clone)]
pub enum BrowseBatch {
    /// Always first: the page's column descriptors (D-006), and whether the
    /// relation's rows may be edited (T-056).
    Columns(Vec<sluice_core::ColumnDesc>, EditGate),
    Rows(Vec<Vec<WireValue>>),
}

/// What the catalog says about one relation (T-047, T-056).
struct RelationShape {
    columns: Vec<(String, tokio_postgres::types::Type)>,
    keys: browse::KeyInfo,
    gate: EditGate,
    is_table: bool,
}

/// How a streamed browse ended.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowseStreamEnd {
    pub rows: usize,
    /// The request's `limit` was reached (more rows may exist).
    pub truncated: bool,
    /// The consumer stopped it (cancel, timeout, or a closed channel).
    pub stopped: bool,
    /// T-047: the strategy used and the next page's position (unstopped only).
    pub page: Option<sluice_core::PageInfo>,
}

/// D-028's per-profile connection budget, "~5, configurable" (T-042). One
/// ceiling for pooled and pinned connections together.
pub const DEFAULT_CONNECTION_BUDGET: usize = 5;

/// How long a request waits for a connection inside the budget before it is
/// refused (T-042). Long enough to queue behind a running query; short enough
/// that an exhausted budget is reported rather than looking like a hang.
pub const BUDGET_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct PgDriver {
    pool: Pool,
    ceiling: i64,
    /// T-054 / D-016: flavour and versions parsed from `version()` at connect;
    /// catalog queries and features gate on it.
    server: Option<version::ServerInfo>,
    tls: TlsMode,
    /// D-028: how a released connection is made clean. Postgres has
    /// `DISCARD ALL`, so `Native`; overridable for tests of the other paths.
    reset: ResetSupport,
    stats: Arc<PoolStats>,
    /// T-024 / R-023: this connection's password, kept **only** to redact it
    /// from every error the driver returns, in every encoding.
    redactor: sluice_core::Redactor,
    /// T-015: the database's default collation and whether it is
    /// deterministic, read at connect. Deterministic means `=` and `LIKE`
    /// distinguish case — the semantics the filter bar reports.
    collation: Option<(String, bool)>,
    /// T-061 / D-027: whether the server refuses writes from this role,
    /// probed at connect.
    read_only: sluice_core::ReadOnlyGuarantee,
    /// T-060 / R-035: which types have a usable equality on *this* server,
    /// probed from the catalog at connect.
    comparability: encode::Comparability,
    /// T-055 / R-028: which catalog areas probed successfully at connect.
    catalog: sluice_core::CatalogSupport,
    /// T-053 / D-016: one level of the navigator tree per parent, filled on
    /// first expand and kept until DDL the app ran, or an explicit refresh.
    /// Shared across `with_*` copies, like the pool.
    nav_cache: Arc<std::sync::Mutex<std::collections::HashMap<Option<NodeId>, Vec<NavNode>>>>,
    /// T-051: set on a per-call copy (`with_cancel`), never on the shared driver.
    cancel: Option<CancelSlot>,
    /// T-050: what one raw execution may retain for display.
    budget: sluice_core::ExecBudget,
}

impl PgDriver {
    pub async fn connect(spec: ConnectSpec) -> Result<Arc<Self>, DriverError> {
        Self::connect_with_pool_size(spec, DEFAULT_CONNECTION_BUDGET).await
    }

    /// T-042 / D-028: the profile's connection budget — the pool's ceiling,
    /// which pooled *and* pinned connections share, since a pinned editor
    /// session is a lease held from this same pool.
    pub fn connection_budget(&self) -> usize {
        self.pool.status().max_size
    }

    /// As [`connect`](Self::connect), with an explicit pool ceiling. Tests use
    /// a ceiling of one so consecutive leases provably share a connection.
    pub async fn connect_with_pool_size(
        spec: ConnectSpec,
        max_size: usize,
    ) -> Result<Arc<Self>, DriverError> {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&spec.host)
            .port(spec.port)
            .user(&spec.user)
            .dbname(&spec.dbname)
            .application_name(&spec.application_name);
        if let Some(t) = spec.statement_timeout {
            cfg.options(&format!("-c statement_timeout={}", t.as_millis().max(1)));
        }
        if let Some(pw) = &spec.password {
            // The one place the plaintext is legitimately needed: handing it to
            // the wire protocol. `tokio_postgres::Config` keeps it internally
            // and its own `Debug` masks it.
            cfg.password(pw.expose());
        }

        // D-007 / T-021: a TLS mode must be *required*, not preferred.
        // tokio-postgres defaults to `Prefer`, which silently falls back to
        // plaintext when the server offers no TLS — so `Verify` used to connect
        // unencrypted to a TLS-less server, and an on-path attacker could strip
        // TLS from any connection. Found by T-024's redaction corpus.
        cfg.ssl_mode(match spec.tls {
            TlsMode::Disabled => tokio_postgres::config::SslMode::Disable,
            TlsMode::Verify | TlsMode::AcceptInvalidCerts => tokio_postgres::config::SslMode::Require,
        });

        let mgr_cfg = ManagerConfig { recycling_method: RecyclingMethod::Fast };

        // `deadpool_postgres::Manager` erases the TLS connector, so both arms
        // produce the same concrete type.
        let manager = if spec.tls == TlsMode::Disabled {
            Manager::from_config(cfg, tokio_postgres::NoTls, mgr_cfg)
        } else {
            let tls_cfg = tls::client_config(spec.tls)
                .map_err(|e| DriverError::new(ErrorKind::ConnectionFailed, e))?;
            let connector = tokio_postgres_rustls::MakeRustlsConnect::new(tls_cfg);
            Manager::from_config(cfg, connector, mgr_cfg)
        };

        // T-042: a request that cannot get a connection within the budget
        // fails with an explanation instead of waiting forever.
        let pool = Pool::builder(manager)
            .max_size(max_size.max(1))
            .runtime(deadpool_postgres::Runtime::Tokio1)
            .wait_timeout(Some(BUDGET_WAIT))
            .build()
            .map_err(|e| DriverError::new(ErrorKind::ConnectionFailed, e.to_string()))?;

        let driver = Self {
            pool,
            ceiling: DEFAULT_VALUE_CEILING,
            server: None,
            tls: spec.tls,
            reset: ResetSupport::Native,
            stats: Arc::new(PoolStats::default()),
            redactor: sluice_core::Redactor::new(spec.password.clone()),
            collation: None,
            read_only: sluice_core::ReadOnlyGuarantee::default(),
            comparability: encode::Comparability::fallback(),
            catalog: sluice_core::CatalogSupport::default(),
            nav_cache: Arc::default(),
            cancel: None,
            budget: sluice_core::ExecBudget::default(),
        };

        // Prove the credentials work now rather than on first browse, and pick
        // up the version banner while we are here (T-054 refines this).
        // T-054: the banner first — what answers decides which catalog
        // queries are safe to run at all.
        let banner = driver
            .with_lease(false, async |client: &tokio_postgres::Client| {
                let row = client
                    .query_one("select version()", &[])
                    .await
                    .map_err(|e| error_map::map_error(&e, None))?;
                Ok(row.get::<_, String>(0))
            })
            .await?;
        let server = version::parse(&banner);

        // T-015: the default collation and its determinism — gated, because
        // `collisdeterministic` only exists on PostgreSQL 12+ catalogs.
        let collation = if server.has_collation_determinism() {
            let c = driver
                .with_lease(false, async |client: &tokio_postgres::Client| {
                    let row = client
                        .query_one(
                            "select (select datcollate from pg_database where datname = current_database()), \
                                    coalesce((select collisdeterministic from pg_collation where collname = 'default'), true)",
                            &[],
                        )
                        .await
                        .map_err(|e| error_map::map_error(&e, None))?;
                    Ok((row.get::<_, String>(0), row.get::<_, bool>(1)))
                })
                .await?;
            Some(c)
        } else {
            None
        };

        // D-028: no `DISCARD ALL` means no native reset — reopen instead.
        let reset = if server.has_discard_all() { ResetSupport::Native } else { ResetSupport::ReopenOnly };

        // T-061: what the database enforces about writes. Never fails connect.
        let read_only = driver
            .with_lease(false, async |client: &tokio_postgres::Client| Ok(read_only::probe(client).await))
            .await
            .unwrap_or_default();

        // T-060: types with a usable `=` here. A failed probe falls back to
        // the static allowlist, which under-protects but never over-claims.
        let comparability = driver
            .with_lease(false, async |client: &tokio_postgres::Client| {
                Ok(match client.query(encode::EQUALITY_TYPES_SQL, &[]).await {
                    Ok(rows) => encode::Comparability::from_probe(rows.iter().map(|r| r.get::<_, i64>(0) as u32)),
                    Err(_) => encode::Comparability::fallback(),
                })
            })
            .await
            .unwrap_or_default();

        // T-055 / R-028: what the catalog demonstrably supports, so behaviour
        // follows what this server does rather than what its banner claims.
        let flavour_verified = !matches!(server.flavour, version::Flavour::Unknown(_));
        let catalog = driver
            .with_lease(false, async |client: &tokio_postgres::Client| Ok(catalog_probe::probe(client, flavour_verified).await))
            .await
            .unwrap_or_default();

        Ok(Arc::new(Self { server: Some(server), collation, reset, read_only, comparability, catalog, ..driver }))
    }

    pub fn with_ceiling(self: Arc<Self>, ceiling: i64) -> Arc<Self> {
        Arc::new(Self {
            pool: self.pool.clone(),
            ceiling,
            server: self.server.clone(),
            tls: self.tls,
            reset: self.reset,
            stats: self.stats.clone(),
            redactor: self.redactor.clone(),
            collation: self.collation.clone(),
            read_only: self.read_only.clone(),
            comparability: self.comparability.clone(),
            catalog: self.catalog.clone(),
            nav_cache: self.nav_cache.clone(),
            cancel: self.cancel.clone(),
            budget: self.budget,
        })
    }

    /// Override the probed catalog support. For exercising T-055's degraded
    /// navigation against a real server, which genuinely supports everything.
    pub fn with_catalog(self: Arc<Self>, catalog: sluice_core::CatalogSupport) -> Arc<Self> {
        self.nav_cache.lock().unwrap_or_else(|p| p.into_inner()).clear();
        Arc::new(Self { catalog, ..self.clone_inner() })
    }

    /// Override the declared reset path. For exercising D-028's `reopen` and
    /// `none` behaviour against a real server; production code never needs it,
    /// since Postgres genuinely has `DISCARD ALL`.
    pub fn with_reset(self: Arc<Self>, reset: ResetSupport) -> Arc<Self> {
        Arc::new(Self {
            pool: self.pool.clone(),
            ceiling: self.ceiling,
            server: self.server.clone(),
            tls: self.tls,
            reset,
            stats: self.stats.clone(),
            redactor: self.redactor.clone(),
            collation: self.collation.clone(),
            read_only: self.read_only.clone(),
            comparability: self.comparability.clone(),
            catalog: self.catalog.clone(),
            nav_cache: self.nav_cache.clone(),
            cancel: self.cancel.clone(),
            budget: self.budget,
        })
    }

    /// Fetch a value that was replaced by an [`WireValue::Oversized`]
    /// placeholder (T-018).
    ///
    /// The ceiling is not applied here: the caller explicitly asked for this one
    /// value, which is the whole point of the placeholder — the cost is paid on
    /// demand for one cell rather than implicitly for every row of a browse.
    pub async fn fetch_value(&self, handle: &str) -> Result<WireValue, DriverError> {
        let (path, ctid, column) = decode_handle(handle).ok_or_else(|| {
            DriverError::new(ErrorKind::NotFound, "malformed value handle")
        })?;

        let relation = NodeId { kind: "table".into(), path };
        let rel = browse::qualified(&relation)?;
        self.with_lease(false, async |client: &tokio_postgres::Client| {
    
            let probe = client
                .prepare(&format!("SELECT * FROM {rel} LIMIT 0"))
                .await
                .map_err(|e| error_map::map_error(&e, None))?;
            let ty = probe
                .columns()
                .iter()
                .find(|c| c.name() == column)
                .map(|c| c.type_().clone())
                .ok_or_else(|| {
                    DriverError::new(ErrorKind::NotFound, format!("no column `{column}`"))
                })?;
    
            // i64::MAX as the ceiling means `value_expr` emits no CASE guard.
            let sql = format!(
                "SELECT {} FROM {} WHERE ctid = ($1)::text::tid",
                encode::value_expr(&column, &ty, i64::MAX),
                rel
            );
            let rows = client
                .query(&sql, &[&Some(ctid)])
                .await
                .map_err(|e| error_map::map_error(&e, Some(&sql)))?;
    
            let row = rows.first().ok_or_else(|| {
                // `ctid` is a physical pointer: an UPDATE or VACUUM FULL moves the
                // row and invalidates the handle. Reporting that plainly is much
                // better than silently returning a different row's data.
                DriverError::new(
                    ErrorKind::NotFound,
                    "that row has changed since the value was listed; re-run the query",
                )
            })?;
    
            encode::decode_value(row, 0, None, &ty, i64::MAX, None)
                .map_err(|e| error_map::map_error(&e, Some(&sql)))
        })
        .await
    }

    /// Stream an oversized value straight to a file (T-018 / R-008).
    ///
    /// Genuinely chunked: the value is pulled with `substr` in slices and
    /// appended as it arrives, so neither the Rust process nor the webview ever
    /// holds the whole thing. That is what makes "save to file" safe for the
    /// 50MB `bytea` R-008 is about.
    ///
    /// Server-side on purpose. Routing the bytes through the webview to use a
    /// browser download would put them back in the process D-007 is trying to
    /// keep small, and would need the `fs`/`dialog` plugins that T-021
    /// deliberately leaves out of the capability set.
    pub async fn stream_value_to_file(
        &self,
        handle: &str,
        dest: &std::path::Path,
    ) -> Result<u64, DriverError> {
        use std::io::Write as _;

        let (path, ctid, column) = decode_handle(handle).ok_or_else(|| {
            DriverError::new(ErrorKind::NotFound, "malformed value handle")
        })?;
        let relation = NodeId { kind: "table".into(), path };
        let rel = browse::qualified(&relation)?;
        self.with_lease(false, async |client: &tokio_postgres::Client| {
    
            let probe = client
                .prepare(&format!("SELECT * FROM {rel} LIMIT 0"))
                .await
                .map_err(|e| error_map::map_error(&e, None))?;
            let ty = probe
                .columns()
                .iter()
                .find(|c| c.name() == column)
                .map(|c| c.type_().clone())
                .ok_or_else(|| DriverError::new(ErrorKind::NotFound, format!("no column `{column}`")))?;
    
            let ident = encode::quote_ident(&column);
            let is_binary = ty == tokio_postgres::types::Type::BYTEA;
            // Non-binary types are rendered to text first so one code path covers
            // text, json and the exotic types alike.
            let source = if is_binary { ident.clone() } else { format!("({ident})::text") };
    
            let mut file = std::fs::File::create(dest)
                .map_err(|e| DriverError::new(ErrorKind::PermissionDenied, e.to_string()))?;
    
            const CHUNK: i64 = 1 << 20; // 1 MiB
            let mut offset: i64 = 1; // substr is 1-based
            let mut written: u64 = 0;
    
            loop {
                let sql = format!(
                    "SELECT substr({source}, {offset}, {CHUNK}) FROM {rel} WHERE ctid = ($1)::text::tid"
                );
                let rows = client
                    .query(&sql, &[&Some(ctid.clone())])
                    .await
                    .map_err(|e| error_map::map_error(&e, Some(&sql)))?;
    
                let Some(row) = rows.first() else {
                    return Err(DriverError::new(
                        ErrorKind::NotFound,
                        "that row has changed since the value was listed; re-run the query",
                    ));
                };
    
                let chunk_len = if is_binary {
                    let bytes: Option<Vec<u8>> = row
                        .try_get(0)
                        .map_err(|e| error_map::map_error(&e, Some(&sql)))?;
                    match bytes {
                        Some(b) if !b.is_empty() => {
                            file.write_all(&b)
                                .map_err(|e| DriverError::new(ErrorKind::Unknown, e.to_string()))?;
                            b.len()
                        }
                        _ => 0,
                    }
                } else {
                    let text: Option<String> = row
                        .try_get(0)
                        .map_err(|e| error_map::map_error(&e, Some(&sql)))?;
                    match text {
                        Some(t) if !t.is_empty() => {
                            file.write_all(t.as_bytes())
                                .map_err(|e| DriverError::new(ErrorKind::Unknown, e.to_string()))?;
                            // substr on text counts CHARACTERS, so the next offset
                            // advances by characters, not by the bytes written.
                            offset += t.chars().count() as i64;
                            written += t.len() as u64;
                            continue;
                        }
                        _ => 0,
                    }
                };
    
                if chunk_len == 0 {
                    break;
                }
                written += chunk_len as u64;
                offset += chunk_len as i64;
            }
    
            file.flush()
                .map_err(|e| DriverError::new(ErrorKind::Unknown, e.to_string()))?;
            Ok(written)
        })
        .await
    }

    /// The relation's real columns and types, its paging keys, and whether it
    /// may be edited (T-047, T-056).
    ///
    /// Learning the real column types from the server is what makes R-014 a
    /// non-issue: types with no Rust mapping arrive as ordinary `Type` values
    /// and are rendered server-side.
    async fn relation_shape(
        &self,
        client: &tokio_postgres::Client,
        relation: &NodeId,
    ) -> Result<RelationShape, DriverError> {
        let rel = browse::qualified(relation)?;
        let probe = client
            .prepare(&format!("SELECT * FROM {rel} LIMIT 0"))
            .await
            .map_err(|e| error_map::map_error(&e, None))?;
        let columns: Vec<(String, tokio_postgres::types::Type)> = probe
            .columns()
            .iter()
            .map(|c| (c.name().to_string(), c.type_().clone()))
            .collect();
        // `ctid` exists on physical tables only; T-018 uses it to build
        // re-fetch handles for oversized values.
        let is_table = relation.kind == "table";
        // T-047: what the catalog says about paging this relation. The
        // primary key in key order and the NOT NULL columns; a view has none.
        // T-056: and every unique key, for the edit gate.
        let (keys, candidates) = if is_table {
            const PK: &str = "
                SELECT a.attname
                FROM pg_index x
                JOIN pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = ANY(x.indkey)
                WHERE x.indrelid = to_regclass($1) AND x.indisprimary
                ORDER BY array_position(x.indkey::int2[], a.attnum)";
            const NOT_NULL: &str = "
                SELECT attname FROM pg_attribute
                WHERE attrelid = to_regclass($1) AND attnum > 0 AND NOT attisdropped AND attnotnull";
            let pk = client.query(PK, &[&rel]).await.map_err(|e| error_map::map_error(&e, Some(PK)))?;
            let nn = client.query(NOT_NULL, &[&rel]).await.map_err(|e| error_map::map_error(&e, Some(NOT_NULL)))?;
            let uk = client
                .query(edit::UNIQUE_KEYS_SQL, &[&rel])
                .await
                .map_err(|e| error_map::map_error(&e, Some(edit::UNIQUE_KEYS_SQL)))?;
            (
                browse::KeyInfo {
                    primary_key: pk.iter().map(|r| r.get(0)).collect(),
                    not_null: nn.iter().map(|r| r.get(0)).collect(),
                },
                uk.iter()
                    .map(|r| edit::KeyCandidate { primary: r.get(0), columns: r.get(1) })
                    .collect::<Vec<_>>(),
            )
        } else {
            (browse::KeyInfo::default(), Vec::new())
        };
        let gate = edit::edit_gate(&relation.kind, &candidates, &keys.not_null);
        Ok(RelationShape { columns, keys, gate, is_table })
    }

    /// Compile staged edits against the catalog as it stands now — the one
    /// build both preview and apply use (T-056, T-057).
    async fn compile_edits(
        &self,
        client: &tokio_postgres::Client,
        edits: &[EditRequest],
    ) -> Result<Vec<edit::CompiledEdit>, DriverError> {
        let mut shapes: Vec<(NodeId, RelationShape)> = Vec::new();
        let mut compiled = Vec::with_capacity(edits.len());
        for (i, e) in edits.iter().enumerate() {
            if !shapes.iter().any(|(r, _)| r == &e.relation) {
                let shape = self.relation_shape(client, &e.relation).await?;
                shapes.push((e.relation.clone(), shape));
            }
            let (_, shape) = shapes.iter().find(|(r, _)| r == &e.relation).expect("inserted above");
            compiled.push(edit::compile_update_with(i, e, &shape.columns, &shape.gate, &self.comparability)?);
        }
        Ok(compiled)
    }

    /// Prepare the relation's probe and compile the structured request.
    async fn compile_browse(
        &self,
        client: &tokio_postgres::Client,
        request: &BrowseRequest,
    ) -> Result<(browse::CompiledBrowse, EditGate), DriverError> {
        let shape = self.relation_shape(client, &request.relation).await?;
        let compiled = browse::compile_with(
            &request.relation,
            &shape.columns,
            request,
            self.ceiling,
            shape.is_table,
            &shape.keys,
            &self.comparability,
        )?;
        Ok((compiled, shape.gate))
    }

    fn decode_row(
        &self,
        compiled: &browse::CompiledBrowse,
        row: &tokio_postgres::Row,
        relation: &NodeId,
    ) -> Result<Vec<WireValue>, DriverError> {
        let ctid: Option<String> = compiled.ctid_idx.and_then(|i| row.try_get(i).ok());
        let mut cells = Vec::with_capacity(compiled.columns.len());
        for plan in &compiled.columns {
            let handle = ctid.as_ref().map(|c| make_handle(relation, c, &plan.name));
            let v = encode::decode_value(row, plan.value_idx, plan.len_idx, &plan.ty, self.ceiling, handle)
                .map_err(|e| error_map::map_error(&e, Some(&compiled.sql)))?;
            cells.push(v);
        }
        Ok(cells)
    }

    /// Structured browse, streamed in batches (T-049 / R-006, D-010).
    ///
    /// Rows are read from the server incrementally (`query_raw`) and handed to
    /// `emit` in batches of `batch_rows`, columns first. `emit` is where the
    /// caller's backpressure lives: it **awaits** credit before returning, so
    /// this loop — and the server cursor behind it — advances only as fast as
    /// the consumer retires batches. Returning `false` stops the stream: the
    /// query is cancelled **server-side** with the protocol cancel request (not
    /// merely abandoned, which would leave the server producing rows nobody
    /// reads), and the lease is released.
    ///
    /// An inherent method for now; lifting streaming into the `Driver` trait
    /// belongs with the second engine.
    ///
    /// `emit` returns an owned future (rather than being an async closure) so a
    /// stream can be spawned onto a multi-threaded runtime: async closures that
    /// borrow their captures currently fail rustc's higher-ranked `Send` check.
    pub async fn browse_batches<F>(
        &self,
        request: BrowseRequest,
        batch_rows: usize,
        mut emit: impl FnMut(BrowseBatch) -> F,
    ) -> Result<BrowseStreamEnd, DriverError>
    where
        F: std::future::Future<Output = bool>,
    {
        use futures_util::TryStreamExt as _;

        let batch_rows = batch_rows.max(1);
        self.with_lease(false, async |client: &tokio_postgres::Client| {
            let (compiled, gate) = self.compile_browse(client, &request).await?;
            if !emit(BrowseBatch::Columns(compiled.column_descs(), gate)).await {
                return Ok(BrowseStreamEnd { rows: 0, truncated: false, stopped: true, page: None });
            }

            let cancel = client.cancel_token();
            let stream = client
                .query_raw(&compiled.sql, compiled.params.iter())
                .await
                .map_err(|e| error_map::map_error(&e, Some(&compiled.sql)))?;
            let mut stream = std::pin::pin!(stream);

            let mut sent = 0usize;
            let mut last: Option<Vec<WireValue>> = None;
            let mut batch: Vec<Vec<WireValue>> = Vec::with_capacity(batch_rows);
            while let Some(row) = stream
                .try_next()
                .await
                .map_err(|e| error_map::map_error(&e, Some(&compiled.sql)))?
            {
                let decoded = self.decode_row(&compiled, &row, &request.relation)?;
                last = Some(decoded.clone());
                batch.push(decoded);
                if batch.len() == batch_rows {
                    let n = batch.len();
                    if !emit(BrowseBatch::Rows(std::mem::take(&mut batch))).await {
                        self.send_cancel(&cancel).await;
                        return Ok(BrowseStreamEnd { rows: sent, truncated: false, stopped: true, page: None });
                    }
                    // Counted only once accepted: a batch refused at cancel was
                    // never delivered.
                    sent += n;
                }
            }
            if !batch.is_empty() {
                let n = batch.len();
                if !emit(BrowseBatch::Rows(batch)).await {
                    return Ok(BrowseStreamEnd { rows: sent, truncated: false, stopped: true, page: None });
                }
                sent += n;
            }
            Ok(BrowseStreamEnd {
                rows: sent,
                truncated: sent as u32 >= request.limit,
                stopped: false,
                page: Some(compiled.paging.page_info(&compiled.columns, last.as_deref(), sent, request.limit)),
            })
        })
        .await
    }

    /// The protocol-level cancel request (a separate connection to the
    /// server). Best effort: if it fails, the query still ends when the
    /// abandoned stream is drained, so this only bounds wasted server work.
    async fn send_cancel(&self, token: &tokio_postgres::CancelToken) {
        let _ = self.send_cancel_checked(token).await;
    }

    async fn send_cancel_checked(&self, token: &tokio_postgres::CancelToken) -> bool {
        let r = if self.tls == TlsMode::Disabled {
            token.cancel_query(tokio_postgres::NoTls).await
        } else {
            match tls::client_config(self.tls) {
                Ok(cfg) => token.cancel_query(tokio_postgres_rustls::MakeRustlsConnect::new(cfg)).await,
                Err(_) => return false,
            }
        };
        r.is_ok()
    }

    /// Run `f` on a leased connection, then release it through the declared
    /// reset path (T-039 / D-028) — on success, error and early return alike.
    ///
    /// `dirty` says whether `f` can mutate session state (T-040 / R-036). Only
    /// raw passthrough can on this driver — it is the one path that carries
    /// `SET`, temp-table DDL, `PREPARE`, or a dangling `BEGIN`. Every
    /// structured path (browse, navigator, value fetch) issues parameterized
    /// SELECTs, and the named statements tokio-postgres prepares for them are
    /// closed when dropped, before release. So structured traffic returns its
    /// connection as-is, and under `ReopenOnly` never pays a reconnect — the
    /// cliff R-036 describes.
    async fn with_lease<T>(
        &self,
        dirty: bool,
        f: impl AsyncFnOnce(&tokio_postgres::Client) -> Result<T, DriverError>,
    ) -> Result<T, DriverError> {
        let out = async {
            let mut lease = Lease::acquire(&self.pool, self.reset, self.stats.clone()).await?;
            if dirty {
                lease.mark_dirty();
            }
            // T-051: register this connection's cancel token; a slot already
            // cancelled refuses to start, closing the cancel-before-start race.
            if let Some(slot) = &self.cancel {
                if !slot.arm(lease.client().cancel_token()) {
                    lease.release().await;
                    return Err(DriverError::new(ErrorKind::Cancelled, "cancelled before it started"));
                }
            }
            let out = f(lease.client()).await;
            if let Some(slot) = &self.cancel {
                slot.disarm();
                // A cancelled statement can leave an aborted transaction; the
                // reset path (ROLLBACK, DISCARD ALL) must run on release.
                if slot.is_cancelled() {
                    lease.mark_dirty();
                }
            }
            lease.release().await;
            out
        }
        .await;
        // T-024: every error leaving a lease — including a pool-acquire error,
        // which can carry a reconnect failure's text — is redacted here.
        out.map_err(|e| self.redactor.error(e))
    }

    fn redact_outcomes(&self, mut r: ExecutionResult) -> ExecutionResult {
        for o in r.outcomes.iter_mut() {
            if let OutcomeBody::Error { error } = &mut o.body {
                *error = self.redactor.error(error.clone());
            }
        }
        r
    }

    /// The redactor for this connection's credentials, for callers that hold
    /// errors the driver did not produce (a caught panic, say).
    pub fn redactor(&self) -> &sluice_core::Redactor {
        &self.redactor
    }

    /// T-050: a copy with a different per-execution result budget.
    pub fn with_budget(self: Arc<Self>, budget: sluice_core::ExecBudget) -> Arc<Self> {
        let mut copy = self.clone_inner();
        copy.budget = budget;
        Arc::new(copy)
    }

    /// T-051: a copy of this driver whose calls register with `slot`, so the
    /// query they run can be cancelled server-side via [`PgDriver::cancel`].
    pub fn with_cancel(self: Arc<Self>, slot: CancelSlot) -> Arc<Self> {
        let mut copy = self.clone_inner();
        copy.cancel = Some(slot);
        Arc::new(copy)
    }

    fn clone_inner(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            ceiling: self.ceiling,
            server: self.server.clone(),
            tls: self.tls,
            reset: self.reset,
            stats: self.stats.clone(),
            redactor: self.redactor.clone(),
            collation: self.collation.clone(),
            read_only: self.read_only.clone(),
            comparability: self.comparability.clone(),
            catalog: self.catalog.clone(),
            nav_cache: self.nav_cache.clone(),
            cancel: self.cancel.clone(),
            budget: self.budget,
        }
    }

    /// T-051 / D-010: cancel the query registered in `slot` with the protocol
    /// cancel request, which stops it **on the server**. Returns whether a
    /// request was sent; `false` means nothing was running (and nothing will
    /// start on that slot now) or the request could not be delivered.
    pub async fn cancel(&self, slot: &CancelSlot) -> bool {
        match slot.trip() {
            Some(token) => self.send_cancel_checked(&token).await,
            None => false,
        }
    }

    /// T-041 / D-028: pin a connection for an editor tab's lifetime. Only
    /// meaningful because this driver declares `affinity: connection`.
    pub async fn open_pinned(&self) -> Result<pinned::PinnedSession, DriverError> {
        let lease = Lease::acquire(&self.pool, self.reset, self.stats.clone())
            .await
            .map_err(|e| self.redactor.error(e))?;
        Ok(pinned::PinnedSession::new(lease))
    }

    /// T-041: run a raw script on a pinned session, with the same redaction
    /// and catalog invalidation as the pooled raw path. On a per-call copy
    /// made with `with_cancel` (T-051), the slot is armed with the pinned
    /// connection's token for the run, so Stop cancels it on the server.
    pub async fn run_pinned(
        &self,
        session: &mut pinned::PinnedSession,
        script: &str,
    ) -> Result<ExecutionResult, DriverError> {
        if let (Some(slot), Some(token)) = (&self.cancel, session.cancel_token()) {
            if !slot.arm(token) {
                return Err(DriverError::new(ErrorKind::Cancelled, "cancelled before it started"));
            }
        }
        let r = session.run(script, self.budget).await.map_err(|e| self.redactor.error(e));
        if let Some(slot) = &self.cancel {
            slot.disarm();
        }
        if navigator::script_may_change_catalog(script) {
            self.refresh_catalog();
        }
        Ok(self.redact_outcomes(r?))
    }

    /// T-053 / D-016: drop the navigator cache. External DDL is undetectable
    /// without polling, so refresh is the contract, not a fallback.
    pub fn refresh_catalog(&self) {
        self.nav_cache.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// Release counters for this driver's pool (T-040).
    pub fn pool_stats(&self) -> PoolStatsSnapshot {
        self.stats.snapshot()
    }
}

#[async_trait]
impl Driver for PgDriver {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            engine: "postgres".into(),
            // T-054: the product's own version and the flavour, parsed.
            server_version: self.server.as_ref().and_then(|s| s.display_version()),
            flavour: self.server.as_ref().map(|s| s.flavour.name()),
            // A Postgres socket can be pinned for a tab's lifetime.
            affinity: SessionAffinity::Connection,
            // `DISCARD ALL` is a real primitive — the contrast T-072 measured
            // against SQL Server, which has none reachable.
            reset: self.reset,
            // Postgres has a protocol-level cancel request.
            cancel: CancelSupport::Native,
            transactions: Some(TransactionSupport {
                interactive: true,
                // Unlike Mongo's 60s guillotine (T-046), Postgres holds an idle
                // transaction open indefinitely.
                max_idle_seconds: None,
            }),
            raw_payload: RawPayloadKind::Script,
            node_kinds: navigator::node_kinds(),
            filter_ops: filter_ops(self.collation.as_ref().is_none_or(|(_, det)| *det)),
            fixed_schema: true,
            supports_script: true,
            string_collation: self.collation.as_ref().map(|(name, _)| name.clone()),
            read_only: self.read_only.clone(),
            catalog: self.catalog.clone(),
        }
    }

    async fn navigator_children(
        &self,
        parent: Option<NodeId>,
    ) -> Result<Vec<NavNode>, DriverError> {
        if let Some(hit) = self.nav_cache.lock().unwrap_or_else(|p| p.into_inner()).get(&parent) {
            return Ok(hit.clone());
        }
        let key = parent.clone();
        let hidden: Vec<String> = self
            .server
            .as_ref()
            .map(|s| s.hidden_schemas().iter().map(|h| h.to_string()).collect())
            .unwrap_or_default();
        let nodes = self
            .with_lease(false, async |client: &tokio_postgres::Client| {
                navigator::children(&client, parent, &hidden, &self.catalog).await
            })
            .await?;
        // A notice (permission-denied, a vanished relation) is not catalog
        // content: never cached, so the next expand tries again.
        if !nodes.iter().any(navigator::is_notice) {
            self.nav_cache.lock().unwrap_or_else(|p| p.into_inner()).insert(key, nodes.clone());
        }
        Ok(nodes)
    }

    async fn browse(&self, request: BrowseRequest) -> Result<ExecutionResult, DriverError> {
        self.with_lease(false, async |client: &tokio_postgres::Client| {
            let (compiled, gate) = self.compile_browse(client, &request).await?;
            let params: Vec<&(dyn ToSql + Sync)> =
                compiled.params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();

            let rows = client
                .query(&compiled.sql, &params)
                .await
                .map_err(|e| error_map::map_error(&e, Some(&compiled.sql)))?;

            let mut out_rows: Vec<Vec<WireValue>> = Vec::with_capacity(rows.len());
            for row in &rows {
                out_rows.push(self.decode_row(&compiled, row, &request.relation)?);
            }

            let truncated = out_rows.len() as u32 >= request.limit;
            // T-047: declare the strategy used and where the next page starts.
            let page = compiled.paging.page_info(
                &compiled.columns,
                out_rows.last().map(Vec::as_slice),
                out_rows.len(),
                request.limit,
            );
            // D-029: structured browse always yields exactly one `rows` outcome.
            let mut result = ExecutionResult::single_rows(compiled.column_descs(), out_rows, truncated);
            result.page = Some(page);
            result.edit = Some(gate);
            Ok(result)
        })
        .await
    }

    /// D-027 / T-056: every staged edit in ONE transaction, or none of them.
    ///
    /// Each edit is compiled against the catalog as it stands now — the gate
    /// the webview saw is not trusted — before `BEGIN`, so a refused edit never
    /// opens a transaction. Inside it, each `UPDATE` must affect exactly one
    /// row; anything else (zero: the row is gone or its key changed) rolls the
    /// whole batch back and is reported as a conflict at that edit's ordinal.
    async fn preview_edits(&self, edits: Vec<EditRequest>) -> Result<EditPlan, DriverError> {
        // Structured catalog reads only: the connection stays clean.
        self.with_lease(false, async |client: &tokio_postgres::Client| {
            let compiled = self.compile_edits(client, &edits).await?;
            Ok(EditPlan::new(compiled.iter().map(edit::CompiledEdit::planned).collect()))
        })
        .await
    }

    async fn apply_edits(
        &self,
        edits: Vec<EditRequest>,
        expected_digest: Option<String>,
    ) -> Result<ExecutionResult, DriverError> {
        if edits.is_empty() {
            return Ok(ExecutionResult {
                outcomes: vec![],
                tx: TxContext { mode: TxMode::ImplicitSingle, disposition: TxDisposition::Committed },
                page: None,
                budget: None,
                edit: None,
            });
        }
        // Dirty: a dropped connection mid-transaction must not be returned to
        // the pool as though clean; the reset path rolls it back.
        let result = self
            .with_lease(true, async |client: &tokio_postgres::Client| {
                let compiled = self.compile_edits(client, &edits).await?;
                // T-057 / R-003: run only what was previewed. The plan is
                // rebuilt by the same code as the preview; a different
                // digest means the statements would differ from what the
                // user was shown, so nothing runs.
                if let Some(expected) = &expected_digest {
                    let plan = EditPlan::new(compiled.iter().map(edit::CompiledEdit::planned).collect());
                    if &plan.digest != expected {
                        return Err(DriverError::new(
                            ErrorKind::Conflict,
                            "the statements these edits compile to have changed since they were previewed; review the preview again",
                        ));
                    }
                }

                client.batch_execute("BEGIN").await.map_err(|e| error_map::map_error(&e, Some("BEGIN")))?;
                let mut outcomes = Vec::with_capacity(compiled.len());
                for (i, c) in compiled.iter().enumerate() {
                    let params: Vec<&(dyn ToSql + Sync)> =
                        c.params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
                    let failure = match client.execute(&c.sql, &params).await {
                        Ok(1) => None,
                        Ok(0) => {
                            // T-058: tell "gone" from "changed since read".
                            let pp: Vec<&(dyn ToSql + Sync)> =
                                c.probe_params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
                            let exists = client.query_opt(&c.probe_sql, &pp).await.ok().flatten().is_some();
                            Some(c.conflict(i, exists))
                        }
                        Ok(n) => Some(DriverError::new(
                            ErrorKind::Conflict,
                            format!("edit {}: matched {n} rows instead of one; no edits were applied", i + 1),
                        )),
                        Err(e) => Some(error_map::map_error(&e, Some(&c.sql))),
                    };
                    let ordinal = i as u32;
                    if let Some(error) = failure {
                        let _ = client.batch_execute("ROLLBACK").await;
                        outcomes.push(Outcome {
                            attribution: Attribution::Ordinal { ordinal },
                            body: OutcomeBody::Error { error },
                        });
                        return Ok(ExecutionResult {
                            outcomes,
                            tx: TxContext {
                                mode: TxMode::ImplicitSingle,
                                disposition: TxDisposition::RolledBack { rolled_back: (0..ordinal).collect() },
                            },
                            page: None,
                            budget: None,
                            edit: None,
                        });
                    }
                    outcomes.push(Outcome {
                        attribution: Attribution::Ordinal { ordinal },
                        body: OutcomeBody::Affected {
                            count: 1,
                            statement_kind: Some("UPDATE".into()),
                            concurrency_omitted: c.concurrency_omitted.clone(),
                            document_errors: vec![],
                        },
                    });
                }
                if let Err(e) = client.batch_execute("COMMIT").await {
                    let n = outcomes.len() as u32;
                    return Ok(ExecutionResult {
                        outcomes: vec![Outcome {
                            attribution: Attribution::Ordinal { ordinal: n.saturating_sub(1) },
                            body: OutcomeBody::Error { error: error_map::map_error(&e, Some("COMMIT")) },
                        }],
                        tx: TxContext {
                            mode: TxMode::ImplicitSingle,
                            disposition: TxDisposition::RolledBack { rolled_back: (0..n).collect() },
                        },
                        page: None,
                        budget: None,
                        edit: None,
                    });
                }
                Ok(ExecutionResult {
                    outcomes,
                    tx: TxContext { mode: TxMode::ImplicitSingle, disposition: TxDisposition::Committed },
                    page: None,
                    budget: None,
                    edit: None,
                })
            })
            .await?;
        Ok(self.redact_outcomes(result))
    }

    async fn raw(&self, request: RawRequest) -> Result<ExecutionResult, DriverError> {
        let script = match request {
            RawRequest::Script(s) => s,
            RawRequest::CommandDocument(_) => {
                return Err(DriverError::new(
                    ErrorKind::Unknown,
                    "this driver declares raw_payload = script",
                ))
            }
        };
        let script_for_cache = script.clone();

        let budget = self.budget;
        let result = self
            .with_lease(true, async |client: &tokio_postgres::Client| run_script(client, &script, budget).await)
        .await?;
        // T-024: a failing statement arrives as an `Error` *outcome* inside an
        // `Ok` result, so `with_lease`'s error redaction never sees it — and the
        // engine quotes offending values back verbatim. Redact every one.
        // T-053 / D-016: DDL this app ran invalidates the navigator cache.
        // Done whether or not the script succeeded — a partially applied
        // explicit transaction can still have changed the catalog.
        if navigator::script_may_change_catalog(&script_for_cache) {
            self.refresh_catalog();
        }
        Ok(self.redact_outcomes(result))
    }
}

/// Run a raw script verbatim on `client` and build its outcome sequence
/// (R-040, T-012, T-050). Shared by the pooled raw path and pinned editor
/// sessions (T-041), which then correct `tx` for an explicit transaction.
pub(crate) async fn run_script(
    client: &tokio_postgres::Client,
    script: &str,
    budget: sluice_core::ExecBudget,
) -> Result<ExecutionResult, DriverError> {

    // R-040: the user's text goes to the engine VERBATIM as one batch. No
    // client-side rewriting, so a splitter bug cannot manufacture a syntax
    // error the user did not write.
    // T-050: streamed, metered against the per-execution budget —
    // rows past it are dropped on arrival, never materialized.
    let result = collect_budgeted(client, script, budget).await;

    let split = sluice_core::script::split(script, ScriptDialect::Postgres);

    match result {
        Ok((bodies, report)) => {
            let attributions = sluice_core::script::attribute(&split, bodies.len());
            let outcomes: Vec<Outcome> = bodies
                .into_iter()
                .zip(attributions)
                .map(|(body, attribution)| Outcome { attribution, body })
                .collect();

            // T-012 / R-041: state what actually survived. Postgres wraps a
            // multi-statement simple query in one implicit transaction; a
            // single statement is its own autocommit unit.
            let mode = if outcomes.len() > 1 {
                TxMode::ImplicitSingle
            } else {
                TxMode::AutocommitPerStatement
            };
            Ok(ExecutionResult {
                outcomes,
                tx: TxContext { mode, disposition: TxDisposition::Committed },
                page: None,
                budget: Some(report),
                edit: None,
            })
        }
        Err(e) => {
            // The batch failed. Postgres wrapped it in one implicit
            // transaction, so every earlier statement was rolled back —
            // reporting them as successes is exactly R-041's false
            // partial-success, and the reason nothing is fabricated here.
            //
            // tokio-postgres surfaces a failed simple query as a single
            // Err with no partial messages, so the successful statements'
            // outcomes are genuinely unavailable. What IS recoverable is
            // *which* statement failed: the error carries a position, and
            // T-011 already computed the spans, so intersecting the two
            // identifies the failing ordinal instead of blaming statement 1.
            let err = error_map::map_error(&e, Some(script));
            let statement_count = split.statements.len().max(1);

            let failing = err
                .hints
                .position
                .filter(|_| split.confident)
                .and_then(|p| {
                    split.statements.iter().position(|s| {
                        p.utf16_offset >= s.start_utf16 && p.utf16_offset <= s.end_utf16
                    })
                });

            let ordinal = failing.unwrap_or(0) as u32;
            let attribution = match failing {
                Some(i) => Attribution::Span {
                    ordinal,
                    start_utf16: split.statements[i].start_utf16,
                    end_utf16: split.statements[i].end_utf16,
                },
                None => Attribution::Ordinal { ordinal },
            };

            let outcomes = vec![Outcome {
                attribution,
                body: OutcomeBody::Error { error: err },
            }];

            // Everything before the failing statement ran and was undone.
            // When the failing statement is unknown, the honest answer is
            // that the whole batch was undone.
            let undone_count = failing.unwrap_or(statement_count) as u32;
            let rolled_back: Vec<u32> = (0..undone_count).collect();

            let multi = statement_count > 1;
            Ok(ExecutionResult {
                outcomes,
                tx: TxContext {
                    mode: if multi {
                        TxMode::ImplicitSingle
                    } else {
                        TxMode::AutocommitPerStatement
                    },
                    disposition: if multi && !rolled_back.is_empty() {
                        TxDisposition::RolledBack { rolled_back }
                    } else {
                        TxDisposition::Committed
                    },
                },
                page: None,
                budget: None,
                edit: None,
            })
        }
    }
}

/// T-050: run a script and turn its message stream into D-029 outcome bodies,
/// keeping rows only while the execution's budget lasts.
///
/// Every statement still runs to completion — the budget limits what is
/// *retained for display*, never what executes. Stopping a script early
/// because a SELECT was large would silently skip the writes after it.
async fn collect_budgeted(
    client: &tokio_postgres::Client,
    script: &str,
    budget: sluice_core::ExecBudget,
) -> Result<(Vec<OutcomeBody>, sluice_core::BudgetReport), tokio_postgres::Error> {
    use futures_util::TryStreamExt as _;

    let stream = client.simple_query_raw(script).await?;
    let mut stream = std::pin::pin!(stream);
    let mut meter = sluice_core::BudgetMeter::new(budget);
    let mut out: Vec<OutcomeBody> = Vec::new();
    let mut pending_cols: Option<Vec<sluice_core::ColumnDesc>> = None;
    let mut pending_rows: Vec<Vec<WireValue>> = Vec::new();
    let mut pending_truncated = false;

    while let Some(msg) = stream.try_next().await? {
        match msg {
            SimpleQueryMessage::RowDescription(cols) => {
                flush_budgeted(&mut out, &mut pending_cols, &mut pending_rows, &mut pending_truncated);
                pending_cols = Some(cols.iter().map(script_column).collect());
            }
            SimpleQueryMessage::Row(row) => {
                // Once spent, later rows are not even decoded.
                if meter.exhausted() {
                    pending_truncated = true;
                    continue;
                }
                let cells: Vec<WireValue> = (0..row.columns().len())
                    .map(|i| match row.get(i) {
                        Some(s) => WireValue::Text(s.to_string()),
                        None => WireValue::Null,
                    })
                    .collect();
                if meter.admit(sluice_core::row_cost(&cells), out.len() as u32) {
                    pending_rows.push(cells);
                } else {
                    pending_truncated = true;
                }
            }
            SimpleQueryMessage::CommandComplete(n) => {
                if pending_cols.is_some() {
                    flush_budgeted(&mut out, &mut pending_cols, &mut pending_rows, &mut pending_truncated);
                } else {
                    out.push(OutcomeBody::Affected {
                        count: n,
                        statement_kind: None,
                        concurrency_omitted: vec![],
                        document_errors: vec![],
                    });
                }
            }
            _ => {}
        }
    }
    flush_budgeted(&mut out, &mut pending_cols, &mut pending_rows, &mut pending_truncated);
    if out.is_empty() {
        out.push(OutcomeBody::Status { message: "OK".into(), document: None });
    }
    Ok((out, meter.report()))
}

fn script_column(c: &tokio_postgres::SimpleColumn) -> sluice_core::ColumnDesc {
    sluice_core::ColumnDesc {
        name: c.name().to_string(),
        // The simple query protocol returns every value in TEXT format and
        // does NOT carry type OIDs, so the honest descriptor is `unknown`.
        type_name: "unknown".into(),
        type_class: TypeClass::Unknown,
        nullable: true,
        mixed: false,
        derived: true,
        exact: None,
        // Raw results are never editable.
        concurrency_comparable: false,
    }
}

fn flush_budgeted(
    out: &mut Vec<OutcomeBody>,
    cols: &mut Option<Vec<sluice_core::ColumnDesc>>,
    rows: &mut Vec<Vec<WireValue>>,
    truncated: &mut bool,
) {
    if let Some(columns) = cols.take() {
        out.push(OutcomeBody::Rows { columns, rows: std::mem::take(rows), truncated: *truncated });
    }
    *truncated = false;
}

/// Opaque re-fetch token for an oversized value (T-018).
///
/// Encodes the relation, the row's `ctid`, and the column. `ctid` is a physical
/// row pointer, so it is stable only until the row is updated or `VACUUM FULL`
/// moves it — a stale handle resolves to nothing and the UI reports the value as
/// gone rather than returning a different row's data.
fn make_handle(relation: &NodeId, ctid: &str, column: &str) -> String {
    use base64::Engine as _;
    let payload = serde_json::json!({
        "rel": relation.path,
        "ctid": ctid,
        "col": column,
    });
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
}

pub fn decode_handle(handle: &str) -> Option<(Vec<String>, String, String)> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(handle).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let rel = v.get("rel")?.as_array()?.iter().filter_map(|s| s.as_str().map(String::from)).collect();
    Some((
        rel,
        v.get("ctid")?.as_str()?.to_string(),
        v.get("col")?.as_str()?.to_string(),
    ))
}

/// The operators this driver declares (T-013 owns the full set).
///
/// R-009: `case_sensitive` is declared, not assumed. Postgres `contains`
/// compiles to `ILIKE`, which is case-INSENSITIVE — the same behaviour the
/// Mongo driver was made to match, so the declared value is the same on both.
/// `strings_case_sensitive`: what the database's default collation does —
/// deterministic collations distinguish case for `=` and `LIKE`. That is
/// reported, not overridden (R-009); the per-predicate toggle opts out.
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
    vec![
        FilterOpDecl { id: "eq".into(), label: "equals".into(), type_classes: all.clone(), case_sensitive: Some(strings_case_sensitive), case_insensitive_toggle: true },
        FilterOpDecl { id: "ne".into(), label: "not equals".into(), type_classes: all.clone(), case_sensitive: Some(strings_case_sensitive), case_insensitive_toggle: true },
        FilterOpDecl { id: "gt".into(), label: "greater than".into(), type_classes: all.clone(), case_sensitive: None, case_insensitive_toggle: false },
        FilterOpDecl { id: "gte".into(), label: "at least".into(), type_classes: all.clone(), case_sensitive: None, case_insensitive_toggle: false },
        FilterOpDecl { id: "lt".into(), label: "less than".into(), type_classes: all.clone(), case_sensitive: None, case_insensitive_toggle: false },
        FilterOpDecl { id: "lte".into(), label: "at most".into(), type_classes: all.clone(), case_sensitive: None, case_insensitive_toggle: false },
        FilterOpDecl { id: "contains".into(), label: "contains".into(), type_classes: vec![TypeClass::Text], case_sensitive: Some(strings_case_sensitive), case_insensitive_toggle: true },
        FilterOpDecl { id: "starts_with".into(), label: "starts with".into(), type_classes: vec![TypeClass::Text], case_sensitive: Some(strings_case_sensitive), case_insensitive_toggle: true },
        FilterOpDecl { id: "ends_with".into(), label: "ends with".into(), type_classes: vec![TypeClass::Text], case_sensitive: Some(strings_case_sensitive), case_insensitive_toggle: true },
        FilterOpDecl { id: "in".into(), label: "is one of".into(), type_classes: all.clone(), case_sensitive: Some(strings_case_sensitive), case_insensitive_toggle: true },
        FilterOpDecl { id: "between".into(), label: "between".into(), type_classes: all.clone(), case_sensitive: None, case_insensitive_toggle: false },
        FilterOpDecl { id: "is_null".into(), label: "is null".into(), type_classes: vec![], case_sensitive: None, case_insensitive_toggle: false },
        FilterOpDecl { id: "is_not_null".into(), label: "is not null".into(), type_classes: vec![], case_sensitive: None, case_insensitive_toggle: false },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-022: the guarantee is unchanged, but it is now carried by the *type*
    /// rather than by a hand-written `Debug` on this one struct — so the
    /// assertion is against `Secret`'s own constant, not a literal this test
    /// could silently drift from.
    #[test]
    fn connect_spec_debug_never_prints_the_password() {
        let spec = ConnectSpec {
            password: Some(Secret::new("hunter2".into())),
            ..Default::default()
        };
        let s = format!("{spec:?}");
        assert!(!s.contains("hunter2"), "{s}");
        assert!(s.contains(sluice_core::REDACTED), "{s}");
    }

    #[test]
    fn handles_round_trip() {
        let rel = NodeId { kind: "table".into(), path: vec!["public".into(), "docs".into()] };
        let h = make_handle(&rel, "(0,1)", "body");
        let (path, ctid, col) = decode_handle(&h).unwrap();
        assert_eq!(path, vec!["public", "docs"]);
        assert_eq!(ctid, "(0,1)");
        assert_eq!(col, "body");
    }

    /// T-013: the Postgres driver declares every core operator (D-005).
    #[test]
    fn every_core_filter_operator_is_declared() {
        let missing = sluice_core::missing_core_ops(&filter_ops(true));
        assert!(missing.is_empty(), "core operators not declared: {missing:?}");
    }

    #[test]
    fn contains_declares_case_insensitivity_matching_the_mongo_driver() {
        // T-015 / R-009: the engine's semantics are reported, not overridden —
        // under a deterministic collation `LIKE` distinguishes case — and the
        // toggle is offered on string operators only.
        let ops = filter_ops(true);
        let c = ops.iter().find(|o| o.id == "contains").unwrap();
        assert_eq!(c.case_sensitive, Some(true));
        assert!(c.case_insensitive_toggle);
        assert!(!ops.iter().find(|o| o.id == "gt").unwrap().case_insensitive_toggle);
        assert_eq!(filter_ops(false).iter().find(|o| o.id == "eq").unwrap().case_sensitive, Some(false));
    }
}
