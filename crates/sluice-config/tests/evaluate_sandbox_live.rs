//! R-033 end-to-end on Linux: a consented config run under bubblewrap cannot
//! reach the session bus or this app's credential files (T-032).
//!
//! A real `drizzle.config.js` reports what it can see — the bus variable, the
//! bus socket, the credential key file — and the test asserts each is hidden.
//! It then runs the **same config without the sandbox** and asserts each is
//! visible, so a sandbox that silently did nothing (or a fixture that hid
//! nothing to begin with) fails instead of passing.
//!
//! One test function in its own binary, because it mutates the process
//! environment (see `evaluate_env_live.rs`). Skipped, not failed, off Linux,
//! without a trusted interpreter, or where bubblewrap cannot run (no
//! unprivileged user namespaces) — CI installs bubblewrap so it runs there.

#[cfg(target_os = "linux")]
#[test]
fn a_sandboxed_config_cannot_reach_the_bus_or_the_credential_files() {
    use sluice_config::consent::{ConsentDecision, ConsentPrompt, ConsentRequest, ConsentStore};
    use sluice_config::evaluate::{
        evaluate_with_consent, probe_sandbox, EnvPolicy, EvalOutcome, TargetDecision, DEFAULT_TIMEOUT,
    };
    use sluice_config::interpreter;
    use sluice_config::sandbox::Sandbox;

    struct Grant;
    impl ConsentPrompt for Grant {
        fn ask(&self, _: &ConsentRequest) -> ConsentDecision {
            ConsentDecision::GrantOnce
        }
    }

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("sluice-sandbox-live-{nanos}"));
    let project = root.join("project");
    let runtime = root.join("xdg-runtime");
    let data = root.join("data").join("sluice");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::create_dir_all(data.join("credentials")).unwrap();

    let bus = runtime.join("bus");
    std::fs::write(&bus, "stand-in for the session bus socket").unwrap();
    let key = data.join("credentials").join("key");
    std::fs::write(&key, [7u8; 32]).unwrap();

    let Ok(interp) = interpreter::resolve(&project, None) else {
        eprintln!("SKIP: no trusted interpreter available");
        return;
    };
    let sandbox = probe_sandbox(vec![data.clone()]);
    if !sandbox.is_active() {
        eprintln!("SKIP: bubblewrap unavailable or cannot create namespaces here");
        return;
    }

    std::env::set_var("XDG_RUNTIME_DIR", &runtime);
    std::env::set_var("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={}", bus.display()));

    let cfg = project.join("drizzle.config.js");
    std::fs::write(
        &cfg,
        format!(
            r#"
            import {{ existsSync }} from "node:fs";
            const seen = [
              process.env.DBUS_SESSION_BUS_ADDRESS ? "bus_env_visible" : "bus_env_hidden",
              existsSync({bus:?}) ? "bus_socket_visible" : "bus_socket_hidden",
              existsSync({key:?}) ? "key_visible" : "key_hidden",
            ].join("__");
            export default {{ url: `postgres://u@h/${{seen}}` }};
            "#,
            bus = bus.to_string_lossy(),
            key = key.to_string_lossy(),
        ),
    )
    .unwrap();

    let seen = |sandbox: Sandbox| -> String {
        let policy = EnvPolicy { sandbox, ..EnvPolicy::default() };
        let mut store = ConsentStore::open_at(root.join("consent.json"));
        match evaluate_with_consent(&cfg, &interp, &mut store, &Grant, "t".into(), DEFAULT_TIMEOUT, &policy)
            .expect("evaluation")
        {
            EvalOutcome::Evaluated { target, .. } => target
                .confirm(TargetDecision::Connect)
                .and_then(|u| u.database)
                .unwrap_or_default(),
            other => panic!("expected Evaluated, got {other:?}"),
        }
    };

    let sandboxed = seen(sandbox);
    let control = seen(Sandbox::Off);

    std::env::remove_var("DBUS_SESSION_BUS_ADDRESS");
    std::env::remove_var("XDG_RUNTIME_DIR");

    assert_eq!(
        control, "bus_env_visible__bus_socket_visible__key_visible",
        "control: without the sandbox the fixture must be visible, or this test proves nothing"
    );
    assert_eq!(
        sandboxed, "bus_env_hidden__bus_socket_hidden__key_hidden",
        "R-033: the sandboxed config could still reach the bus or the credential files"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn a_sandboxed_config_cannot_reach_the_bus_or_the_credential_files() {
    eprintln!("SKIP: bubblewrap sandboxing is Linux-only (macOS/Windows scope keychain items per app)");
}
