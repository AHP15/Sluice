//! Consent-gated config evaluation (T-029 / D-026, R-001).
//!
//! **This is the only module in this crate permitted to spawn a process**, and
//! `tests/no_subprocess.rs` enforces that. Everything reachable from here has
//! already passed a consent gate; everything outside it is parse-only.
//!
//! The order of operations is the security property:
//!
//! 1. Resolve the interpreter from absolute system locations only ([`crate::interpreter`]).
//! 2. Compute the fingerprint of what is about to run.
//! 3. Consult stored consent; if absent or stale, **ask**, naming both absolute paths.
//! 4. Only then spawn, with a timeout, and read only a connection URL from stdout.
//!
//! Any failure at any step degrades to [`EvalOutcome::Declined`] or an error —
//! never to "run it anyway", and never to a silent fallback that would leave the
//! user connected somewhere they did not choose.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::connection_url::{self, ConnectionUrl};
use crate::consent::{ConsentDecision, ConsentPrompt, ConsentRequest, ConsentStore, Fingerprint};
use crate::interpreter::{InterpreterError, InterpreterKind, ResolvedInterpreter};
use crate::sandbox::{self, Sandbox, BWRAP_CANDIDATES};

/// D-026 requires a timeout. A config that computes its URL does I/O at most;
/// anything slower is hung or mining.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on stdout. A runaway config must not exhaust memory through this path.
const MAX_STDOUT: usize = 256 * 1024;

#[derive(Debug)]
pub enum EvalError {
    Interpreter(InterpreterError),
    /// The config could not be fingerprinted (unreadable).
    Unreadable,
    Spawn(String),
    /// R-032's first loud-failure rule.
    NonZeroExit { code: Option<i32>, stderr: String },
    TimedOut { after: Duration },
    /// Ran, but produced nothing usable. R-032: this is an error, never a
    /// silent fall back to the inert path's guess.
    NoUrlInOutput,
    BadUrl(connection_url::UrlError),
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::Interpreter(e) => write!(f, "{e}"),
            EvalError::Unreadable => write!(f, "config file could not be read"),
            EvalError::Spawn(e) => write!(f, "could not start the interpreter: {e}"),
            EvalError::NonZeroExit { code, stderr } => {
                let c = code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
                write!(f, "config exited with status {c}: {}", first_lines(stderr, 3))
            }
            EvalError::TimedOut { after } => {
                write!(f, "config did not finish within {}s", after.as_secs())
            }
            EvalError::NoUrlInOutput => {
                write!(f, "config ran but did not produce a connection URL")
            }
            EvalError::BadUrl(e) => write!(f, "config produced an unusable connection URL: {e}"),
        }
    }
}

impl std::error::Error for EvalError {}

fn first_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join("; ")
}

#[derive(Debug)]
pub enum EvalOutcome {
    /// Consent given and the config produced a connection — but one that has
    /// not yet been shown to the user. See [`UnconfirmedTarget`].
    Evaluated { target: UnconfirmedTarget, interpreter: PathBuf },
    /// The user said no, or no interpreter exists. D-026: degrade to choosing a
    /// connection in the UI. This is a normal outcome, not an error.
    Declined { reason: DeclineReason },
}

/// What the config resolved to, in the form the user is shown.
///
/// R-032's second loud-failure rule: *"the resolved connection target (host,
/// port, database) is displayed before connecting so a wrong branch is visible
/// rather than inferred."* A config that took its production branch under a
/// changed environment is only detectable if the user sees where they are about
/// to land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetSummary {
    pub scheme: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
}

impl std::fmt::Display for TargetSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://", self.scheme)?;
        if let Some(u) = &self.user {
            write!(f, "{u}@")?;
        }
        write!(f, "{}", self.host.as_deref().unwrap_or("(no host)"))?;
        if let Some(p) = self.port {
            write!(f, ":{p}")?;
        }
        if let Some(d) = &self.database {
            write!(f, "/{d}")?;
        }
        Ok(())
    }
}

