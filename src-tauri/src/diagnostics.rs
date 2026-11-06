//! Shape-only logging, crash files, and the copy-diagnostics preview
//! (T-023 / D-020).
//!
//! D-020's tension: logging everything makes the log the most sensitive file on
//! disk; logging nothing makes intermittent driver bugs undiagnosable. The
//! resolution here is that the log records a query's **shape** — kind,
//! duration, row count, error kind, profile id — and never its values.
//!
//! That is enforced by the API rather than by care at call sites:
//! [`query_event`] is the only function in the app that emits a query event,
//! and its parameters are shape-typed — a `&'static str` kind, a row count, an
//! [`ErrorKind`]. There is no argument through which a parameter value, a
//! result row, or an error *message* (which can quote row data, e.g. a
//! duplicate-key value) could reach the log. A test forbids `tracing` macros
//! anywhere else in this crate, so a new call site cannot bypass it.
//!
//! The one exception is opt-in and per run: full query text is logged only
//! while "verbose diagnostics" is switched on. It starts off on every launch.
//!
//! Nothing here is ever uploaded. "Copy diagnostics" builds a text block the
//! UI shows in full; what is copied is exactly what was shown.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use sluice_core::{DriverError, ErrorKind};

/// Per log file; three files are kept, so at most ~20 MB on disk.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
pub const KEEP_LOG_FILES: usize = 3;
/// Crash files kept; older ones are pruned.
pub const KEEP_CRASH_FILES: usize = 5;
/// Log lines included in a diagnostics preview.
const PREVIEW_LOG_LINES: usize = 200;

static VERBOSE: AtomicBool = AtomicBool::new(false);
static DIRS: OnceLock<DiagnosticsDirs> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct DiagnosticsDirs {
    pub logs: PathBuf,
    pub crashes: PathBuf,
}

impl DiagnosticsDirs {
    /// `<cache dir>/sluice/{logs,crashes}` — the cache dir, because D-014 puts
    /// disposable state there and these are safe to delete.
    pub fn default_location() -> Option<Self> {
        let base = dirs::cache_dir()?.join("sluice");
        Some(Self { logs: base.join("logs"), crashes: base.join("crashes") })
    }
}

/// Install the subscriber and the panic hook. Called once at startup; a
/// failure to set up logging must never stop the app from starting.
pub fn init() {
    let Some(dirs) = DiagnosticsDirs::default_location() else { return };
    let _ = fs::create_dir_all(&dirs.logs);
    let _ = fs::create_dir_all(&dirs.crashes);

    if let Ok(writer) = SizeRotatingFile::open(dirs.logs.join("sluice.log"), MAX_LOG_BYTES, KEEP_LOG_FILES) {
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(Mutex::new(writer))
            .with_target(false)
            .with_current_span(false)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    }

    install_panic_hook(dirs.crashes.clone());
    let _ = DIRS.set(dirs);
}

pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::SeqCst);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::SeqCst)
}

/// The profile a session belongs to: session ids are `<profile>::<token>`.
pub fn profile_of(session_id: &str) -> &str {
    session_id.split("::").next().unwrap_or(session_id)
}

/// Emit one query event. The only query-logging entry point in the app.
///
/// `text` is logged **only** while verbose diagnostics is on; otherwise it is
/// dropped here, before it can reach any formatter.
pub fn query_event(
    kind: &'static str,
    profile: &str,
    started: Instant,
    rows: Option<usize>,
    error: Option<ErrorKind>,
    text: Option<&str>,
) {
    let duration_ms = started.elapsed().as_millis() as u64;
    let rows = rows.map(|r| r as u64);
    let error_kind = error.map(error_kind_label);
    match (verbose().then_some(text).flatten(), error_kind) {
        (Some(text), Some(e)) => {
            tracing::warn!(kind, profile, duration_ms, rows, error_kind = e, query_text = text, "query")
        }
        (Some(text), None) => {
            tracing::info!(kind, profile, duration_ms, rows, query_text = text, "query")
        }
        (None, Some(e)) => tracing::warn!(kind, profile, duration_ms, rows, error_kind = e, "query"),
        (None, None) => tracing::info!(kind, profile, duration_ms, rows, "query"),
    }
}

