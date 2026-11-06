//! T-061 / D-027 against a real server: the read-only probe reports what the
//! database actually enforces. Skips when the dev container is not running
//! (see live.rs for the `docker run` line).

use sluice_core::{Driver, RawRequest, ReadOnlyGuarantee, Secret};
use sluice_postgres::{tls::TlsMode, ConnectSpec, PgDriver};

fn spec(user: &str, password: &str) -> ConnectSpec {
    ConnectSpec {
        host: std::env::var("SLUICE_TEST_PG_HOST").unwrap_or_else(|_| "localhost".into()),
        port: std::env::var("SLUICE_TEST_PG_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(55432),
        user: user.into(),
        password: Some(Secret::new(password.into())),
        dbname: "sluice_dev".into(),
        tls: TlsMode::Disabled,
        application_name: "sluice-tests".into(),
        statement_timeout: None,
    }
}

#[tokio::test]
async fn the_probe_tells_a_superuser_from_a_read_only_role() {
    let admin = match PgDriver::connect(spec("postgres", "sluice_dev")).await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIP: no Postgres reachable ({e})");
            return;
        }
    };
    let caps = admin.capabilities();
    let ReadOnlyGuarantee::NotEnforced { reasons } = &caps.read_only else { panic!("{:?}", caps.read_only) };
    assert!(reasons.iter().any(|r| r.contains("superuser")), "{reasons:?}");

    // A role that can only read: no table writes, no CREATE anywhere. On
    // Postgres < 15 `public` grants CREATE to PUBLIC, so revoke it from the
    // role's view explicitly — otherwise the probe would (correctly) report it.
    admin
        .raw(RawRequest::Script(
            "DROP TABLE IF EXISTS public.t061_ro; CREATE TABLE public.t061_ro (id int);
             DO $$ BEGIN
               IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 't061_reader') THEN
                 REASSIGN OWNED BY t061_reader TO postgres; DROP OWNED BY t061_reader; DROP ROLE t061_reader;
               END IF;
             END $$;
             CREATE ROLE t061_reader LOGIN PASSWORD 'reader_pw';
             REVOKE CREATE ON SCHEMA public FROM PUBLIC;
             REVOKE CREATE ON DATABASE sluice_dev FROM PUBLIC;
             GRANT SELECT ON public.t061_ro TO t061_reader;"
                .into(),
        ))
        .await
        .expect("setup");

    let reader = PgDriver::connect(spec("t061_reader", "reader_pw")).await.expect("reader connects");
    let ro = reader.capabilities().read_only;
    assert!(ro.is_enforced(), "{ro:?}");
    drop(reader);

    // A table write grant flips it, and says which way it can write.
    admin
        .raw(RawRequest::Script("GRANT INSERT ON public.t061_ro TO t061_reader;".into()))
        .await
        .expect("grant");
    let writer = PgDriver::connect(spec("t061_reader", "reader_pw")).await.expect("reconnects");
    let ReadOnlyGuarantee::NotEnforced { reasons } = writer.capabilities().read_only else { panic!() };
    assert!(reasons.iter().any(|r| r.contains("INSERT")), "{reasons:?}");
    drop(writer);

    let _ = admin
        .raw(RawRequest::Script(
            "DROP TABLE public.t061_ro; DROP OWNED BY t061_reader; DROP ROLE t061_reader;".into(),
        ))
        .await;
}
