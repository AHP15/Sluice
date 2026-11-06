//! Per-directory consent for evaluating executable configs (T-029 / D-026, R-001).
//!
//! D-026: *"the app never evaluates one without explicit user consent for that
//! specific directory ... Consent is remembered per canonical directory path in
//! the config store and is invalidated when the config file's content hash
//! changes."*
//!
//! ## What consent is keyed to
//!
//! A [`Fingerprint`] — not just the directory. Granting consent means "I approve
//! *this code*", so if the code changes the grant must not carry over. T-029
//! fingerprints the config's own hash plus the resolved interpreter path.
//!
//! R-031 is the recorded knowledge that this is **too narrow**: a config's real
//! executable surface is its transitive import graph, which changes constantly
//! while the config file stays byte-identical. T-030 broadens the fingerprint to
//! cover lockfiles and directly-imported local files. The struct is versioned
//! (`fingerprint_version`) so a broadened definition invalidates every existing
//! grant rather than silently accepting one computed the old way — an
//! unversioned fingerprint would let stale trust survive the very change that
//! was meant to revoke it.
//!
//! This module hashes and stores. It never executes anything, which is why it is
//! classified inert in `tests/no_subprocess.rs`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Bumped whenever the *definition* of a fingerprint changes, so that
/// broadening it (T-030) invalidates existing grants instead of honouring them.
///
/// - **1** (T-029): config hash + interpreter path.
/// - **2** (T-030 / R-031): plus every governing lockfile and every local file
///   the config imports directly. Every v1 grant re-prompts once.
pub const FINGERPRINT_VERSION: u32 = 2;

/// T-030 / R-031: a grant not used for this long expires and re-prompts, so
/// trust given once and forgotten does not stand indefinitely.
pub const CONSENT_DISUSE_TTL_SECS: u64 = 90 * 24 * 60 * 60;

/// Hash some bytes into the short hex form stored in the consent file.
pub fn hash_bytes(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Hash a file's contents, or `None` if unreadable.
pub fn hash_file(path: &Path) -> Option<String> {
    std::fs::read(path).ok().as_deref().map(hash_bytes)
}

/// What a grant is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub fingerprint_version: u32,
    /// SHA-256 of the config file's bytes.
    pub config_hash: String,
    /// The exact interpreter that will run it. R-031 requires a re-prompt when
    /// this changes: approving `node` is not approving `bun`, and approving a
    /// system binary is not approving a replacement at the same path.
    pub interpreter_path: PathBuf,
    /// T-030: hashes of the governing lockfiles (`lockfile:<path>`) and of each
    /// local file the config imports directly (`import:<specifier>`; an
    /// unresolvable one is `unresolved`, so its later appearance re-prompts).
    /// See [`crate::imports`].
    #[serde(default)]
    pub additional_hashes: BTreeMap<String, String>,
}

impl Fingerprint {
    /// Compute the fingerprint of what is about to run: the config, its
    /// lockfiles and direct local imports (T-030), and the interpreter.
    pub fn compute(config_path: &Path, interpreter_path: &Path) -> Option<Self> {
        Some(Self {
            fingerprint_version: FINGERPRINT_VERSION,
            config_hash: hash_file(config_path)?,
            interpreter_path: interpreter_path.to_path_buf(),
            additional_hashes: crate::imports::extra_hashes(config_path),
        })
    }

    /// Whether a stored grant covers this fingerprint.
    ///
    /// Deliberately an exact comparison including `fingerprint_version`: a grant
    /// recorded under an older, narrower definition does not satisfy a newer one.
    pub fn matches(&self, other: &Fingerprint) -> bool {
        self == other
    }
}

/// A recorded grant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsentRecord {
    pub fingerprint: Fingerprint,
    /// Unix seconds. Used by T-030's disuse TTL.
    pub granted_at: u64,
    pub last_used_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ConsentFile {
    #[serde(default)]
    records: BTreeMap<String, ConsentRecord>,
}

/// What the user decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentDecision {
    /// Evaluate, and remember for this directory + fingerprint.
    GrantAndRemember,
    /// Evaluate exactly once; do not persist.
    GrantOnce,
    /// Do not evaluate. D-026 degrades to choosing a connection in the UI.
    Decline,
}

