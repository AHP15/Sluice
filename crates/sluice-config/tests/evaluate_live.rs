//! End-to-end evaluation against a real interpreter (T-029 / D-026).
//!
//! The unit tests in `src/evaluate.rs` prove the *gate* — that declining does
//! not execute, that the dialog names both absolute paths, that an edited config
//! re-asks. They deliberately use a non-existent interpreter, so they prove
//! nothing about whether evaluation actually produces a connection.
//!
//! These tests spawn the real thing. They are **skipped, not failed**, when no
//! interpreter is resolvable, so `cargo test` stays green on a machine without
//! node or bun — the same convention `sluice-postgres/tests/live.rs` uses.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sluice_config::consent::{
    ConsentDecision, ConsentPrompt, ConsentRequest, ConsentStore, Fingerprint,
};
use sluice_config::connection_url::ConnectionUrl;
use sluice_config::evaluate::{
    evaluate_with_consent, DeclineReason, EnvPolicy, EvalError, EvalOutcome, TargetDecision,
    UnconfirmedTarget, DEFAULT_TIMEOUT,
};
use sluice_config::interpreter::{self, ResolvedInterpreter};

struct Grant;
impl ConsentPrompt for Grant {
    fn ask(&self, _: &ConsentRequest) -> ConsentDecision {
        ConsentDecision::GrantAndRemember
    }
}

struct Decline;
impl ConsentPrompt for Decline {
    fn ask(&self, _: &ConsentRequest) -> ConsentDecision {
        ConsentDecision::Decline
    }
}

fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join("sluice-eval-live").join(format!("{tag}-{nanos}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Resolve a real interpreter, or `None` to skip.
fn interpreter(scan_dir: &Path) -> Option<ResolvedInterpreter> {
    match interpreter::resolve(scan_dir, None) {
        Ok(i) => Some(i),
        Err(e) => {
            eprintln!("SKIP: no trusted interpreter available ({e})");
            None
        }
    }
}

/// The only way past an evaluated target is to confirm it, which is the
/// display-before-connect rule (R-032) enforced by the type.
fn connect(target: UnconfirmedTarget) -> ConnectionUrl {
    target.confirm(TargetDecision::Connect).expect("Connect releases the URL")
}

fn store(dir: &Path) -> ConsentStore {
    ConsentStore::open_at(dir.join("consent.json"))
}

/// The happy path D-026 exists to preserve: a real TS config that computes its
/// URL, evaluated once the user consents.
#[test]
fn a_consented_typescript_config_yields_a_connection() {
    let dir = tmp("ts-ok");
    let Some(interp) = interpreter(&dir) else { return };

    let cfg = dir.join("drizzle.config.ts");
    std::fs::write(
        &cfg,
        r#"
        const port: number = 5432;
        export default {
          dialect: "postgresql",
          dbCredentials: { url: `postgres://app:s3cret@db.internal:${port}/shop` },
        };
        "#,
    )
    .unwrap();

    let out = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Grant,
        "test".into(),
        DEFAULT_TIMEOUT,
        &EnvPolicy::default(),
    );

    match out {
        Ok(EvalOutcome::Evaluated { target, .. }) => {
            let url = connect(target);
            assert_eq!(url.host.as_deref(), Some("db.internal"));
            assert_eq!(url.port, Some(5432));
            assert_eq!(url.database.as_deref(), Some("shop"));
            assert_eq!(url.user.as_deref(), Some("app"));
            // T-022: the password from an evaluated config is wrapped like any
            // other parse boundary.
            assert_eq!(url.password.as_ref().unwrap().expose(), "s3cret");
            assert!(!format!("{url:?}").contains("s3cret"));
        }
        // Node older than the type-stripping flag fails loudly; that is the
        // designed behaviour, not a test failure.
        Err(EvalError::NonZeroExit { .. }) if interp.kind == interpreter::InterpreterKind::Node => {
            eprintln!("SKIP: node cannot strip types on this version");
        }
        other => panic!("expected an evaluated connection, got {other:?}"),
    }
}

#[test]
fn a_consented_javascript_config_yields_a_connection() {
    let dir = tmp("js-ok");
    let Some(interp) = interpreter(&dir) else { return };

    let cfg = dir.join("drizzle.config.js");
    std::fs::write(&cfg, r#"export default { url: "postgres://u:p@h:5432/js_db" };"#).unwrap();

    let out = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Grant,
        "test".into(),
        DEFAULT_TIMEOUT,
        &EnvPolicy::default(),
    )
    .expect("evaluation");

    match out {
        EvalOutcome::Evaluated { target, .. } => {
            let url = connect(target);
            assert_eq!(url.database.as_deref(), Some("js_db"));
        }
        other => panic!("expected Evaluated, got {other:?}"),
    }
}

/// **The R-001 end-to-end test.** Declining must leave the config unexecuted —
/// proven by a side effect the config would have caused had it run.
#[test]
fn declining_really_does_not_execute_the_config() {
    let dir = tmp("no-exec");
    let Some(interp) = interpreter(&dir) else { return };

    let marker = dir.join("EXECUTED");
    let cfg = dir.join("drizzle.config.js");
    std::fs::write(
        &cfg,
        format!(
            r#"
            import {{ writeFileSync }} from "node:fs";
            writeFileSync({:?}, "the config executed");
            export default {{ url: "postgres://attacker@evil/db" }};
            "#,
            marker.to_string_lossy()
        ),
    )
    .unwrap();

    let out = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Decline,
        "test".into(),
        DEFAULT_TIMEOUT,
        &EnvPolicy::default(),
    )
    .expect("declining is not an error");

    assert!(matches!(
        out,
        EvalOutcome::Declined { reason: DeclineReason::UserDeclined }
    ));
    assert!(
        !marker.exists(),
        "R-001: the config executed despite the user declining consent"
    );
}

