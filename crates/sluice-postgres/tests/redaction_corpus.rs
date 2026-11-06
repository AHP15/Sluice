//! T-024 / R-023: the redaction fixture corpus — real error shapes, each
//! asserted to contain no credential substring in any encoding.
//!
//! R-023's point is that redaction which passes a synthetic test can leak on
//! the error shapes that occur in production. So these are produced by a real
//! server and a real driver: a bad password, the engine echoing the password
//! back inside an error, a TLS failure, a DNS failure, and a driver panic. The
//! password is deliberately hostile — URL, quoting and non-ASCII characters —
//! so every encoding path is exercised.
//!
//! Skipped (not failed) without a server, like `live.rs`; CI runs it.

use sluice_core::{guard_driver_call, Driver, DriverError, ErrorKind, RawRequest, Redactor, Secret, SessionId};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

const PW: &str = "p@ss w:rd/'é\"x";
const ROLE: &str = "redact_probe";

fn spec(user: &str, password: &str, host: &str, tls: TlsMode) -> ConnectSpec {
    ConnectSpec {
        host: host.into(),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: user.into(),
        password: Some(Secret::new(password.into())),
        dbname: "sluice_dev".into(),
        tls,
        application_name: "sluice-redaction".into(),
        statement_timeout: None,
    }
}

/// Every form R-023 names, so "absent" means absent in each of them.
fn encodings(p: &str) -> Vec<String> {
    let pct = |upper: bool, plus: bool| -> String {
        p.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    (b as char).to_string()
                } else if plus && b == b' ' {
                    "+".into()
                } else if upper {
                    format!("%{b:02X}")
                } else {
                    format!("%{b:02x}")
                }
            })
            .collect()
    };
    vec![
        p.to_string(),
        pct(true, false),
        pct(false, false),
        pct(true, true),
        p.replace('\'', "''"),
        p.replace('\\', "\\\\").replace('\'', "\\'"),
        serde_json::to_string(p).unwrap().trim_matches('"').to_string(),
    ]
}

fn assert_no_credential(label: &str, e: &DriverError, secret: &str) {
    let json = serde_json::to_string(e).unwrap();
    let debug = format!("{e:?}");
    for form in encodings(secret) {
        assert!(!json.contains(&form), "[{label}] {form:?} survived in JSON: {json}");
        assert!(!debug.contains(&form), "[{label}] {form:?} survived in Debug: {debug}");
    }
}

async fn admin() -> Option<std::sync::Arc<PgDriver>> {
    match PgDriver::connect(spec("postgres", "sluice_dev", "localhost", TlsMode::Disabled)).await {
        Ok(d) => Some(d),
        Err(e) => {
            eprintln!("SKIP: no Postgres reachable ({e})");
            None
        }
    }
}

#[tokio::test]
async fn no_credential_survives_any_real_error_shape() {
    let Some(admin) = admin().await else { return };
    let quoted = PW.replace('\'', "''");
    admin
        .raw(RawRequest::Script(format!(
            "DO $$ BEGIN IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{ROLE}') THEN \
             CREATE ROLE {ROLE} LOGIN; END IF; END $$; ALTER ROLE {ROLE} PASSWORD '{quoted}';"
        )))
        .await
        .expect("create probe role");

    let mut checked = 0;

    // 1. Bad password — a hostile *wrong* password, which the redactor still
    //    knows because it is the one this connection was given.
    let wrong = format!("{PW}-wrong");
    let e = PgDriver::connect(spec(ROLE, &wrong, "localhost", TlsMode::Disabled))
        .await
        .err()
        .expect("a wrong password must fail");
    assert_eq!(e.kind, ErrorKind::AuthFailed, "{e:?}");
    assert_no_credential("bad password", &e, &wrong);
    checked += 1;

    // 2. The engine echoing the password back: Postgres quotes the bad literal
    //    in the message, so the raw password genuinely appears in driver text.
    let d = PgDriver::connect(spec(ROLE, PW, "localhost", TlsMode::Disabled))
        .await
        .expect("the probe role logs in with the hostile password");
    let r = d.raw(RawRequest::Script(format!("SELECT '{quoted}'::int"))).await.expect("raw");
    let echoed = r
        .outcomes
        .iter()
        .find_map(|o| match &o.body {
            sluice_core::OutcomeBody::Error { error } => Some(error.clone()),
            _ => None,
        })
        .expect("the cast must fail");
    assert!(
        echoed.message.contains("[redacted]"),
        "the engine did echo the value; it must arrive redacted: {echoed:?}"
    );
    assert_no_credential("engine echo", &echoed, PW);
    checked += 1;

    // 3. TLS failure: verification demanded of a server that offers none.
    let e = PgDriver::connect(spec(ROLE, PW, "localhost", TlsMode::Verify))
        .await
        .err()
        .expect("TLS must fail against a non-TLS server");
    assert_no_credential("tls", &e, PW);
    checked += 1;

    // 4. DNS failure.
    let e = PgDriver::connect(spec(ROLE, PW, "no-such-host.invalid", TlsMode::Disabled))
        .await
        .err()
        .expect("DNS must fail");
    assert_eq!(e.kind, ErrorKind::ConnectionFailed, "{e:?}");
    assert_no_credential("dns", &e, PW);
    checked += 1;

    // 5. A driver panic whose text carries the connection URL and password —
    //    the path the driver never sees as an error. Caught by the guard, then
    //    redacted with the session's credentials, as the app does.
    let session = SessionId("probe::1".into());
    let panicked = guard_driver_call(&session, async {
        let url = format!("postgres://{ROLE}:{PW}@localhost/sluice_dev");
        if !url.is_empty() {
            panic!("decoder failed while handling {url} (password {PW})");
        }
        Ok::<(), DriverError>(())
    })
    .await
    .expect_err("the panic surfaces as an error");
    let panicked = Redactor::new([Secret::new(PW.to_string())]).error(panicked);
    assert_eq!(panicked.kind, ErrorKind::DriverBug);
    assert_no_credential("driver panic", &panicked, PW);
    checked += 1;

    let _ = admin.raw(RawRequest::Script(format!("DROP ROLE IF EXISTS {ROLE}"))).await;
    println!("REDACTION-CORPUS: {checked} error shapes, 0 credential leaks");
}

/// D-007 / T-021: `Verify` must *require* TLS. tokio-postgres's default
/// `Prefer` fell back to plaintext against a TLS-less server — a downgrade
/// anyone on the path could force. Against the (TLS-less) test server, Verify
/// and AcceptInvalidCerts must both refuse to connect, and say why.
#[tokio::test]
async fn a_tls_mode_never_falls_back_to_plaintext() {
    if admin().await.is_none() {
        return;
    }
    for mode in [TlsMode::Verify, TlsMode::AcceptInvalidCerts] {
        let e = PgDriver::connect(spec("postgres", "sluice_dev", "localhost", mode))
            .await
            .err()
            .unwrap_or_else(|| panic!("{mode:?} connected without TLS — downgrade"));
        let text = serde_json::to_string(&e).unwrap();
        assert!(text.to_lowercase().contains("tls"), "{mode:?}: the error should name TLS: {text}");
        eprintln!("{mode:?} → {} {:?}", e.message, e.payload.extra);
    }
}