/// Run a driver call and log its shape. `rows` reads the row count off a
/// success; `text` is the verbose-only query text.
pub async fn logged<T>(
    kind: &'static str,
    session_id: &str,
    text: Option<&str>,
    rows: impl FnOnce(&T) -> Option<usize>,
    call: impl std::future::Future<Output = Result<T, DriverError>>,
) -> Result<T, DriverError> {
    let started = Instant::now();
    let result = call.await;
    let (n, err) = match &result {
        Ok(v) => (rows(v), None),
        Err(e) => (None, Some(e.kind)),
    };
    query_event(kind, profile_of(session_id), started, n, err, text);
    result
}

fn error_kind_label(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::ConnectionFailed => "connection_failed",
        ErrorKind::AuthFailed => "auth_failed",
        ErrorKind::Timeout => "timeout",
        ErrorKind::Cancelled => "cancelled",
        ErrorKind::SyntaxError => "syntax_error",
        ErrorKind::ConstraintViolation => "constraint_violation",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::NotFound => "not_found",
        ErrorKind::Conflict => "conflict",
        ErrorKind::DriverBug => "driver_bug",
        ErrorKind::Unknown => "unknown",
    }
}

// ---------------------------------------------------------------- crashes

fn install_panic_hook(crash_dir: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // T-024: a panic message can quote a connection string. No session's
        // password is known here, so credential *shapes* are redacted before
        // the message reaches the crash file or the shape log.
        let message = sluice_core::redact_shapes(&panic_message(info.payload()));
        let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let report = crash_report(&message, location.as_deref(), &backtrace);
        let _ = write_crash_file(&crash_dir, &report);
        // The location is code, not data, so it is safe in the shape log.
        tracing::error!(kind = "panic", location = location.as_deref().unwrap_or("unknown"), "panic");
        // T-006 still turns a driver panic into an error; this only records it.
        previous(info);
    }));
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic payload)".to_string()
    }
}

/// The crash file's content. Local only; the user sees it in full before it is
/// ever copied anywhere.
pub fn crash_report(message: &str, location: Option<&str>, backtrace: &str) -> String {
    format!(
        "Sluice {version} crash report\n\
         platform: {os}/{arch}\n\
         thread: {thread}\n\
         location: {location}\n\
         message: {message}\n\n\
         backtrace:\n{backtrace}\n",
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        thread = std::thread::current().name().unwrap_or("unnamed"),
        location = location.unwrap_or("unknown"),
    )
}

pub fn write_crash_file(dir: &Path, report: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("crash-{millis:013}.txt"));
    fs::write(&path, report)?;
    prune(dir, "crash-", KEEP_CRASH_FILES);
    Ok(path)
}

/// Keep the newest `keep` files starting with `prefix` (names sort by time).
fn prune(dir: &Path, prefix: &str, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut names: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(prefix)))
        .collect();
    names.sort();
    let excess = names.len().saturating_sub(keep);
    for old in names.into_iter().take(excess) {
        let _ = fs::remove_file(old);
    }
}

// ---------------------------------------------------------------- preview

/// The exact text "copy diagnostics" will copy.
pub fn preview() -> String {
    match DIRS.get() {
        Some(d) => preview_from(d, verbose()),
        None => preview_text(None, &[], verbose()),
    }
}

pub fn preview_from(dirs: &DiagnosticsDirs, verbose: bool) -> String {
    let crash = latest(&dirs.crashes, "crash-").and_then(|p| fs::read_to_string(p).ok());
    let log = fs::read_to_string(dirs.logs.join("sluice.log")).unwrap_or_default();
    let tail: Vec<&str> = {
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(PREVIEW_LOG_LINES)..].to_vec()
    };
    preview_text(crash.as_deref(), &tail, verbose)
}