/// The control for the test above — the same config, consented, *does* run.
/// Without this, `declining_really_does_not_execute_the_config` would pass even
/// if evaluation were broken entirely.
#[test]
fn the_non_execution_test_has_a_working_control() {
    let dir = tmp("exec-control");
    let Some(interp) = interpreter(&dir) else { return };

    let marker = dir.join("EXECUTED");
    let cfg = dir.join("drizzle.config.js");
    std::fs::write(
        &cfg,
        format!(
            r#"
            import {{ writeFileSync }} from "node:fs";
            writeFileSync({:?}, "the config executed");
            export default {{ url: "postgres://u:p@h/control" }};
            "#,
            marker.to_string_lossy()
        ),
    )
    .unwrap();

    let out = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Grant,
        "test".into(),
        DEFAULT_TIMEOUT,
        &EnvPolicy::default(),
    )
    .expect("evaluation");

    assert!(matches!(out, EvalOutcome::Evaluated { .. }));
    assert!(marker.exists(), "control failed: consented config did not run");
}

/// R-032's first loud-failure rule: a throwing config is an error, never a
/// silent fallback to the inert path's guess.
#[test]
fn a_throwing_config_surfaces_an_error_rather_than_falling_back() {
    let dir = tmp("throws");
    let Some(interp) = interpreter(&dir) else { return };

    let cfg = dir.join("drizzle.config.js");
    std::fs::write(&cfg, r#"throw new Error("DATABASE_URL is not set");"#).unwrap();

    let err = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Grant,
        "test".into(),
        DEFAULT_TIMEOUT,
        &EnvPolicy::default(),
    )
    .expect_err("a throwing config must be an error");

    assert!(matches!(err, EvalError::NonZeroExit { .. }), "{err:?}");
    assert!(err.to_string().contains("DATABASE_URL is not set"), "{err}");
}

/// A config that returns no URL is an error too — the app must not proceed with
/// a guess it did not get from the config.
#[test]
fn a_config_without_a_url_is_an_error() {
    let dir = tmp("nourl");
    let Some(interp) = interpreter(&dir) else { return };

    let cfg = dir.join("drizzle.config.js");
    std::fs::write(&cfg, r#"export default { dialect: "postgresql" };"#).unwrap();

    let err = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Grant,
        "test".into(),
        DEFAULT_TIMEOUT,
        &EnvPolicy::default(),
    )
    .expect_err("no URL must be an error");

    assert!(matches!(err, EvalError::NoUrlInOutput), "{err:?}");
}

