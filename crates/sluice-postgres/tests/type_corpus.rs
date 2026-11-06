//! The Postgres type-fidelity corpus (T-068 / D-021, D-006).
//!
//! D-021 singles this out as the one test worth writing beyond unit tests,
//! because D-006 is the decision that fails **silently**: a numeric losing a
//! digit or a timestamp losing its offset is a plausible-looking wrong value,
//! and manual use cannot catch it — noticing requires already knowing the
//! right answer. This file is where the right answers are written down.
//!
//! Every case is checked three ways:
//!
//! 1. **Read** — browsing the stored value yields exactly the expected
//!    `WireValue`.
//! 2. **Wire** — that value survives the JSON encoding the webview receives:
//!    serialize, deserialize, compare.
//! 3. **Write-back** — the value read, bound back as a filter parameter through
//!    the same text-and-cast path an edit uses, finds the original row. This is
//!    R-005's "assert edit round-trips as well as reads": if the read lost
//!    anything, the server no longer considers it equal. Types with no `=`
//!    operator (`json`, `point`, `xml`) say so instead.
//!
//! ## Running it
//!
//! Gated on `TEST_PG_URL` and **skipped** by default, per D-021:
//!
//! ```sh
//! docker run -d --name sluice-pg -e POSTGRES_PASSWORD=sluice_dev \
//!   -e POSTGRES_DB=sluice_dev -p 55432:5432 postgres:17
//! TEST_PG_URL=postgres://postgres:sluice_dev@localhost:55432/sluice_dev?sslmode=disable \
//!   cargo test -p sluice-postgres --test type_corpus -- --nocapture
//! ```
//!
//! It prints one machine-readable summary line, `TYPE-CORPUS: ...`, either
//! `SKIPPED` or a case and check count. R-018 is that an opt-in test nothing
//! runs is indistinguishable from an absent one; T-069's CI job greps this line
//! and fails on a skip or a zero count, so a broken env var cannot pass.
//!
//! ## Engine facts the corpus records
//!
//! - Postgres `text` cannot hold U+0000 and `jsonb` rejects `\u0000`; the NUL
//!   cases are therefore `bytea` (raw zero bytes) and `json` (the escape kept
//!   verbatim), plus a test that the server rejects a NUL in `text` loudly.
//! - `Decimal128` is Mongo's; no Mongo driver exists yet. Its extremes are
//!   covered at the wire-contract level in `sluice-core`, and belong in a Mongo
//!   corpus beside this one when that engine lands (R-018's Action).

use std::str::FromStr;

use sluice_core::{
    BrowseRequest, Driver, FilterTree, NodeId, OutcomeBody, Predicate, RawRequest, Secret,
    SortKey, WireValue,
};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

const SCHEMA: &str = "type_corpus";

/// What the read must produce.
enum Expect {
    /// Exactly this value.
    Is(WireValue),
    /// Where the canonical text depends on the session (time zone) rather
    /// than the stored value, a check plus the write-back is the honest test.
    Check(fn(&WireValue) -> Result<(), String>),
}

struct Case {
    name: &'static str,
    sql_type: &'static str,
    /// A SQL expression for the stored value.
    literal: &'static str,
    expect: Expect,
    /// `None` = write-back applies; `Some(why)` = the type has no equality.
    no_writeback: Option<&'static str>,
}

fn num(s: &str) -> WireValue {
    WireValue::Numeric(s.into())
}
fn ts(s: &str) -> WireValue {
    WireValue::Timestamp(s.into())
}
fn text(s: &str) -> WireValue {
    WireValue::Text(s.into())
}
fn opaque(type_name: &str, rendered: &str) -> WireValue {
    WireValue::Opaque { type_name: type_name.into(), rendered: rendered.into() }
}

fn case(name: &'static str, sql_type: &'static str, literal: &'static str, expect: WireValue) -> Case {
    Case { name, sql_type, literal, expect: Expect::Is(expect), no_writeback: None }
}