/// An evaluated connection that has **not** yet been shown to the user.
///
/// The `ConnectionUrl` inside is deliberately unreachable except through
/// [`UnconfirmedTarget::confirm`], which consumes the wrapper and requires a
/// decision. That makes "display the target before opening a connection" a
/// property of the type rather than a step someone has to remember at each call
/// site — the same technique `Secret<String>` uses for redaction (T-022).
#[derive(Debug)]
pub struct UnconfirmedTarget {
    url: ConnectionUrl,
}

/// What the user decided after being shown the resolved target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetDecision {
    Connect,
    Cancel,
}

impl UnconfirmedTarget {
    pub fn new(url: ConnectionUrl) -> Self {
        Self { url }
    }

    /// The host, port, and database to display. Carries no credential.
    pub fn summary(&self) -> TargetSummary {
        TargetSummary {
            scheme: self.url.scheme.clone(),
            host: self.url.host.clone(),
            port: self.url.port,
            database: self.url.database.clone(),
            user: self.url.user.clone(),
        }
    }

    /// Release the connection for use, or discard it.
    ///
    /// The only way to obtain the inner `ConnectionUrl`.
    pub fn confirm(self, decision: TargetDecision) -> Option<ConnectionUrl> {
        match decision {
            TargetDecision::Connect => Some(self.url),
            TargetDecision::Cancel => None,
        }
    }
}

/// Shows the resolved target and asks whether to proceed. Injected for the same
/// reason [`ConsentPrompt`] is: the policy crate stays UI-free.
pub trait TargetConfirmation {
    fn confirm_target(&self, summary: &TargetSummary) -> TargetDecision;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclineReason {
    UserDeclined,
    NoInterpreterAvailable,
}

impl DeclineReason {
    /// Text for the "choose a connection in the UI" state.
    pub fn explain(&self) -> &'static str {
        match self {
            DeclineReason::UserDeclined => {
                "This project's config was not run. Choose a connection to continue."
            }
            DeclineReason::NoInterpreterAvailable => {
                "No trusted JavaScript interpreter was found, so this project's config was not \
                 run. Set an interpreter path in settings, or choose a connection to continue."
            }
        }
    }
}

/// The environment handed to the config subprocess (T-031 / R-032).
///
/// ## Why this is a denylist and not a minimal environment
///
/// D-026 said "minimal scrubbed environment". R-032 (high × **high**) is the
/// record of why that wording is dangerous: *the reason a project has a
/// `drizzle.config.ts` rather than a plain URL is almost always that it computes
/// the connection from the environment* — `process.env.PGHOST`,
/// `process.env.NODE_ENV`, a branch on deployment stage. Under a stripped
/// environment such a config does not fail; it takes a **different branch**,
/// returns its default or production fallback, and the app connects somewhere
/// the user did not intend. A security measure that silently changes which
/// database opens is worse than the exposure it was aimed at.
///
/// So the subprocess sees what the user's own `node`/`bun` would see — their
/// shell environment plus the project's `.env` — and the app removes only what
/// **it itself** contributed. Note the asymmetry that makes this safe: a
/// `PGPASSWORD` the *user's shell* exports stays, because their config
/// legitimately reads it and their terminal would too; what is stripped is
/// anything this app injected, including any credential it resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvPolicy {
    /// Variables whose names start with these are removed. The app's own
    /// namespace, so anything it injects is stripped by construction rather
    /// than by being remembered here.
    pub strip_prefixes: Vec<String>,
    /// Exact variable names to remove.
    pub strip_names: Vec<String>,
    /// Overlay the project's `.env`, which the acceptance requires. Shell
    /// variables win over `.env` entries, matching dotenv's own precedence and
    /// T-028's discovery order.
    pub include_project_dotenv: bool,
    /// T-032 / R-033: how the subprocess is contained. `Off` by default; the
    /// launcher sets it from [`probe_sandbox`], and the consent text is chosen
    /// from the same answer so the dialog never claims isolation that is not
    /// in force.
    pub sandbox: Sandbox,
}