/// D-026 requires a timeout. A config that never settles must be killed.
#[test]
fn a_hanging_config_times_out_and_is_killed() {
    let dir = tmp("hang");
    let Some(interp) = interpreter(&dir) else { return };

    let cfg = dir.join("drizzle.config.js");
    // Must hold the event loop OPEN, not merely await forever: a promise with
    // no pending handle lets the loop drain and the runtime exits 0, which is a
    // config that produced no URL rather than one that hung. The timer is what
    // makes this a genuine hang.
    std::fs::write(
        &cfg,
        "await new Promise((r) => setTimeout(r, 60000));\nexport default { url: \"postgres://u:p@h/never\" };",
    )
    .unwrap();

    let started = std::time::Instant::now();
    let err = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Grant,
        "test".into(),
        Duration::from_millis(600),
        &EnvPolicy::default(),
    )
    .expect_err("a hanging config must time out");

    assert!(matches!(err, EvalError::TimedOut { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5), "timeout was not enforced promptly");
}

/// Consent survives across store instances, so the second launch in a project
/// does not re-prompt — the ergonomic half of D-026.
#[test]
fn a_second_run_in_the_same_project_does_not_re_prompt() {
    let dir = tmp("remember-live");
    let Some(interp) = interpreter(&dir) else { return };

    let cfg = dir.join("drizzle.config.js");
    std::fs::write(&cfg, r#"export default { url: "postgres://u:p@h/remembered" };"#).unwrap();

    let first = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Grant,
        "test".into(),
        DEFAULT_TIMEOUT,
        &EnvPolicy::default(),
    )
    .expect("first run");
    assert!(matches!(first, EvalOutcome::Evaluated { .. }));

    // A fresh store from disk, and a prompt that would decline if consulted.
    let second = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Decline,
        "test".into(),
        DEFAULT_TIMEOUT,
        &EnvPolicy::default(),
    )
    .expect("second run");

    match second {
        EvalOutcome::Evaluated { target, .. } => {
            let url = connect(target);
            assert_eq!(url.database.as_deref(), Some("remembered"));
        }
        other => panic!("remembered consent was not honoured: {other:?}"),
    }
}

/// And the invalidation: editing the config after a grant re-prompts, so the
/// declining prompt now takes effect.
#[test]
fn editing_the_config_after_a_grant_re_prompts() {
    let dir = tmp("reask-live");
    let Some(interp) = interpreter(&dir) else { return };

    let cfg = dir.join("drizzle.config.js");
    std::fs::write(&cfg, r#"export default { url: "postgres://u:p@h/v1" };"#).unwrap();

    let mut s = store(&dir);
    s.grant(&dir, Fingerprint::compute(&cfg, &interp.path).unwrap()).unwrap();

    std::fs::write(&cfg, r#"export default { url: "postgres://u:p@h/v2" };"#).unwrap();

    let out = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Decline,
        "test".into(),
        DEFAULT_TIMEOUT,
        &EnvPolicy::default(),
    )
    .expect("declining is not an error");

    assert!(
        matches!(out, EvalOutcome::Declined { .. }),
        "an edited config must re-prompt, and the decline must take effect"
    );
}

/// R-032's loud-failure rule, under a noisy config: one that writes far more
/// than an OS pipe buffer to stderr before failing must surface *its* error,
/// not hang on a full pipe and be misreported as a timeout.
#[test]
fn a_noisy_failing_config_reports_its_own_error_not_a_timeout() {
    let dir = tmp("noisy");
    let Some(interp) = interpreter(&dir) else { return };

    let cfg = dir.join("drizzle.config.js");
    std::fs::write(
        &cfg,
        r#"
        for (let i = 0; i < 4000; i++) process.stderr.write("deprecation warning ".repeat(10) + "\n");
        for (let i = 0; i < 4000; i++) process.stdout.write("debug log line ".repeat(10) + "\n");
        throw new Error("PGHOST missing in noisy config");
        "#,
    )
    .unwrap();

    let err = evaluate_with_consent(
        &cfg,
        &interp,
        &mut store(&dir),
        &Grant,
        "test".into(),
        Duration::from_secs(8),
        &EnvPolicy::default(),
    )
    .expect_err("a throwing config must be an error");

    assert!(matches!(err, EvalError::NonZeroExit { .. }), "expected the config's own failure, got {err:?}");
}