fn preview_text(crash: Option<&str>, log_tail: &[&str], verbose: bool) -> String {
    let mut out = format!(
        "Sluice {} on {}/{}\nverbose diagnostics: {}\n\n== latest crash ==\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        if verbose { "ON — query text below may include sensitive values" } else { "off" },
    );
    out.push_str(crash.unwrap_or("(none)\n"));
    out.push_str(&format!("\n== recent log ({} lines) ==\n", log_tail.len()));
    for l in log_tail {
        out.push_str(l);
        out.push('\n');
    }
    out
}

fn latest(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let mut names: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(prefix)))
        .collect();
    names.sort();
    names.pop()
}

// ---------------------------------------------------------------- rotation

/// A log file that rotates by **size**: past `max_bytes` the current file
/// becomes `.1`, `.1` becomes `.2`, and so on, keeping `keep` files in total.
pub struct SizeRotatingFile {
    path: PathBuf,
    max_bytes: u64,
    keep: usize,
    file: File,
    size: u64,
}

impl SizeRotatingFile {
    pub fn open(path: PathBuf, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self { path, max_bytes, keep: keep.max(1), file, size })
    }

    fn rotated(&self, n: usize) -> PathBuf {
        PathBuf::from(format!("{}.{n}", self.path.display()))
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        // Oldest falls off; the rest shift up one.
        let _ = fs::remove_file(self.rotated(self.keep - 1));
        for n in (1..self.keep - 1).rev() {
            let _ = fs::rename(self.rotated(n), self.rotated(n + 1));
        }
        if self.keep > 1 {
            fs::rename(&self.path, self.rotated(1))?;
        } else {
            fs::remove_file(&self.path)?;
        }
        self.file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.size = 0;
        Ok(())
    }
}