/// Everything the dialog must state. D-026 requires the config file's and the
/// interpreter's **absolute** paths.
#[derive(Debug, Clone)]
pub struct ConsentRequest {
    pub directory: PathBuf,
    pub config_path: PathBuf,
    pub interpreter_path: PathBuf,
    pub interpreter_label: &'static str,
    /// Platform-accurate statement of what the code will be able to reach.
    /// T-032 (R-033) owns the wording; it differs materially on Linux.
    pub blast_radius: String,
}

/// Asks the user. Injected so the policy in this crate stays testable and free
/// of any UI dependency — `src-tauri` supplies the real dialog.
pub trait ConsentPrompt {
    fn ask(&self, request: &ConsentRequest) -> ConsentDecision;
}

/// Always declines. The correct default for any headless or non-interactive
/// context: no prompt is possible, so no code runs.
pub struct AlwaysDecline;

impl ConsentPrompt for AlwaysDecline {
    fn ask(&self, _: &ConsentRequest) -> ConsentDecision {
        ConsentDecision::Decline
    }
}

/// Persistent per-directory consent.
pub struct ConsentStore {
    path: PathBuf,
    file: ConsentFile,
}

impl ConsentStore {
    /// Open (or start) the store at an explicit path.
    pub fn open_at(path: PathBuf) -> Self {
        let file = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<ConsentFile>(&t).ok())
            .unwrap_or_default();
        Self { path, file }
    }

    /// The default location under the app's data dir.
    ///
    /// Self-contained rather than part of the app state store, because T-033
    /// (three-store app state with versioning and atomic writes) is Phase 2 and
    /// this is needed now. T-033 should absorb it; the format is a plain map so
    /// that migration is mechanical.
    pub fn default_path(data_dir: &Path) -> PathBuf {
        data_dir.join("sluice").join("consent.json")
    }

    /// Canonical directory key. Canonicalized so `.`, `..`, symlinked, and
    /// trailing-slash spellings of one directory cannot each hold their own
    /// grant — otherwise "remember this directory" is trivially bypassed.
    fn key(dir: &Path) -> String {
        dir.canonicalize()
            .unwrap_or_else(|_| dir.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }

    /// Whether a live grant covers this directory and fingerprint.
    pub fn is_granted(&self, dir: &Path, fp: &Fingerprint) -> bool {
        self.is_granted_at(dir, fp, now_secs())
    }

    /// [`is_granted`](Self::is_granted) at an explicit time, for the TTL.
    ///
    /// A grant is live when its fingerprint matches **and** it was last used
    /// within [`CONSENT_DISUSE_TTL_SECS`]. Use is what refreshes it — every
    /// evaluation `touch`es the record — so a project in daily use never
    /// re-prompts for age, and one untouched for a quarter does.
    pub fn is_granted_at(&self, dir: &Path, fp: &Fingerprint, now: u64) -> bool {
        self.file.records.get(&Self::key(dir)).is_some_and(|r| {
            r.fingerprint.matches(fp) && now.saturating_sub(r.last_used_at) <= CONSENT_DISUSE_TTL_SECS
        })
    }

    pub fn record(&self, dir: &Path) -> Option<&ConsentRecord> {
        self.file.records.get(&Self::key(dir))
    }

    /// Persist a grant.
    pub fn grant(&mut self, dir: &Path, fp: Fingerprint) -> std::io::Result<()> {
        let now = now_secs();
        self.file.records.insert(
            Self::key(dir),
            ConsentRecord { fingerprint: fp, granted_at: now, last_used_at: now },
        );
        self.save()
    }

    /// Drop a directory's grant.
    pub fn revoke(&mut self, dir: &Path) -> std::io::Result<()> {
        self.file.records.remove(&Self::key(dir));
        self.save()
    }

    pub fn touch(&mut self, dir: &Path) -> std::io::Result<()> {
        if let Some(r) = self.file.records.get_mut(&Self::key(dir)) {
            r.last_used_at = now_secs();
            return self.save();
        }
        Ok(())
    }

    /// Write via a temp file and rename, so an interrupted write cannot leave a
    /// truncated consent file — which would read as "no grants" and re-prompt,
    /// or worse, fail to parse and be silently replaced.
    fn save(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(&self.file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &self.path)
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let d = std::env::temp_dir().join("sluice-consent").join(format!("{tag}-{nanos}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn fixture(tag: &str, body: &str) -> (PathBuf, PathBuf, Fingerprint) {
        let dir = tmp(tag);
        let cfg = dir.join("drizzle.config.ts");
        fs::write(&cfg, body).unwrap();
        let interp = PathBuf::from("/usr/local/bin/bun");
        let fp = Fingerprint::compute(&cfg, &interp).unwrap();
        (dir, cfg, fp)
    }

    /// R-031: `npm install` changes what the config loads without touching
    /// the config. A lockfile change must re-prompt.
    #[test]
    fn a_lockfile_change_invalidates_the_grant() {
        let (dir, cfg, _) = fixture("lock", "export default {}");
        fs::write(dir.join("package-lock.json"), r#"{"v":1}"#).unwrap();
        let interp = PathBuf::from("/usr/local/bin/bun");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, Fingerprint::compute(&cfg, &interp).unwrap()).unwrap();
        assert!(store.is_granted(&dir, &Fingerprint::compute(&cfg, &interp).unwrap()));

        fs::write(dir.join("package-lock.json"), r#"{"v":2,"evil":"dep"}"#).unwrap();
        assert!(!store.is_granted(&dir, &Fingerprint::compute(&cfg, &interp).unwrap()));
    }

    /// R-031: the config is byte-identical, but a file it imports changed.
    #[test]
    fn a_change_to_a_directly_imported_local_file_invalidates_the_grant() {
        let (dir, cfg, _) = fixture("import", r#"import { url } from "./env"; export default { url };"#);
        fs::write(dir.join("env.ts"), r#"export const url = "postgres://h/db";"#).unwrap();
        let interp = PathBuf::from("/usr/local/bin/bun");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, Fingerprint::compute(&cfg, &interp).unwrap()).unwrap();

        fs::write(dir.join("env.ts"), r#"import "child_process"; export const url = "x";"#).unwrap();
        assert!(!store.is_granted(&dir, &Fingerprint::compute(&cfg, &interp).unwrap()));
    }

    /// An import that did not resolve at grant time must not be a blind spot:
    /// the file appearing later re-prompts.
    #[test]
    fn a_previously_missing_import_appearing_invalidates_the_grant() {
        let (dir, cfg, _) = fixture("appear", r#"import "./later";"#);
        let interp = PathBuf::from("/usr/local/bin/bun");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, Fingerprint::compute(&cfg, &interp).unwrap()).unwrap();

        fs::write(dir.join("later.js"), "process.exit(0)").unwrap();
        assert!(!store.is_granted(&dir, &Fingerprint::compute(&cfg, &interp).unwrap()));
    }

    /// Editing unrelated application code must NOT re-prompt — the scan is
    /// shallow on purpose (consent fatigue erodes the guard).
    #[test]
    fn unrelated_files_do_not_invalidate_the_grant() {
        let (dir, cfg, _) = fixture("unrelated", r#"export default { schema: "./src/schema.ts" };"#);
        fs::create_dir(dir.join("src")).unwrap();
        fs::write(dir.join("src/schema.ts"), "v1").unwrap();
        let interp = PathBuf::from("/usr/local/bin/bun");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, Fingerprint::compute(&cfg, &interp).unwrap()).unwrap();

        fs::write(dir.join("src/schema.ts"), "v2").unwrap();
        fs::write(dir.join("README.md"), "docs").unwrap();
        assert!(store.is_granted(&dir, &Fingerprint::compute(&cfg, &interp).unwrap()));
    }

    #[test]
    fn a_grant_expires_after_the_disuse_ttl_and_use_refreshes_it() {
        let (dir, _cfg, fp) = fixture("ttl", "export default {}");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, fp.clone()).unwrap();
        let used = store.record(&dir).unwrap().last_used_at;

        assert!(store.is_granted_at(&dir, &fp, used + CONSENT_DISUSE_TTL_SECS));
        assert!(!store.is_granted_at(&dir, &fp, used + CONSENT_DISUSE_TTL_SECS + 1));

        // A clock behind the record (e.g. adjusted) does not expire it.
        assert!(store.is_granted_at(&dir, &fp, used.saturating_sub(10)));
    }

    #[test]
    fn a_version_1_grant_does_not_satisfy_the_broadened_fingerprint() {
        let (dir, _cfg, fp) = fixture("v1", "export default {}");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        let mut old = fp.clone();
        old.fingerprint_version = 1;
        old.additional_hashes.clear();
        store.grant(&dir, old).unwrap();
        assert!(!store.is_granted(&dir, &fp));
    }

    #[test]
    fn a_grant_is_remembered_for_the_same_fingerprint() {
        let (dir, _cfg, fp) = fixture("grant", "export default {}");
        let store_path = dir.join("consent.json");

        let mut store = ConsentStore::open_at(store_path.clone());
        assert!(!store.is_granted(&dir, &fp));
        store.grant(&dir, fp.clone()).unwrap();
        assert!(store.is_granted(&dir, &fp));

        // Survives a reopen.
        let reopened = ConsentStore::open_at(store_path);
        assert!(reopened.is_granted(&dir, &fp));
    }

    /// D-026's invalidation rule, and the core of why consent is safe to
    /// remember at all.
    #[test]
    fn editing_the_config_invalidates_the_grant() {
        let (dir, cfg, fp) = fixture("invalidate", "export default { a: 1 }");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, fp).unwrap();

        fs::write(&cfg, "export default { a: 2 }").unwrap();
        let new_fp = Fingerprint::compute(&cfg, Path::new("/usr/local/bin/bun")).unwrap();

        assert!(!store.is_granted(&dir, &new_fp), "edited config must re-prompt");
    }

    /// R-031: approving one interpreter is not approving another.
    #[test]
    fn changing_the_interpreter_invalidates_the_grant() {
        let (dir, cfg, fp) = fixture("interp", "export default {}");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, fp).unwrap();

        let other = Fingerprint::compute(&cfg, Path::new("/usr/bin/node")).unwrap();
        assert!(!store.is_granted(&dir, &other));
    }

    /// The versioning that keeps T-030's broadening from being a no-op on
    /// already-granted directories.
    #[test]
    fn a_grant_under_an_older_fingerprint_version_does_not_satisfy_a_newer_one() {
        let (dir, _cfg, mut fp) = fixture("version", "export default {}");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, fp.clone()).unwrap();

        fp.fingerprint_version += 1;
        assert!(!store.is_granted(&dir, &fp));
    }

    /// Likewise, a broadened fingerprint carrying new hashes must not be
    /// satisfied by a grant recorded without them.
    #[test]
    fn extra_hashes_in_the_fingerprint_invalidate_an_older_grant() {
        let (dir, _cfg, fp) = fixture("extra", "export default {}");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, fp.clone()).unwrap();

        let mut broadened = fp.clone();
        broadened
            .additional_hashes
            .insert("bun.lockb".into(), hash_bytes(b"lockfile"));
        assert!(!store.is_granted(&dir, &broadened));
    }

    #[test]
    fn different_spellings_of_one_directory_share_a_grant() {
        let (dir, _cfg, fp) = fixture("spelling", "export default {}");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, fp.clone()).unwrap();

        let awkward = dir.join("sub").join("..");
        fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(store.is_granted(&awkward, &fp), "canonicalization must collapse spellings");
    }

    #[test]
    fn revoking_removes_the_grant() {
        let (dir, _cfg, fp) = fixture("revoke", "export default {}");
        let mut store = ConsentStore::open_at(dir.join("consent.json"));
        store.grant(&dir, fp.clone()).unwrap();
        store.revoke(&dir).unwrap();
        assert!(!store.is_granted(&dir, &fp));
    }

    #[test]
    fn a_corrupt_consent_file_reads_as_no_grants_rather_than_panicking() {
        let dir = tmp("corrupt");
        let p = dir.join("consent.json");
        fs::write(&p, "{not json").unwrap();
        let store = ConsentStore::open_at(p);
        assert!(store.record(&dir).is_none());
    }

    #[test]
    fn the_default_prompt_declines() {
        let req = ConsentRequest {
            directory: PathBuf::from("/x"),
            config_path: PathBuf::from("/x/drizzle.config.ts"),
            interpreter_path: PathBuf::from("/usr/local/bin/bun"),
            interpreter_label: "bun",
            blast_radius: String::new(),
        };
        assert_eq!(AlwaysDecline.ask(&req), ConsentDecision::Decline);
    }
}