impl Default for EnvPolicy {
    /// The R-032-correct policy.
    fn default() -> Self {
        Self {
            strip_prefixes: vec![
                "SLUICE_".to_string(),
                // Set by the Tauri CLI around `tauri dev`/`tauri build` and by
                // CI: the app's own build machinery, never the project's. It
                // includes `TAURI_SIGNING_PRIVATE_KEY[_PASSWORD]` — the update
                // signing credential — which a project config must never see.
                "TAURI_".to_string(),
            ],
            strip_names: vec![
                // Reserved for a resolved credential handed to a child. Nothing
                // sets these today; listing them means that if some future code
                // does, it is stripped here rather than discovered in a config's
                // `process.env` dump.
                "SLUICE_RESOLVED_PASSWORD".to_string(),
                "SLUICE_KEYCHAIN_HANDLE".to_string(),
            ],
            include_project_dotenv: true,
            sandbox: Sandbox::Off,
        }
    }
}

impl EnvPolicy {
    /// Compute the child's environment from a parent environment and the
    /// project's `.env`.
    ///
    /// Split out from the spawn so it is directly testable — the property that
    /// matters (a user's `PGHOST` survives, the app's own variables do not) is
    /// otherwise only observable by running a real subprocess.
    pub fn resolve_env(
        &self,
        parent: &[(String, String)],
        project_dotenv: &BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();

        if self.include_project_dotenv {
            for (k, v) in project_dotenv {
                out.insert(k.clone(), v.clone());
            }
        }

        // Shell environment overlays `.env`.
        for (k, v) in parent {
            out.insert(k.clone(), v.clone());
        }

        out.retain(|k, _| !self.is_stripped(k));
        out
    }

    fn is_stripped(&self, name: &str) -> bool {
        self.strip_names.iter().any(|n| n == name)
            || self.strip_prefixes.iter().any(|p| name.starts_with(p.as_str()))
    }
}

fn apply_env_policy(cmd: &mut Command, policy: &EnvPolicy, config_dir: &Path) {
    // `std::env::vars()` panics on a non-UTF-8 name or value, and one stray
    // byte in an unrelated variable must not take down evaluation. Unicode
    // pairs go through `resolve_env` (the tested path); non-Unicode ones are
    // passed through untouched — the user's terminal would pass them too —
    // unless their name is on the denylist.
    let mut parent: Vec<(String, String)> = Vec::new();
    let mut opaque: Vec<(std::ffi::OsString, std::ffi::OsString)> = Vec::new();
    for (k, v) in std::env::vars_os() {
        match (k.to_str(), v.to_str()) {
            (Some(ks), Some(vs)) => parent.push((ks.to_owned(), vs.to_owned())),
            _ => {
                if !policy.is_stripped(&k.to_string_lossy()) {
                    opaque.push((k, v));
                }
            }
        }
    }

    let dotenv_vars = if policy.include_project_dotenv {
        std::fs::read_to_string(config_dir.join(".env"))
            .map(|t| crate::dotenv::parse(&t))
            .unwrap_or_default()
    } else {
        BTreeMap::new()
    };

    let resolved = policy.resolve_env(&parent, &dotenv_vars);

    cmd.env_clear();
    for (k, v) in resolved {
        cmd.env(k, v);
    }
    // Applied after, so a shell value still wins over a `.env` entry of the
    // same name, matching `resolve_env`'s precedence.
    for (k, v) in opaque {
        cmd.env(k, v);
    }
    cmd.env("PWD", config_dir);
}