/// A timestamptz renders in the session's zone; assert the instant survived
/// and the offset is present, and let write-back prove equality.
fn has_offset(v: &WireValue) -> Result<(), String> {
    match v {
        WireValue::Timestamp(s) if s.contains('+') || s.rsplit_once('-').is_some_and(|(_, z)| z.len() <= 5) => Ok(()),
        WireValue::Timestamp(s) => Err(format!("no UTC offset in {s:?}")),
        other => Err(format!("expected a tagged timestamp, got {other:?}")),
    }
}

fn corpus() -> Vec<Case> {
    let deep_jsonb: &'static str = Box::leak(
        format!("'{}1{}'::jsonb", "[".repeat(500), "]".repeat(500)).into_boxed_str(),
    );
    let deep_expect = format!("{}1{}", "[".repeat(500), "]".repeat(500));

    vec![
        // ---- integers: only int2/int4 go raw; int8 is exact text (D-006)
        case("int2 min", "int2", "-32768", WireValue::Int(-32768)),
        case("int2 max", "int2", "32767", WireValue::Int(32767)),
        case("int4 min", "int4", "-2147483648", WireValue::Int(-2147483648)),
        case("int4 max", "int4", "2147483647", WireValue::Int(2147483647)),
        case("int8 max", "int8", "9223372036854775807", num("9223372036854775807")),
        case("int8 min", "int8", "-9223372036854775808", num("-9223372036854775808")),
        case("int8 2^53+1", "int8", "9007199254740993", num("9007199254740993")),
        // ---- exact decimals
        case("numeric 38 digits", "numeric", "12345678901234567890123456789012345678",
             num("12345678901234567890123456789012345678")),
        case("numeric(38,0) max", "numeric(38,0)", "99999999999999999999999999999999999999",
             num("99999999999999999999999999999999999999")),
        case("numeric tiny", "numeric", "-0.00000000000000000000000000000000000001",
             num("-0.00000000000000000000000000000000000001")),
        case("numeric scale kept", "numeric(20,10)", "1.5", num("1.5000000000")),
        case("numeric 1+1e-20", "numeric", "1.00000000000000000001", num("1.00000000000000000001")),
        case("numeric NaN", "numeric", "'NaN'", num("NaN")),
        case("numeric +inf", "numeric", "'Infinity'", num("Infinity")),
        case("numeric -inf", "numeric", "'-Infinity'", num("-Infinity")),
        // ---- binary floats: raw only when JSON can carry them
        case("float8 0.1", "float8", "0.1", WireValue::Float(0.1)),
        case("float8 max", "float8", "1.7976931348623157e308", WireValue::Float(f64::MAX)),
        case("float8 min subnormal", "float8", "5e-324", WireValue::Float(5e-324)),
        case("float8 -0", "float8", "'-0'", WireValue::Float(-0.0)),
        case("float4 max", "float4", "3.4028235e38", WireValue::Float(f32::MAX as f64)),
        case("float8 NaN", "float8", "'NaN'", opaque("float8", "NaN")),
        case("float8 +inf", "float8", "'Infinity'", opaque("float8", "Infinity")),
        case("float4 -inf", "float4", "'-Infinity'", opaque("float4", "-Infinity")),
        // ---- money (exact, but locale-rendered by the server)
        case("money max", "money", "'92233720368547758.07'", num("$92,233,720,368,547,758.07")),
        // ---- boolean
        case("bool true", "bool", "true", WireValue::Bool(true)),
        case("bool false", "bool", "false", WireValue::Bool(false)),
        // ---- text
        case("text empty is not null", "text", "''", text("")),
        case("text emoji and zwj", "text", "'😀 👨‍👩‍👧 ﷽'", text("😀 👨‍👩‍👧 ﷽")),
        case("text rtl and combining", "text", "'שָׁלוֹם é'", text("שָׁלוֹם é")),
        case("text quotes and backslashes", "text", r#"E'it''s \\ "q" \t tab'"#, text("it's \\ \"q\" \t tab")),
        case("text newline", "text", "E'line1\\nline2\\r\\n'", text("line1\nline2\r\n")),
        case("varchar(3) multibyte", "varchar(3)", "'日本語'", text("日本語")),
        case("char(5) padding", "char(5)", "'ab'", text("ab")),
        case("name", "name", "'pg_catalog'", text("pg_catalog")),
        // ---- uuid
        case("uuid nil", "uuid", "'00000000-0000-0000-0000-000000000000'",
             WireValue::Uuid("00000000-0000-0000-0000-000000000000".into())),
        case("uuid max", "uuid", "'FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF'",
             WireValue::Uuid("ffffffff-ffff-ffff-ffff-ffffffffffff".into())),
        // ---- bytea, including the NUL-byte case text cannot hold
        case("bytea empty", "bytea", "''::bytea", WireValue::Bytes("".into())),
        case("bytea with NULs", "bytea", "'\\x00ff0000'::bytea", WireValue::Bytes("AP8AAA==".into())),
        // ---- temporal: negative (BC / pre-epoch), extreme and infinite
        case("date BC", "date", "'4713-01-01 BC'", ts("4713-01-01 BC")),
        case("date max", "date", "'5874897-12-31'", ts("5874897-12-31")),
        case("date infinity", "date", "'infinity'", ts("infinity")),
        case("date -infinity", "date", "'-infinity'", ts("-infinity")),
        case("timestamp pre-epoch micros", "timestamp", "'1969-12-31 23:59:59.999999'",
             ts("1969-12-31 23:59:59.999999")),
        case("timestamp BC", "timestamp", "'0044-03-15 12:00:00 BC'", ts("0044-03-15 12:00:00 BC")),
        case("timestamp infinity", "timestamp", "'infinity'", ts("infinity")),
        Case {
            name: "timestamptz with offset",
            sql_type: "timestamptz",
            literal: "'2026-08-02 12:34:56.123456+05:30'",
            expect: Expect::Check(has_offset),
            no_writeback: None,
        },
        Case {
            name: "timestamptz pre-epoch",
            sql_type: "timestamptz",
            literal: "'1900-01-01 00:00:00.000001-08'",
            expect: Expect::Check(has_offset),
            no_writeback: None,
        },
        case("time max", "time", "'24:00:00'", ts("24:00:00")),
        case("timetz offset", "timetz", "'23:59:59.999999-15:59'", ts("23:59:59.999999-15:59")),
        case("interval mixed signs", "interval", "'-178000000 years 3 mons -4 days 05:06:07.000008'",
             // Postgres normalizes months into years; same value (write-back proves it).
             ts("-177999999 years -9 mons -4 days +05:06:07.000008")),
        // ---- arrays, empty and hostile
        case("int4[] empty", "int4[]", "'{}'", opaque("_int4", "{}")),
        case("int8[] extremes and null", "int8[]", "'{-9223372036854775808,NULL,9223372036854775807}'",
             opaque("_int8", "{-9223372036854775808,NULL,9223372036854775807}")),
        case("text[] quoting", "text[]", r#"ARRAY['a,b', 'q"uote', '', NULL, '😀']"#,
             opaque("_text", r#"{"a,b","q\"uote","",NULL,😀}"#)),
        case("int4[][] nested", "int4[]", "'{{1,2},{3,4}}'", opaque("_int4", "{{1,2},{3,4}}")),
        // ---- json / jsonb
        case("jsonb deeply nested", "jsonb", deep_jsonb, text(&deep_expect)),
        case("jsonb big number kept", "jsonb", r#"'{"n": 12345678901234567890.123456789}'"#,
             text(r#"{"n": 12345678901234567890.123456789}"#)),
        case("jsonb empty containers", "jsonb", r#"'{"a": [], "b": {}}'"#, text(r#"{"a": [], "b": {}}"#)),
        Case {
            name: "json keeps NUL escape and key order verbatim",
            sql_type: "json",
            literal: r#"'{"z": "\u0000", "a": 1, "a": 2}'"#,
            expect: Expect::Is(text(r#"{"z": "\u0000", "a": 1, "a": 2}"#)),
            no_writeback: Some("json has no equality operator"),
        },
        // ---- exotic built-ins: no Rust mapping needed (R-014)
        // `inet::text` always keeps the prefix length, unlike inet's output function.
        case("inet v6", "inet", "'2001:db8::1/128'", opaque("inet", "2001:db8::1/128")),
        case("cidr", "cidr", "'10.0.0.0/8'", opaque("cidr", "10.0.0.0/8")),
        case("macaddr8", "macaddr8", "'08:00:2b:01:02:03:04:05'", opaque("macaddr8", "08:00:2b:01:02:03:04:05")),
        case("bit varying", "varbit", "B'101100'", opaque("varbit", "101100")),
        case("tsvector", "tsvector", "to_tsvector('simple', 'the quick fox')",
             opaque("tsvector", "'fox':3 'quick':2 'the':1")),
        case("int4range empty", "int4range", "'empty'", opaque("int4range", "empty")),
        case("numrange exact bounds", "numrange", "'[1.00000000000000000001,2)'",
             opaque("numrange", "[1.00000000000000000001,2)")),
        case("pg_lsn", "pg_lsn", "'FFFFFFFF/FFFFFFFF'", opaque("pg_lsn", "FFFFFFFF/FFFFFFFF")),
        Case {
            name: "point",
            sql_type: "point",
            literal: "'(1.5,-2.25)'",
            expect: Expect::Is(opaque("point", "(1.5,-2.25)")),
            no_writeback: Some("point has no equality operator"),
        },
        Case {
            name: "xml",
            sql_type: "xml",
            literal: "'<a b=\"1\">&amp;😀</a>'",
            expect: Expect::Is(opaque("xml", "<a b=\"1\">&amp;😀</a>")),
            no_writeback: Some("xml has no equality operator"),
        },
    ]
}

fn spec_from_env() -> Option<ConnectSpec> {
    let url = std::env::var("TEST_PG_URL").ok()?;
    let cfg = tokio_postgres::Config::from_str(&url).expect("TEST_PG_URL is not a valid Postgres URL");
    let host = match cfg.get_hosts().first() {
        Some(tokio_postgres::config::Host::Tcp(h)) => h.clone(),
        _ => "localhost".into(),
    };
    let tls = match cfg.get_ssl_mode() {
        tokio_postgres::config::SslMode::Disable => TlsMode::Disabled,
        _ => TlsMode::Verify,
    };
    Some(ConnectSpec {
        host,
        port: cfg.get_ports().first().copied().unwrap_or(5432),
        user: cfg.get_user().unwrap_or("postgres").to_string(),
        password: cfg
            .get_password()
            .map(|p| Secret::new(String::from_utf8_lossy(p).into_owned())),
        dbname: cfg.get_dbname().unwrap_or("postgres").to_string(),
        tls,
        application_name: "sluice-type-corpus".into(),
        statement_timeout: None,
    })
}

async fn exec(d: &PgDriver, sql: &str) -> Result<(), String> {
    let r = d.raw(RawRequest::Script(sql.into())).await.map_err(|e| e.to_string())?;
    for o in &r.outcomes {
        if let OutcomeBody::Error { error } = &o.body {
            return Err(error.to_string());
        }
    }
    Ok(())
}

fn rel(table: &str) -> NodeId {
    NodeId { kind: "table".into(), path: vec![SCHEMA.into(), table.into()] }
}

async fn browse(d: &PgDriver, table: &str, filter: Option<FilterTree>) -> Result<Vec<Vec<WireValue>>, String> {
    let r = d
        .browse(BrowseRequest {
            relation: rel(table),
            filter,
            sort: vec![SortKey { column: "id".into(), descending: false }],
            limit: 10,
            cursor: None,
            offset: None,
        })
        .await
        .map_err(|e| e.to_string())?;
    let (cols, rows) = r.first_rows().ok_or("browse returned no rows outcome")?;
    let vi = cols.iter().position(|c| c.name == "v").ok_or("no column v")?;
    let ii = cols.iter().position(|c| c.name == "id").ok_or("no column id")?;
    Ok(rows.iter().map(|r| vec![r[ii].clone(), r[vi].clone()]).collect())
}

/// Equality that treats a NaN float as equal to itself — the corpus asserts
/// representation, and `NaN != NaN` would make that untestable.
fn same(a: &WireValue, b: &WireValue) -> bool {
    match (a, b) {
        (WireValue::Float(x), WireValue::Float(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

#[tokio::test]
async fn every_type_round_trips_through_the_wire_encoding_unchanged() {
    let Some(spec) = spec_from_env() else {
        println!("TYPE-CORPUS: SKIPPED (TEST_PG_URL unset)");
        return;
    };
    let d = PgDriver::connect(spec).await.expect("TEST_PG_URL is set but the server is unreachable");

    exec(&d, &format!("drop schema if exists {SCHEMA} cascade; create schema {SCHEMA};"))
        .await
        .expect("corpus schema setup");

    let cases = corpus();
    let mut failures: Vec<String> = Vec::new();
    let mut checks = 0usize;

    for (i, c) in cases.iter().enumerate() {
        let table = format!("c{i:03}");
        let setup = format!(
            "create table {SCHEMA}.{table}(id int primary key, v {ty}); \
             insert into {SCHEMA}.{table} values (1, ({lit})::{ty}), (2, null);",
            ty = c.sql_type,
            lit = c.literal,
        );
        if let Err(e) = exec(&d, &setup).await {
            failures.push(format!("[{}] setup failed: {e}", c.name));
            continue;
        }

        // 1. Read.
        let rows = match browse(&d, &table, None).await {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("[{}] browse failed: {e}", c.name));
                continue;
            }
        };
        let got = rows[0][1].clone();
        checks += 1;
        match &c.expect {
            Expect::Is(want) if !same(&got, want) => {
                failures.push(format!("[{}] read: want {want:?}, got {got:?}", c.name))
            }
            Expect::Check(f) => {
                if let Err(e) = f(&got) {
                    failures.push(format!("[{}] read: {e}", c.name));
                }
            }
            _ => {}
        }
        checks += 1;
        if rows[1][1] != WireValue::Null {
            failures.push(format!("[{}] NULL row read as {:?}", c.name, rows[1][1]));
        }

        // 2. The JSON the webview receives.
        checks += 1;
        match serde_json::to_string(&got)
            .map_err(|e| e.to_string())
            .and_then(|j| serde_json::from_str::<WireValue>(&j).map_err(|e| format!("{e} in {j}")))
        {
            Ok(back) if same(&back, &got) => {}
            Ok(back) => failures.push(format!("[{}] JSON changed {got:?} into {back:?}", c.name)),
            Err(e) => failures.push(format!("[{}] JSON round trip failed: {e}", c.name)),
        }

        // 3. Write-back: bind what was read; the server must find the row.
        if c.no_writeback.is_none() {
            checks += 1;
            let filter = FilterTree::Pred(Predicate { case_insensitive: false, column: "v".into(), op: "eq".into(), value: got.clone() });
            match browse(&d, &table, Some(filter)).await {
                Ok(found) if found.len() == 1 && found[0][0] == WireValue::Int(1) => {}
                Ok(found) => failures.push(format!(
                    "[{}] write-back: binding {got:?} matched {} row(s) — the read lost information",
                    c.name,
                    found.len()
                )),
                Err(e) => failures.push(format!("[{}] write-back failed: {e}", c.name)),
            }
        }
    }

    // Engine fact: text cannot hold NUL, and the server must say so rather
    // than truncate.
    checks += 1;
    if exec(&d, &format!("create table {SCHEMA}.nul(v text); insert into {SCHEMA}.nul values (E'a\\u0000b');"))
        .await
        .is_ok()
    {
        failures.push("[text NUL] the server accepted a NUL in text; the corpus needs a text NUL case".into());
    }

    let _ = exec(&d, &format!("drop schema if exists {SCHEMA} cascade;")).await;

    println!("TYPE-CORPUS: {} cases, {checks} checks, {} failure(s)", cases.len(), failures.len());
    assert!(
        failures.is_empty(),
        "type-fidelity corpus: {} failure(s)\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
