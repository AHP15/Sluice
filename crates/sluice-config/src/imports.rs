//! What a config's consent fingerprint covers beyond the file itself
//! (T-030 / R-031).
//!
//! A config's executable surface is its import graph, which changes through
//! `npm install` and `git pull` while `drizzle.config.ts` stays byte-identical.
//! This module finds, **without executing anything**, the extra files whose
//! change should re-prompt:
//!
//! - every lockfile governing the config's dependencies — found by walking up
//!   from the config's directory, because in a monorepo the lockfile sits at the
//!   workspace root while the config lives in `packages/db`;
//! - the local files the config names by **relative** import specifier
//!   (`./env`, `../lib/db`), resolved the way TS/Node would by extension and
//!   `index` lookup.
//!
//! Deliberately **shallow** — direct imports only. Following them transitively
//! would reach application code (`./src/db/schema` and on), so ordinary edits
//! would re-prompt constantly, and consent fatigue degrades the very guard it is
//! for. R-031 accepts the residual: a malicious edit deep in the graph that
//! changes no lockfile and no directly-imported file still inherits consent.
//!
//! Inert: reads and hashes only (see `tests/no_subprocess.rs`).

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::consent::hash_file;

/// Lockfiles of every JS package manager the interpreter resolution supports.
pub const LOCKFILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
];

/// Extensions tried, in order, for an extensionless specifier.
const EXTENSIONS: &[&str] = &["ts", "mts", "cts", "tsx", "js", "mjs", "cjs", "jsx", "json"];

/// Upper bound on the walk toward the filesystem root.
const MAX_ANCESTORS: usize = 12;

/// Upper bound on how many specifiers one config contributes.
const MAX_IMPORTS: usize = 64;

/// The extra hashes for a config, keyed by stable labels:
/// `lockfile:<path>` and `import:<specifier>`. Paths are relative to the
/// config's directory so a grant survives the directory being renamed.
///
/// An import that does not resolve is recorded as `unresolved`, so the file
/// *appearing* later changes the fingerprint too.
pub fn extra_hashes(config_path: &Path) -> BTreeMap<String, String> {
    let dir = config_path.parent().unwrap_or(Path::new("."));
    let mut out = BTreeMap::new();

    for lock in lockfiles(dir) {
        let label = format!("lockfile:{}", relative_label(dir, &lock));
        out.insert(label, hash_file(&lock).unwrap_or_else(|| "unreadable".into()));
    }

    if let Ok(src) = std::fs::read_to_string(config_path) {
        for spec in relative_specifiers(&src).into_iter().take(MAX_IMPORTS) {
            let hash = match resolve_specifier(dir, &spec) {
                Some(p) => hash_file(&p).unwrap_or_else(|| "unreadable".into()),
                None => "unresolved".into(),
            };
            out.insert(format!("import:{spec}"), hash);
        }
    }
    out
}