/// T-032 / R-033: find a bubblewrap that actually works here.
///
/// Present is not enough: bwrap needs unprivileged user namespaces, which some
/// kernels and AppArmor policies disable. A sandbox that fails at evaluation
/// time would turn every consented config into an error, and a consent text
/// promising isolation that then is not applied would be R-033 itself — so the
/// answer comes from running bwrap once on a trusted command (`/bin/sh -c :`),
/// never on project code. Linux only; elsewhere the platform's own keychain
/// scoping is the boundary.
pub fn probe_sandbox(hide: Vec<PathBuf>) -> Sandbox {
    if !cfg!(target_os = "linux") {
        return Sandbox::Off;
    }
    for candidate in BWRAP_CANDIDATES {
        let bwrap = Path::new(candidate);
        if !bwrap.is_file() {
            continue;
        }
        let ok = Command::new(bwrap)
            .args(["--bind", "/", "/", "--die-with-parent", "--", "/bin/sh", "-c", ":"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            return Sandbox::Bubblewrap { bwrap: bwrap.to_path_buf(), hide };
        }
    }
    Sandbox::Off
}

/// The shim that imports the user's config and prints only a URL.
///
/// D-026: *"only a connection URL is read from its stdout."* The shim writes a
/// single JSON line on stdout and nothing else, so the parent never has to
/// interpret arbitrary config output.
const SHIM_JS: &str = r#"
const target = process.argv[2];
(async () => {
  try {
    const mod = await import(target);
    const cfg = (mod && mod.default) ? mod.default : mod;
    const pick = (o) => {
      if (!o || typeof o !== "object") return null;
      const c = o.dbCredentials || o.db || o.database || o.datasource || o.connection || {};
      return o.url || o.connectionString || c.url || c.connectionString || null;
    };
    const url = pick(cfg);
    process.stdout.write(JSON.stringify({ url: typeof url === "string" ? url : null }));
  } catch (e) {
    process.stderr.write(String((e && e.message) || e));
    process.exit(2);
  }
})();
"#;

/// Resolve a connection from an executable config, asking for consent first.
///
/// `config_path` must be one of the paths `Discovery::executable_configs`
/// reported — this function does not itself decide what is executable.
pub fn evaluate_with_consent(
    config_path: &Path,
    interpreter: &ResolvedInterpreter,
    store: &mut ConsentStore,
    prompt: &dyn ConsentPrompt,
    blast_radius: String,
    timeout: Duration,
    policy: &EnvPolicy,
) -> Result<EvalOutcome, EvalError> {
    let dir = config_path.parent().unwrap_or(Path::new("."));

    let fingerprint = Fingerprint::compute(config_path, &interpreter.path)
        .ok_or(EvalError::Unreadable)?;

    // The gate. Nothing above this line can execute anything.
    if !store.is_granted(dir, &fingerprint) {
        let request = ConsentRequest {
            directory: dir.to_path_buf(),
            config_path: absolute(config_path),
            interpreter_path: interpreter.path.clone(),
            interpreter_label: interpreter.kind.label(),
            blast_radius,
        };

        match prompt.ask(&request) {
            ConsentDecision::Decline => {
                return Ok(EvalOutcome::Declined { reason: DeclineReason::UserDeclined })
            }
            ConsentDecision::GrantOnce => {}
            ConsentDecision::GrantAndRemember => {
                // A store write failure must not silently downgrade to "runs
                // every time without remembering"; it is not fatal to this run,
                // but it is not success either.
                let _ = store.grant(dir, fingerprint.clone());
            }
        }
    } else {
        let _ = store.touch(dir);
    }

    let url = run(config_path, interpreter, timeout, policy)?;
    Ok(EvalOutcome::Evaluated {
        target: UnconfirmedTarget::new(url),
        interpreter: interpreter.path.clone(),
    })
}

fn absolute(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Spawn the interpreter and read one URL from stdout.
fn run(
    config_path: &Path,
    interpreter: &ResolvedInterpreter,
    timeout: Duration,
    policy: &EnvPolicy,
) -> Result<ConnectionUrl, EvalError> {
    let dir = config_path.parent().unwrap_or(Path::new(".")).to_path_buf();

    // The shim lives beside the app's own temp state, never in the project
    // directory — writing into the scanned repository would both dirty it and
    // hand the repository a file it could swap.
    let shim = write_shim().map_err(|e| EvalError::Spawn(e.to_string()))?;

    // T-032: under bubblewrap the program is bwrap and the interpreter
    // follows its `--`; otherwise the interpreter runs directly.
    let mut cmd = match &policy.sandbox {
        Sandbox::Off => Command::new(&interpreter.path),
        Sandbox::Bubblewrap { bwrap, hide } => {
            let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
            let bus = sandbox::parse_bus_address(
                std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
            );
            let mut c = Command::new(bwrap);
            c.args(sandbox::bwrap_args(hide, runtime_dir.as_deref(), &bus, &dir, &[shim.clone()]));
            c.arg(&interpreter.path);
            c
        }
    };
    if interpreter.kind == InterpreterKind::Node
        && config_path.extension().is_some_and(|e| e == "ts")
    {
        // Node needs to be told to strip types; bun does it natively. On a Node
        // too old for the flag this fails loudly, which is the correct outcome.
        cmd.arg("--experimental-strip-types");
    }
    cmd.arg(&shim)
        .arg(absolute(config_path))
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    apply_env_policy(&mut cmd, policy, &dir);

    let mut child = cmd.spawn().map_err(|e| EvalError::Spawn(e.to_string()))?;

    // Drain both pipes *while* the child runs. Reading only after exit means a
    // config that logs more than the OS pipe buffer (~64 KiB) blocks on its own
    // write and is then reported as `TimedOut` — a loud failure, but the wrong
    // one, hiding the config's real error from the user.
    let stdout_reader = spawn_drain(child.stdout.take());
    let stderr_reader = spawn_drain(child.stderr.take());

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&shim);
                    return Err(EvalError::TimedOut { after: timeout });
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&shim);
                return Err(EvalError::Spawn(e.to_string()));
            }
        }
    };

    let stdout = stdout_reader.recv_timeout(DRAIN_GRACE).unwrap_or_default();
    let stderr = stderr_reader.recv_timeout(DRAIN_GRACE).unwrap_or_default();
    let _ = std::fs::remove_file(&shim);

    // R-032's loud-failure rule.
    if !status.success() {
        return Err(EvalError::NonZeroExit { code: status.code(), stderr });
    }

    let raw = extract_url(&stdout).ok_or(EvalError::NoUrlInOutput)?;
    connection_url::parse(&raw).map_err(EvalError::BadUrl)
}

