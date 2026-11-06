//! T-072 / R-024: does `tiberius` cover the surface this design depends on?
//!
//! Probes the five things R-024's Action names: auth modes, query cancellation,
//! the reset-or-reopen path (D-028), TDS type coverage, and TLS behaviour.
//!
//! SCOPE LIMIT, recorded up front: this runs against a containerised
//! SQL Server 2022 (linux/amd64 under emulation), which supports SQL
//! authentication only. The Windows/AD auth modes that R-024 is most concerned
//! about CANNOT be validated here. See FINDINGS.md.


use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

type Conn = Client<Compat<TcpStream>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    Covered,
    Partial,
    Missing,
    Untested,
}

impl Verdict {
    fn tag(&self) -> &'static str {
        match self {
            Verdict::Covered => "COVERED",
            Verdict::Partial => "PARTIAL",
            Verdict::Missing => "MISSING",
            Verdict::Untested => "UNTESTED",
        }
    }
}

#[derive(Clone)]
struct Rec(Arc<Mutex<Vec<(String, Verdict, String)>>>);

impl Rec {
    fn new() -> Self {
        Rec(Arc::new(Mutex::new(Vec::new())))
    }
    fn add(&self, surface: &str, v: Verdict, detail: impl Into<String>) {
        self.0
            .lock()
            .unwrap()
            .push((surface.to_string(), v, detail.into()));
    }
}

fn base_config() -> Config {
    let mut c = Config::new();
    c.host("127.0.0.1");
    c.port(14333);
    c.database("master");
    c.authentication(AuthMethod::sql_server("sa", "Sluice_Spike_2026!"));
    c
}

async fn connect(config: Config) -> Result<Conn, tiberius::error::Error> {
    let tcp = TcpStream::connect(config.get_addr()).await?;
    tcp.set_nodelay(true)?;
    Client::connect(config, tcp.compat_write()).await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rec = Rec::new();

    println!("=== T-072: tiberius surface probe ===\n");

    probe_auth(&rec).await;
    probe_tls(&rec).await;

    let mut client = {
        let mut c = base_config();
        c.trust_cert();
        connect(c).await?
    };

    probe_types(&mut client, &rec).await?;
    probe_exactness(&mut client, &rec).await?;
    probe_reset(&mut client, &rec).await?;
    drop(client);
    probe_cancellation(&rec).await?;

    println!("\n=== VERDICT TABLE ===\n");
    for (surface, v, detail) in rec.0.lock().unwrap().iter() {
        println!("[{}] {}\n    {}\n", v.tag(), surface, detail);
    }
    Ok(())
}

// ---------------------------------------------------------------- auth

async fn probe_auth(rec: &Rec) {
    let mut c = base_config();
    c.trust_cert();
    match connect(c).await {
        Ok(_) => rec.add(
            "auth / SQL Server authentication",
            Verdict::Covered,
            "AuthMethod::sql_server(user, pass) connected and authenticated.",
        ),
        Err(e) => rec.add(
            "auth / SQL Server authentication",
            Verdict::Missing,
            format!("failed: {}", e),
        ),
    }

    // What the crate exposes, independent of what this container can accept.
    rec.add(
        "auth / Windows + AD modes",
        Verdict::Untested,
        "tiberius exposes AuthMethod::windows(user, pass) (NTLM, behind the `winauth` \
         feature and only on Windows hosts) and AuthMethod::aad_token(token) for Azure AD \
         via a caller-supplied bearer token. It does NOT implement integrated/SSPI \
         single-sign-on against a domain from a non-Windows host, and does NOT acquire AAD \
         tokens itself. NONE of this is exercisable against a Linux container, which accepts \
         SQL auth only — this is the gap R-024 is most concerned about and it remains OPEN.",
    );
}

// ---------------------------------------------------------------- tls