/// Lockfiles in the nearest directory (the config's own, or an ancestor) that
/// has any — Node resolves `node_modules` upward, so that directory's lockfile
/// is the one pinning what the config will load. The walk stops at a
/// repository root (`.git`) so it never wanders out of the project.
pub fn lockfiles(start: &Path) -> Vec<PathBuf> {
    let mut dir = Some(start.to_path_buf());
    for _ in 0..MAX_ANCESTORS {
        let Some(d) = dir else { break };
        let found: Vec<PathBuf> =
            LOCKFILES.iter().map(|l| d.join(l)).filter(|p| p.is_file()).collect();
        if !found.is_empty() {
            return found;
        }
        if d.join(".git").exists() {
            break;
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    Vec::new()
}

/// Relative specifiers in `import … from "x"`, `import "x"`, `export … from
/// "x"`, `import("x")` and `require("x")`. A lexical scan, not a parse: a
/// match inside a comment only adds a hash, which errs toward re-prompting.
pub fn relative_specifiers(src: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(off) = src[i..].find(['"', '\'', '`']) {
        let open = i + off;
        let quote = src.as_bytes()[open] as char;
        let Some(len) = src[open + 1..].find(quote) else { break };
        let lit = &src[open + 1..open + 1 + len];
        // Resume after the closing quote, so it is never read as an opener.
        i = open + 1 + len + 1;
        if !(lit.starts_with("./") || lit.starts_with("../")) || lit.contains('\n') {
            continue;
        }
        if is_import_position(&src[..open]) && !out.iter().any(|s| s == lit) {
            out.push(lit.to_string());
        }
    }
    out
}

/// Whether the text just before a string literal makes it a module specifier.
fn is_import_position(before: &str) -> bool {
    let t = before.trim_end();
    t.ends_with("from")
        || t.ends_with("import")
        || t.ends_with("import(")
        || t.ends_with("require(")
}

/// Resolve a relative specifier the way TS/Node would, without executing.
pub fn resolve_specifier(dir: &Path, spec: &str) -> Option<PathBuf> {
    let base = normalize(&dir.join(spec));
    if base.is_file() {
        return Some(base);
    }
    // TS's ESM convention: `import "./env.js"` names the source `./env.ts`.
    if let Some(stem) = spec.strip_suffix(".js").or_else(|| spec.strip_suffix(".mjs")) {
        for ext in ["ts", "mts", "tsx"] {
            let p = normalize(&dir.join(format!("{stem}.{ext}")));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    for ext in EXTENSIONS {
        let p = PathBuf::from(format!("{}.{ext}", base.to_string_lossy()));
        if p.is_file() {
            return Some(p);
        }
    }
    if base.is_dir() {
        for ext in EXTENSIONS {
            let p = base.join(format!("index.{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Lexically collapse `.` and `..` so labels are stable.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn relative_label(dir: &Path, target: &Path) -> String {
    let mut up = PathBuf::new();
    let mut base = dir.to_path_buf();
    loop {
        if let Ok(rest) = target.strip_prefix(&base) {
            return up.join(rest).to_string_lossy().into_owned();
        }
        if !base.pop() {
            return target.to_string_lossy().into_owned();
        }
        up.push("..");
    }
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
        let d = std::env::temp_dir().join("sluice-imports").join(format!("{tag}-{nanos}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finds_relative_specifiers_in_every_import_form_and_ignores_packages() {
        let src = r#"
            import "dotenv/config";
            import { env } from "./env";
            import * as db from '../lib/db.js';
            export { x } from "./reexport";
            const y = require("./legacy.cjs");
            const z = await import(`./dynamic`);
            const notAnImport = "./just-a-string";
            import pg from "pg";
        "#;
        assert_eq!(
            relative_specifiers(src),
            vec!["./env", "../lib/db.js", "./reexport", "./legacy.cjs", "./dynamic"]
        );
    }

    #[test]
    fn resolution_follows_ts_and_node_rules() {
        let d = tmp("resolve");
        fs::write(d.join("env.ts"), "").unwrap();
        fs::create_dir(d.join("lib")).unwrap();
        fs::write(d.join("lib/index.ts"), "").unwrap();
        fs::write(d.join("util.mjs"), "").unwrap();

        assert_eq!(resolve_specifier(&d, "./env"), Some(d.join("env.ts")));
        assert_eq!(resolve_specifier(&d, "./env.js"), Some(d.join("env.ts")), "ESM .js → .ts");
        assert_eq!(resolve_specifier(&d, "./lib"), Some(d.join("lib/index.ts")));
        assert_eq!(resolve_specifier(&d, "./util.mjs"), Some(d.join("util.mjs")));
        assert_eq!(resolve_specifier(&d, "./missing"), None);
    }

    #[test]
    fn the_nearest_lockfile_wins_and_the_walk_stops_at_the_repo_root() {
        let root = tmp("mono");
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "lock").unwrap();
        let pkg = root.join("packages/db");
        fs::create_dir_all(&pkg).unwrap();
        assert_eq!(lockfiles(&pkg), vec![root.join("pnpm-lock.yaml")]);

        // Above a .git boundary is outside the project.
        let outer = tmp("outer");
        fs::write(outer.join("yarn.lock"), "lock").unwrap();
        let repo = outer.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(lockfiles(&repo).is_empty());
    }

    #[test]
    fn labels_are_stable_and_relative() {
        let root = tmp("labels");
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join("package-lock.json"), "{}").unwrap();
        let pkg = root.join("app");
        fs::create_dir(&pkg).unwrap();
        let cfg = pkg.join("drizzle.config.ts");
        fs::write(&cfg, r#"import { u } from "./missing";"#).unwrap();

        let keys: Vec<String> = extra_hashes(&cfg).into_keys().collect();
        assert_eq!(keys, vec!["import:./missing", "lockfile:../package-lock.json"]);
        assert_eq!(extra_hashes(&cfg)["import:./missing"], "unresolved");
    }
}
