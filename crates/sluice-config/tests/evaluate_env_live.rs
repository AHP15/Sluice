//! R-032 end-to-end: the config subprocess sees the user's environment, minus
//! only what the app itself contributed (T-031).
//!
//! The unit tests in `src/evaluate.rs` prove `EnvPolicy::resolve_env`. This
//! proves the *spawn* actually applies it — that a real `drizzle.config.js`
//! branching on `process.env` takes the branch the user's terminal would.
//!
//! **One test function, in its own binary, on purpose.** It mutates the process
//! environment, and `set_var` racing a concurrent `getenv` in another test
//! thread is undefined behaviour on some platforms. Cargo runs each `tests/`
//! file as a separate process, so nothing else is running here.
//!
//! Skipped, not failed, when no trusted interpreter is resolvable — the same
//! convention as `evaluate_live.rs`.

use std::path::{Path, PathBuf};

use sluice_config::consent::{ConsentDecision, ConsentPrompt, ConsentRequest, ConsentStore};
use sluice_config::evaluate::{
    evaluate_with_consent, EnvPolicy, EvalOutcome, TargetDecision, DEFAULT_TIMEOUT,
};
use sluice_config::interpreter;

struct Grant;
impl ConsentPrompt for Grant {
    fn ask(&self, _: &ConsentRequest) -> ConsentDecision {
        ConsentDecision::GrantOnce
    }
}

fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let d = std::env::temp_dir().join("sluice-eval-env").join(format!("{tag}-{nanos}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Evaluate `cfg` and return the database name it resolved to.
fn resolved_database(dir: &Path, cfg: &Path) -> Option<String> {
    let interp = interpreter::resolve(dir, None).ok()?;
    let mut store = ConsentStore::open_at(dir.join("consent.json"));
    let out = evaluate_with_consent(
        cfg,
        &interp,
        &mut store,
        &Grant,
        "test".into(),
        DEFAULT_TIMEOUT,
        &EnvPolicy::default(),
    )
    .expect("evaluation");
    match out {
        EvalOutcome::Evaluated { target, .. } => {
            target.confirm(TargetDecision::Connect).and_then(|u| u.database)
        }
        other => panic!("expected Evaluated, got {other:?}"),
    }
}

#[test]
fn the_config_takes_the_branch_the_users_terminal_would() {
    let dir = tmp("env");
    if interpreter::resolve(&dir, None).is_err() {
        eprintln!("SKIP: no trusted interpreter available");
        return;
    }

    // The shape R-032 describes: a config that computes its target from the
    // environment and has a dangerous fallback when a variable is absent.
    let cfg = dir.join("drizzle.config.js");
    std::fs::write(
        &cfg,
        r#"
        const e = process.env;
        const seen = [
          e.T031_STAGE ?? "PRODUCTION_FALLBACK",
          e.T031_FROM_DOTENV ?? "no_dotenv",
          e.SLUICE_T031_INJECTED ? "LEAKED_APP_VAR" : "app_var_stripped",
          e.TAURI_SIGNING_PRIVATE_KEY ? "LEAKED_SIGNING_KEY" : "signing_key_stripped",
        ].join("__");
        export default { url: `postgres://u@h/${seen}` };
        "#,
    )
    .unwrap();
    std::fs::write(dir.join(".env"), "T031_FROM_DOTENV=dotenv_seen\nT031_STAGE=from_dotenv\n")
        .unwrap();

    // The user's shell: a stage variable (which must beat `.env`), plus
    // variables standing in for ones the app and its build tooling inject.
    std::env::set_var("T031_STAGE", "staging");
    std::env::set_var("SLUICE_T031_INJECTED", "x");
    std::env::set_var("TAURI_SIGNING_PRIVATE_KEY", "not-a-real-key");

    let db = resolved_database(&dir, &cfg);

    std::env::remove_var("T031_STAGE");
    std::env::remove_var("SLUICE_T031_INJECTED");
    std::env::remove_var("TAURI_SIGNING_PRIVATE_KEY");

    assert_eq!(
        db.as_deref(),
        Some("staging__dotenv_seen__app_var_stripped__signing_key_stripped"),
        "R-032: the subprocess did not see the environment the user's terminal would"
    );
}