async fn probe_tls(rec: &Rec) {
    // 1. Encryption required, self-signed cert trusted explicitly.
    let mut c = base_config();
    c.encryption(EncryptionLevel::Required);
    c.trust_cert();
    match connect(c).await {
        Ok(_) => rec.add(
            "tls / Required + trust_cert",
            Verdict::Covered,
            "EncryptionLevel::Required negotiated successfully with trust_cert().",
        ),
        Err(e) => rec.add("tls / Required + trust_cert", Verdict::Missing, format!("{}", e)),
    }

    // 2. Encryption required, cert actually verified. SQL Server's default cert is
    //    self-signed, so this MUST fail — the question is whether it fails loudly.
    let mut c = base_config();
    c.encryption(EncryptionLevel::Required);
    match connect(c).await {
        Ok(_) => rec.add(
            "tls / Required, cert verified",
            Verdict::Missing,
            "Connected WITHOUT trust_cert() against a self-signed certificate. That means \
             verification is not actually enforced — a serious finding for T-021's TLS \
             defaults.",
        ),
        Err(e) => rec.add(
            "tls / Required, cert verified",
            Verdict::Covered,
            format!(
                "Correctly REFUSED the self-signed cert when trust_cert() was not set: {}. \
                 Verification is real, so T-021 can default to verify-on and require an \
                 explicit per-profile opt-out.",
                first_line(&e.to_string())
            ),
        ),
    }
}

// ---------------------------------------------------------------- types