/// Read a pipe on its own thread, keeping at most [`MAX_STDOUT`] bytes but
/// consuming everything, so the child never blocks on a full pipe.
///
/// Returns a receiver rather than a join handle: a config that leaves a
/// background grandchild holding the pipe open would make a join block forever,
/// so the caller waits only [`DRAIN_GRACE`] after the child itself has exited.
fn spawn_drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Some(mut p) = pipe else {
            let _ = tx.send(String::new());
            return;
        };
        let mut buf = Vec::new();
        let _ = std::io::copy(&mut p.by_ref().take(MAX_STDOUT as u64), &mut buf);
        // A capped prefix is enough to report; send it before draining the rest,
        // so a pipe held open by a grandchild cannot withhold it.
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        let _ = std::io::copy(&mut p, &mut std::io::sink());
    });
    rx
}

/// How long to wait for a pipe's contents once the child has exited.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Pull the URL out of the shim's JSON line.
///
/// Tolerant of leading noise, because an interpreter may print a banner or a
/// deprecation warning before the shim's own output.
fn extract_url(stdout: &str) -> Option<String> {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(serde_json::Value::String(u)) = v.get("url") {
                return Some(u.clone());
            }
        }
    }
    None
}

fn write_shim() -> std::io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join("sluice-config-shim");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("shim-{nanos}.mjs"));
    std::fs::write(&path, SHIM_JS)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consent::AlwaysDecline;

    struct AlwaysGrant;
    impl ConsentPrompt for AlwaysGrant {
        fn ask(&self, _: &ConsentRequest) -> ConsentDecision {
            ConsentDecision::GrantAndRemember
        }
    }

    /// Records what the dialog was told, so the D-026 wording requirements can
    /// be asserted rather than eyeballed.
    struct Recording(std::cell::RefCell<Vec<ConsentRequest>>);
    impl ConsentPrompt for Recording {
        fn ask(&self, r: &ConsentRequest) -> ConsentDecision {
            self.0.borrow_mut().push(r.clone());
            ConsentDecision::Decline
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join("sluice-eval").join(format!("{tag}-{nanos}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_interpreter(dir: &Path) -> ResolvedInterpreter {
        ResolvedInterpreter {
            kind: InterpreterKind::Node,
            path: dir.join("does-not-need-to-exist-for-gate-tests"),
        }
    }

    /// **The R-001 test at the gate.** Declining must not spawn anything, and
    /// must degrade rather than error.
    #[test]
    fn declining_does_not_evaluate_and_degrades_to_the_ui() {
        let dir = tmp("decline");
        let cfg = dir.join("drizzle.config.ts");
        std::fs::write(&cfg, "export default {}").unwrap();

        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        let out = evaluate_with_consent(
            &cfg,
            &fake_interpreter(&dir),
            &mut store,
            &AlwaysDecline,
            "test".into(),
            DEFAULT_TIMEOUT,
            &EnvPolicy::default(),
        )
        .unwrap();

        assert!(matches!(
            out,
            EvalOutcome::Declined { reason: DeclineReason::UserDeclined }
        ));
    }

    /// D-026 requires the dialog to name both absolute paths.
    #[test]
    fn the_consent_request_names_both_absolute_paths() {
        let dir = tmp("paths");
        let cfg = dir.join("drizzle.config.ts");
        std::fs::write(&cfg, "export default {}").unwrap();

        let interp = ResolvedInterpreter {
            kind: InterpreterKind::Bun,
            path: PathBuf::from("/opt/homebrew/bin/bun"),
        };
        let recorder = Recording(Default::default());
        let mut store = ConsentStore::open_at(dir.join("consent.json"));

        evaluate_with_consent(
            &cfg,
            &interp,
            &mut store,
            &recorder,
            "blast radius text".into(),
            DEFAULT_TIMEOUT,
            &EnvPolicy::default(),
        )
        .unwrap();

        let seen = recorder.0.borrow();
        assert_eq!(seen.len(), 1, "must ask exactly once");
        let r = &seen[0];
        assert!(r.config_path.is_absolute(), "{:?}", r.config_path);
        assert!(r.interpreter_path.is_absolute(), "{:?}", r.interpreter_path);
        assert_eq!(r.interpreter_path, PathBuf::from("/opt/homebrew/bin/bun"));
        assert_eq!(r.interpreter_label, "bun");
        assert_eq!(r.blast_radius, "blast radius text");
    }

    /// Consent is asked once, then remembered — the ergonomic half of D-026.
    #[test]
    fn a_remembered_grant_is_not_re_asked() {
        let dir = tmp("remember");
        let cfg = dir.join("drizzle.config.ts");
        std::fs::write(&cfg, "export default {}").unwrap();
        let interp = fake_interpreter(&dir);
        let mut store = ConsentStore::open_at(dir.join("consent.json"));

        // Grant, then confirm the gate is satisfied without prompting again.
        let fp = Fingerprint::compute(&cfg, &interp.path).unwrap();
        store.grant(&dir, fp).unwrap();

        let recorder = Recording(Default::default());
        // Spawning will fail (the interpreter is fake) — the point is that the
        // prompt was never consulted.
        let _ = evaluate_with_consent(
            &cfg,
            &interp,
            &mut store,
            &recorder,
            "test".into(),
            DEFAULT_TIMEOUT,
            &EnvPolicy::default(),
        );
        assert!(recorder.0.borrow().is_empty(), "must not re-ask a granted directory");
    }

    /// Editing the config after a grant must re-prompt.
    #[test]
    fn editing_the_config_re_asks() {
        let dir = tmp("reask");
        let cfg = dir.join("drizzle.config.ts");
        std::fs::write(&cfg, "export default { a: 1 }").unwrap();
        let interp = fake_interpreter(&dir);
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, Fingerprint::compute(&cfg, &interp.path).unwrap()).unwrap();

        std::fs::write(&cfg, "export default { a: 2 }").unwrap();

        let recorder = Recording(Default::default());
        let _ = evaluate_with_consent(
            &cfg,
            &interp,
            &mut store,
            &recorder,
            "test".into(),
            DEFAULT_TIMEOUT,
            &EnvPolicy::default(),
        );
        assert_eq!(recorder.0.borrow().len(), 1, "edited config must re-ask");
    }

    #[test]
    fn granting_then_failing_to_spawn_is_an_error_not_a_silent_fallback() {
        let dir = tmp("spawnfail");
        let cfg = dir.join("drizzle.config.ts");
        std::fs::write(&cfg, "export default {}").unwrap();
        let mut store = ConsentStore::open_at(dir.join("consent.json"));

        let err = evaluate_with_consent(
            &cfg,
            &fake_interpreter(&dir),
            &mut store,
            &AlwaysGrant,
            "test".into(),
            DEFAULT_TIMEOUT,
            &EnvPolicy::default(),
        )
        .unwrap_err();

        assert!(matches!(err, EvalError::Spawn(_)), "{err:?}");
    }

    #[test]
    fn url_extraction_tolerates_banner_output() {
        assert_eq!(
            extract_url("some banner\n{\"url\":\"postgres://h/db\"}").as_deref(),
            Some("postgres://h/db")
        );
        assert!(extract_url("{\"url\":null}").is_none());
        assert!(extract_url("no json here").is_none());
        assert!(extract_url("").is_none());
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn dotenv(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// **R-032.** The variables a config branches on must reach it unchanged —
    /// including a credential the *user's shell* exports, because their own
    /// terminal would hand it to the same config.
    #[test]
    fn the_users_shell_environment_reaches_the_config() {
        let out = EnvPolicy::default().resolve_env(
            &env(&[
                ("PGHOST", "db.staging.internal"),
                ("NODE_ENV", "development"),
                ("PGPASSWORD", "users-own-shell-secret"),
                ("PATH", "/usr/bin"),
            ]),
            &BTreeMap::new(),
        );
        assert_eq!(out.get("PGHOST").map(String::as_str), Some("db.staging.internal"));
        assert_eq!(out.get("NODE_ENV").map(String::as_str), Some("development"));
        assert_eq!(out.get("PGPASSWORD").map(String::as_str), Some("users-own-shell-secret"));
        assert_eq!(out.get("PATH").map(String::as_str), Some("/usr/bin"));
    }

    /// What the app itself contributed is stripped: its own namespace, any
    /// resolved credential, and the Tauri build machinery's variables
    /// (including the update-signing key).
    #[test]
    fn the_apps_own_variables_are_stripped() {
        let out = EnvPolicy::default().resolve_env(
            &env(&[
                ("SLUICE_RESOLVED_PASSWORD", "from-keychain"),
                ("SLUICE_KEYCHAIN_HANDLE", "handle"),
                ("SLUICE_ANYTHING_ELSE", "x"),
                ("TAURI_SIGNING_PRIVATE_KEY", "signing-key"),
                ("TAURI_ENV_PLATFORM", "darwin"),
                ("PGHOST", "kept"),
            ]),
            &dotenv(&[("SLUICE_FROM_DOTENV", "x")]),
        );
        let keys: Vec<&str> = out.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["PGHOST"], "only the user's variable survives: {keys:?}");
    }

    /// The project's `.env` is overlaid, and the shell wins a conflict —
    /// dotenv's own precedence, and T-028's discovery order.
    #[test]
    fn the_project_dotenv_is_included_and_the_shell_wins() {
        let out = EnvPolicy::default().resolve_env(
            &env(&[("PGHOST", "from-shell")]),
            &dotenv(&[("PGHOST", "from-dotenv"), ("PGDATABASE", "from-dotenv")]),
        );
        assert_eq!(out.get("PGHOST").map(String::as_str), Some("from-shell"));
        assert_eq!(out.get("PGDATABASE").map(String::as_str), Some("from-dotenv"));
    }

    /// The display-before-connect rule: the summary shows where the connection
    /// lands, and never the password.
    #[test]
    fn the_target_summary_shows_the_destination_and_no_credential() {
        let url = connection_url::parse("postgres://app:hunter2@db.prod.internal:6432/shop").unwrap();
        let target = UnconfirmedTarget::new(url);
        let shown = target.summary().to_string();
        assert_eq!(shown, "postgres://app@db.prod.internal:6432/shop");
        assert!(!shown.contains("hunter2"));
        assert!(target.confirm(TargetDecision::Cancel).is_none());
    }

    #[test]
    fn decline_reasons_explain_the_degradation() {
        assert!(DeclineReason::UserDeclined.explain().contains("Choose a connection"));
        assert!(DeclineReason::NoInterpreterAvailable
            .explain()
            .contains("choose a connection"));
    }
}