impl Write for SizeRotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let n = self.file.write(buf)?;
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn tmp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join("sluice-diag").join(format!("{tag}-{nanos}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A writer the test can read back.
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn capture(f: impl FnOnce()) -> String {
        let buf = Buf::default();
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        buf.text()
    }

    /// Tests that flip the process-wide verbose flag must not interleave.
    static VERBOSE_LOCK: Mutex<()> = Mutex::new(());

    /// D-020's default: shape fields present, query text absent.
    #[test]
    fn a_query_event_logs_shape_only_by_default() {
        let _g = VERBOSE_LOCK.lock().unwrap();
        set_verbose(false);
        let out = capture(|| {
            query_event(
                "script",
                "prod-ro",
                Instant::now(),
                Some(42),
                None,
                Some("select * from users where ssn = '123-45-6789'"),
            )
        });
        for field in ["\"kind\":\"script\"", "\"profile\":\"prod-ro\"", "\"rows\":42", "duration_ms"] {
            assert!(out.contains(field), "missing {field}: {out}");
        }
        assert!(!out.contains("123-45-6789"), "query text leaked with verbose off: {out}");
        assert!(!out.contains("query_text"), "{out}");
    }

    #[test]
    fn query_text_appears_only_while_verbose_is_on() {
        let _g = VERBOSE_LOCK.lock().unwrap();
        set_verbose(true);
        let on = capture(|| query_event("script", "p", Instant::now(), None, None, Some("select 1")));
        set_verbose(false);
        let off = capture(|| query_event("script", "p", Instant::now(), None, None, Some("select 1")));
        assert!(on.contains("\"query_text\":\"select 1\""), "{on}");
        assert!(!off.contains("select 1"), "{off}");
    }

    /// Errors log their kind, never their message — a message can quote data
    /// (`Key (email)=(a@b.c) already exists`).
    #[tokio::test]
    async fn a_failed_call_logs_the_error_kind_not_its_message() {
        let out = {
            let buf = Buf::default();
            let sink = buf.clone();
            let subscriber = tracing_subscriber::fmt().json().with_writer(move || sink.clone()).finish();
            let _d = tracing::subscriber::set_default(subscriber);
            let _ = logged::<()>("browse", "shop::abc", None, |_| None, async {
                Err(DriverError::new(ErrorKind::ConstraintViolation, "Key (email)=(a@b.c) already exists"))
            })
            .await;
            buf.text()
        };
        assert!(out.contains("\"error_kind\":\"constraint_violation\""), "{out}");
        assert!(out.contains("\"profile\":\"shop\""), "session token stripped to the profile: {out}");
        assert!(!out.contains("a@b.c"), "error message leaked: {out}");
    }

    #[test]
    fn the_log_rotates_by_size_and_keeps_a_bounded_number_of_files() {
        let dir = tmp("rotate");
        let path = dir.join("sluice.log");
        let mut w = SizeRotatingFile::open(path.clone(), 100, 3).unwrap();
        for i in 0..50 {
            writeln!(w, "line {i:04} xxxxxxxxxxxxxxxxxxxx").unwrap();
        }
        w.flush().unwrap();
        let mut files: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files, vec!["sluice.log", "sluice.log.1", "sluice.log.2"], "{files:?}");
        for f in &files {
            let len = fs::metadata(dir.join(f)).unwrap().len();
            assert!(len <= 100, "{f} is {len} bytes, over the cap");
        }
        // The newest line is in the live file.
        assert!(fs::read_to_string(&path).unwrap().contains("line 0049"));
    }

    #[test]
    fn crash_files_carry_the_report_and_are_pruned() {
        let dir = tmp("crash");
        for i in 0..(KEEP_CRASH_FILES + 3) {
            write_crash_file(&dir, &crash_report(&format!("boom {i}"), Some("src/x.rs:1:1"), "bt")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let count = fs::read_dir(&dir).unwrap().count();
        assert_eq!(count, KEEP_CRASH_FILES);
        let newest = fs::read_to_string(latest(&dir, "crash-").unwrap()).unwrap();
        assert!(newest.contains(&format!("boom {}", KEEP_CRASH_FILES + 2)), "{newest}");
        assert!(newest.contains("location: src/x.rs:1:1"));
        assert!(newest.contains("backtrace:"));
    }

    /// The preview is the whole payload: crash file, recent log, and whether
    /// verbose mode (which can capture query text) was on.
    #[test]
    fn the_preview_contains_exactly_the_crash_and_the_log_tail() {
        let base = tmp("preview");
        let dirs = DiagnosticsDirs { logs: base.join("logs"), crashes: base.join("crashes") };
        fs::create_dir_all(&dirs.logs).unwrap();
        let log: String = (0..300).map(|i| format!("{{\"n\":{i}}}\n")).collect();
        fs::write(dirs.logs.join("sluice.log"), log).unwrap();
        write_crash_file(&dirs.crashes, "CRASH BODY\n").unwrap();

        let text = preview_from(&dirs, false);
        assert!(text.contains("CRASH BODY"));
        assert!(text.contains("verbose diagnostics: off"));
        assert!(text.contains("== recent log (200 lines) =="));
        assert!(!text.contains("{\"n\":99}"), "older lines beyond the tail are excluded");
        assert!(text.contains("{\"n\":299}"));

        assert!(preview_from(&dirs, true).contains("may include sensitive values"));
    }

    /// The structural guard: no other module in this crate may log. A new
    /// `tracing::info!(…, value = row)` elsewhere would bypass the shape-only
    /// API, so it fails here instead of in a user's log file.
    #[test]
    fn only_this_module_emits_log_events() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        for entry in fs::read_dir(&src).unwrap().flatten() {
            let p = entry.path();
            if p.extension().is_none_or(|e| e != "rs") || p.file_name().is_some_and(|n| n == "diagnostics.rs") {
                continue;
            }
            let text = fs::read_to_string(&p).unwrap();
            for marker in ["tracing::", "info!(", "warn!(", "error!(", "debug!(", "trace!(", "event!("] {
                if text.contains(marker) {
                    offenders.push(format!("{}: {marker}", p.display()));
                }
            }
        }
        assert!(offenders.is_empty(), "log events outside diagnostics.rs: {offenders:?}");
    }
}