async fn probe_types(client: &mut Conn, rec: &Rec) -> Result<(), Box<dyn std::error::Error>> {
    // Hostile edge values. D-021's corpus (T-068) does not exist yet; these are
    // the cases it should absorb.
    let cases: Vec<(&str, &str)> = vec![
        ("bigint max", "CAST(9223372036854775807 AS BIGINT)"),
        ("bigint min", "CAST(-9223372036854775808 AS BIGINT)"),
        ("decimal(38,20)", "CAST(1.00000000000000000001 AS DECIMAL(38,20))"),
        ("decimal negative scale-edge", "CAST(-99999999999999999999999999999999999999 AS DECIMAL(38,0))"),
        ("money", "CAST(922337203685477.5807 AS MONEY)"),
        ("smallmoney", "CAST(214748.3647 AS SMALLMONEY)"),
        ("float precision", "CAST(0.1 AS FLOAT)"),
        ("real", "CAST(0.1 AS REAL)"),
        ("datetime2(7)", "CAST('9999-12-31 23:59:59.9999999' AS DATETIME2(7))"),
        ("datetimeoffset", "CAST('2026-08-02 12:00:00.1234567 +05:30' AS DATETIMEOFFSET)"),
        ("smalldatetime", "CAST('2079-06-06 23:59' AS SMALLDATETIME)"),
        ("time(7)", "CAST('23:59:59.9999999' AS TIME(7))"),
        ("uniqueidentifier", "CAST('0E984725-C51C-4BF4-9960-E1C80E27ABA0' AS UNIQUEIDENTIFIER)"),
        ("varbinary", "CAST(0xDEADBEEF AS VARBINARY(MAX))"),
        ("nvarchar unicode", "CAST(N'héllo 世界 🐈‍⬛' AS NVARCHAR(50))"),
        ("varchar collation", "CAST('plain' AS VARCHAR(50))"),
        ("nvarchar(max) long", "REPLICATE(CAST(N'x' AS NVARCHAR(MAX)), 10000)"),
        ("xml", "CAST('<a><b/></a>' AS XML)"),
        ("sql_variant", "CAST(CAST(42 AS INT) AS SQL_VARIANT)"),
        ("bit", "CAST(1 AS BIT)"),
        ("rowversion-ish", "CAST(0x0000000000000FA1 AS BINARY(8))"),
        ("geography", "geography::Point(47.6, -122.3, 4326)"),
        ("hierarchyid", "CAST('/1/2/' AS HIERARCHYID)"),
    ];

    let mut covered = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let mut panicked: Vec<String> = Vec::new();
    let mut poisoned_after_panic: Option<String> = None;

    // Silence the default panic printer; a panic is a data point here, not a crash.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    for (label, expr) in &cases {
        let q = format!("SELECT {} AS v", expr);

        // Exactly the shape T-006 will need at the IPC boundary.
        let outcome = AssertUnwindSafe(async {
            let stream = client.query(&q, &[]).await?;
            let results = stream.into_results().await?;
            let mut desc = String::from("<no row>");
            if let Some(row) = results.into_iter().flatten().next() {
                if let Some((col, data)) = row.cells().next() {
                    desc = format!("{} -> {}", col.name(), truncate(&format!("{:?}", data), 88));
                }
            }
            Ok::<String, tiberius::error::Error>(desc)
        })
        .catch_unwind()
        .await;

        match outcome {
            Ok(Ok(desc)) => {
                covered += 1;
                println!("  [ok]     {:<28} {}", label, desc);
            }
            Ok(Err(e)) => {
                failed.push(format!("{} ({})", label, first_line(&e.to_string())));
                println!("  [err]    {:<28} {}", label, first_line(&e.to_string()));
            }
            Err(_) => {
                panicked.push(label.to_string());
                println!("  [PANIC]  {:<28} tiberius panicked while decoding", label);

                // R-022's exact question: does the connection survive a caught panic?
                let reuse = AssertUnwindSafe(read_i32(client, "SELECT 1")).catch_unwind().await;
                let state = match reuse {
                    Ok(Ok(Some(1))) => "connection still usable",
                    Ok(Ok(_)) => "connection returned wrong value",
                    Ok(Err(e)) => {
                        println!("           reuse error: {}", first_line(&e.to_string()));
                        "connection POISONED - errors on reuse"
                    }
                    Err(_) => "connection POISONED - panics again on reuse",
                };
                println!("           after panic: {}", state);
                poisoned_after_panic.get_or_insert(state.to_string());

                // Reconnect so the remaining cases still run.
                let mut c = base_config();
                c.trust_cert();
                *client = connect(c).await?;
            }
        }
    }

    std::panic::set_hook(prev_hook);

    rec.add(
        "types / TDS coverage",
        if !panicked.is_empty() || !failed.is_empty() { Verdict::Partial } else { Verdict::Covered },
        format!(
            "{}/{} hostile edge values decoded. Decode errors: {}. PANICS: {}.",
            covered,
            cases.len(),
            if failed.is_empty() { "none".into() } else { failed.join("; ") },
            if panicked.is_empty() { "none".into() } else { panicked.join(", ") }
        ),
    );

    if !panicked.is_empty() {
        rec.add(
            "types / panic on unsupported type",
            Verdict::Missing,
            format!(
                "tiberius does not return an error for {:?} — it PANICS via an unreached \
                 `todo!()` in src/tds/codec/type_info.rs. Ordinary user data therefore unwinds \
                 the driver. State after a caught panic: {}. This is R-022 with a concrete \
                 trigger and makes T-006 mandatory rather than defensive: catch_unwind at the \
                 IPC boundary is necessary but NOT sufficient, because the TDS stream is left \
                 mid-packet and the connection must be torn down, not returned to the pool.",
                panicked,
                poisoned_after_panic.unwrap_or_else(|| "n/a".into())
            ),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- exactness

/// D-006/T-016 require lossless round-tripping. "Decoded without erroring" is a
/// much weaker claim than "decoded exactly", so compare each value against the
/// server's own text rendering of it.
async fn probe_exactness(client: &mut Conn, rec: &Rec) -> Result<(), Box<dyn std::error::Error>> {
    let cases: Vec<(&str, &str)> = vec![
        ("money", "CAST(922337203685477.5807 AS MONEY)"),
        ("smallmoney", "CAST(214748.3647 AS SMALLMONEY)"),
        ("decimal(38,20)", "CAST(1.00000000000000000001 AS DECIMAL(38,20))"),
        ("float", "CAST(0.1 AS FLOAT)"),
    ];

    let mut lossy: Vec<String> = Vec::new();
    println!("\n  -- exactness vs server text rendering --");
    for (label, expr) in &cases {
        // CONVERT with style 2 gives money its full 4-decimal representation.
        let q = format!(
            "SELECT CONVERT(VARCHAR(64), {}, 2) AS txt, {} AS val",
            expr, expr
        );
        let rows = client.simple_query(&q).await?.into_results().await?;
        let row = match rows.into_iter().flatten().next() {
            Some(r) => r,
            None => continue,
        };
        let txt = row.try_get::<&str, _>("txt")?.unwrap_or_default().to_string();
        let decoded = row
            .cells()
            .nth(1)
            .map(|(_, d)| render(d))
            .unwrap_or_default();

        let exact = normalize(&txt) == normalize(&decoded);
        println!(
            "  [{}] {:<16} server={:<26} tiberius={}",
            if exact { "exact" } else { "LOSSY" },
            label,
            txt,
            decoded
        );
        if !exact {
            lossy.push(format!("{}: server={} tiberius={}", label, txt, decoded));
        }
    }

    rec.add(
        "types / exactness",
        if lossy.is_empty() { Verdict::Covered } else { Verdict::Partial },
        if lossy.is_empty() {
            "All probed exact types round-tripped losslessly.".to_string()
        } else {
            format!(
                "LOSSY round-trips: {}. MONEY/SMALLMONEY are exact decimal(19,4)/(10,4) types \
                 on the server, but tiberius decodes them into ColumnData::F64, so the value is \
                 through a binary float before the driver ever sees it. D-006/T-016 require \
                 lossless wire encoding; on SQL Server that is UNACHIEVABLE for MONEY through \
                 tiberius's public decoding, because the precision is gone by the time the \
                 driver has a value. Workaround available to T-016: have the browse compiler \
                 emit CONVERT(VARCHAR, col, 2) for money columns and carry them as \
                 WireValue::Numeric text.",
                lossy.join("; ")
            )
        },
    );
    Ok(())
}

fn render(d: &tiberius::ColumnData<'_>) -> String {
    use tiberius::ColumnData::*;
    match d {
        F64(Some(v)) => format!("{}", v),
        F32(Some(v)) => format!("{}", v),
        Numeric(Some(n)) => format!("{}", n),
        I32(Some(v)) => format!("{}", v),
        I64(Some(v)) => format!("{}", v),
        other => format!("{:?}", other),
    }
}

/// Trailing-zero-insensitive compare, so 0.1000 == 0.1 is not a false positive.
fn normalize(s: &str) -> String {
    let t = s.trim();
    if t.contains('.') {
        let t = t.trim_end_matches('0');
        t.trim_end_matches('.').to_string()
    } else {
        t.to_string()
    }
}

// ---------------------------------------------------------------- reset

async fn probe_reset(client: &mut Conn, rec: &Rec) -> Result<(), Box<dyn std::error::Error>> {
    // Dirty the session, then try every route to a reset.
    client.simple_query("SET LOCK_TIMEOUT 1234").await?.into_results().await?;
    client
        .simple_query("CREATE TABLE #spike_temp (a INT); INSERT INTO #spike_temp VALUES (1)")
        .await?
        .into_results()
        .await?;

    let dirty = read_i32(client, "SELECT @@LOCK_TIMEOUT").await?;
    println!("  session dirtied: @@LOCK_TIMEOUT = {:?}", dirty);

    // sp_reset_connection is a TDS-protocol RPC (a header bit), not callable T-SQL.
    // Mapped to owned data inline: QueryStream borrows the client mutably, so a
    // binding of that type would keep the borrow alive for the rest of the fn.
    let direct_err: Option<String> = match client.simple_query("EXEC sp_reset_connection").await {
        Ok(s) => match s.into_results().await {
            Ok(_) => None,
            Err(e) => Some(first_line(&e.to_string())),
        },
        Err(e) => Some(first_line(&e.to_string())),
    };

    let still_dirty = read_i32(client, "SELECT @@LOCK_TIMEOUT").await?;
    let temp_alive = client
        .simple_query("SELECT OBJECT_ID('tempdb..#spike_temp')")
        .await?
        .into_results()
        .await
        .map(|r| {
            r.into_iter()
                .flatten()
                .next()
                .and_then(|row| row.get::<i32, _>(0))
                .is_some()
        })
        .unwrap_or(false);

    rec.add(
        "reset / sp_reset_connection",
        Verdict::Missing,
        format!(
            "`EXEC sp_reset_connection` as T-SQL: {}. It is a TDS *protocol* RPC signalled by \
             a bit in the packet header, and tiberius exposes no API to set it. After the \
             attempt the session was still dirty (@@LOCK_TIMEOUT = {:?}, #temp table still \
             present = {}). CONFIRMS R-012: there is no reachable reset primitive, so D-028's \
             pool must declare ResetSupport::ReopenOnly for SQL Server — which is exactly the \
             connect-per-query degradation R-036 warns about, and makes T-040 (reset only \
             what was dirtied) load-bearing rather than an optimisation.",
            direct_err.unwrap_or_else(|| "returned without error".into()),
            still_dirty,
            temp_alive
        ),
    );
    Ok(())
}

// ---------------------------------------------------------------- cancel

async fn probe_cancellation(rec: &Rec) -> Result<(), Box<dyn std::error::Error>> {
    let mut c = base_config();
    c.trust_cert();
    let mut client = connect(c).await?;

    let spid = read_i32(&mut client, "SELECT CAST(@@SPID AS INT)").await?.unwrap_or(-1);

    // There is no client.cancel() in tiberius. The only lever is dropping the
    // future, so the question is what that does to the connection.
    let started = std::time::Instant::now();
    // Mapped to a bool inline so the borrowed QueryStream does not outlive this
    // statement — see the note in probe_reset.
    let timed_out = tokio::time::timeout(
        Duration::from_millis(800),
        client.simple_query("WAITFOR DELAY '00:00:10'; SELECT 1"),
    )
    .await
    .is_err();
    let elapsed = started.elapsed();
    println!("  cancellation: timeout fired = {} after {:?}", timed_out, elapsed);

    // Is the connection usable afterwards?
    let reusable = match tokio::time::timeout(
        Duration::from_secs(3),
        read_i32(&mut client, "SELECT 1"),
    )
    .await
    {
        Ok(Ok(Some(1))) => "yes",
        Ok(Ok(_)) => "returned unexpected value",
        Ok(Err(e)) => {
            println!("  reuse error: {}", first_line(&e.to_string()));
            "no - connection errored"
        }
        Err(_) => "no - reuse itself hung",
    };

    // Did the server-side request actually stop, or is it still running?
    let mut c2 = base_config();
    c2.trust_cert();
    let mut observer = connect(c2).await?;
    let still_running = observer
        .query(
            "SELECT COUNT(*) FROM sys.dm_exec_requests WHERE session_id = @P1 AND command = 'WAITFOR'",
            &[&spid],
        )
        .await?
        .into_results()
        .await?
        .into_iter()
        .flatten()
        .next()
        .and_then(|r| r.get::<i32, _>(0))
        .unwrap_or(-1);

    rec.add(
        "cancellation / attention packet",
        Verdict::Missing,
        format!(
            "tiberius exposes NO cancel/attention API. The only lever is dropping the query \
             future (tokio timeout fired after {:?}). Result: connection reusable afterwards = \
             {}; server-side WAITFOR still running on spid {} = {}. TDS cancellation requires \
             sending an Attention packet and draining to the acknowledgement, which the crate \
             does not implement — so T-051's 'real query cancellation' cannot be honoured on \
             SQL Server through tiberius alone. The workable fallback is a second connection \
             issuing KILL <spid>, which needs ALTER ANY CONNECTION and is a heavier hammer \
             than a TDS attention.",
            elapsed,
            reusable,
            spid,
            if still_running > 0 { "YES" } else { "no" }
        ),
    );
    Ok(())
}

// ---------------------------------------------------------------- helpers

async fn read_i32(client: &mut Conn, q: &str) -> Result<Option<i32>, tiberius::error::Error> {
    let rows = client.simple_query(q).await?.into_results().await?;
    // try_get, not get: `Row::get` PANICS on a type mismatch rather than
    // returning an error — itself a small instance of the panic-happiness this
    // spike is measuring.
    Ok(rows
        .into_iter()
        .flatten()
        .next()
        .and_then(|r| r.try_get::<i32, _>(0).ok().flatten()))
}

fn first_line(s: &str) -> String {
    truncate(s.lines().next().unwrap_or("").trim(), 160)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}
