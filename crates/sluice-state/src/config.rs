//! Versioned, human-editable TOML documents in the config dir (T-033 / D-014).
//!
//! The mechanics only — the profile and saved-query schema is
//! [`crate::workspace`]'s. Every document carries a top-level integer
//! `version`. On load:
//!
//! - **missing file** → `Ok(None)`: first run, not an error;
//! - **`version` newer than this build** → [`StateError::NewerVersion`], and the
//!   file is left untouched;
//! - **older `version`** → each [`Migration`] is applied in order on the raw
//!   table before deserializing. The migrated form is **not** written back on
//!   load: an older build syncing the same file must keep reading it until the
//!   user actually saves something here.
//!
//! A hand-written file with no `version` line is read as version 1, the first
//! format — the friendliest reading of "I typed this myself".

use std::fs;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{write_atomic, StateError};

/// Upgrades a document from `from` to `from + 1`, on the raw table, so a
/// migration never needs the old Rust types to still exist.
pub struct Migration {
    pub from: u32,
    pub apply: fn(&mut toml::Table) -> Result<(), String>,
}

/// One versioned document's identity: what to call it in errors, the version
/// this build writes, and how to get there from older ones.
pub struct DocumentSpec<'a> {
    pub what: &'a str,
    pub current: u32,
    pub migrations: &'a [Migration],
    /// Comment lines written above the document on every save. Each line is
    /// prefixed with `# ` here; pass the text only.
    pub header: &'a str,
}

/// A loaded document and whether it needed migrating to get here.
#[derive(Debug)]
pub struct Loaded<T> {
    pub value: T,
    /// The version the file was at, when older than [`DocumentSpec::current`].
    pub migrated_from: Option<u32>,
}

pub fn load<T: DeserializeOwned>(path: &Path, spec: &DocumentSpec) -> Result<Option<Loaded<T>>, StateError> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StateError::io(spec.what, e)),
    };
    parse(&text, spec).map(Some)
}

pub fn parse<T: DeserializeOwned>(text: &str, spec: &DocumentSpec) -> Result<Loaded<T>, StateError> {
    let invalid = |detail: String| StateError::Invalid { what: spec.what.to_string(), detail };
    let mut table: toml::Table = toml::from_str(text).map_err(|e| invalid(e.to_string()))?;

    let found = match table.remove("version") {
        None => 1,
        Some(toml::Value::Integer(v)) if v >= 1 && v <= u32::MAX as i64 => v as u32,
        Some(other) => return Err(invalid(format!("`version` must be a positive integer, found {other}"))),
    };
    if found > spec.current {
        return Err(StateError::NewerVersion { what: spec.what.to_string(), found, supported: spec.current });
    }

    let mut at = found;
    while at < spec.current {
        let m = spec
            .migrations
            .iter()
            .find(|m| m.from == at)
            .ok_or_else(|| invalid(format!("no migration from format {at}")))?;
        (m.apply)(&mut table).map_err(|e| invalid(format!("migrating from format {at}: {e}")))?;
        at += 1;
    }

    let value = T::deserialize(toml::Value::Table(table)).map_err(|e| invalid(e.to_string()))?;
    Ok(Loaded { value, migrated_from: (found < spec.current).then_some(found) })
}

/// Render `value` with the header comment and `version` as its first key.
pub fn render<T: Serialize>(value: &T, spec: &DocumentSpec) -> Result<String, StateError> {
    let invalid = |detail: String| StateError::Invalid { what: spec.what.to_string(), detail };
    let body = toml::Value::try_from(value).map_err(|e| invalid(e.to_string()))?;
    let toml::Value::Table(body) = body else {
        return Err(invalid("document must serialize to a table".into()));
    };

    let mut doc = toml::Table::new();
    doc.insert("version".into(), toml::Value::Integer(spec.current as i64));
    for (k, v) in body {
        if k != "version" {
            doc.insert(k, v);
        }
    }

    let mut out = String::new();
    for line in spec.header.lines() {
        if line.is_empty() {
            out.push_str("#\n");
        } else {
            out.push_str("# ");
            out.push_str(line);
            out.push('\n');
        }
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&toml::to_string_pretty(&doc).map_err(|e| invalid(e.to_string()))?);
    Ok(out)
}

/// Save atomically (temp file plus rename).
pub fn save<T: Serialize>(path: &Path, value: &T, spec: &DocumentSpec) -> Result<(), StateError> {
    write_atomic(path, render(value, spec)?.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
    struct Doc {
        #[serde(default)]
        names: Vec<String>,
    }

    /// Format 1 stored a single `name`; format 2 stores a list of `names`.
    fn v1_to_v2(t: &mut toml::Table) -> Result<(), String> {
        if let Some(v) = t.remove("name") {
            t.insert("names".into(), toml::Value::Array(vec![v]));
        }
        Ok(())
    }

    const MIGRATIONS: &[Migration] = &[Migration { from: 1, apply: v1_to_v2 }];
    const SPEC: DocumentSpec = DocumentSpec { what: "test doc", current: 2, migrations: MIGRATIONS, header: "hello\n\nworld" };

    #[test]
    fn a_missing_file_is_first_run_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load::<Doc>(&dir.path().join("nope.toml"), &SPEC).unwrap().is_none());
    }

    #[test]
    fn round_trips_with_version_first_and_the_header_above_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.toml");
        let doc = Doc { names: vec!["a".into()] };
        save(&path, &doc, &SPEC).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# hello\n#\n# world\n\nversion = 2\n"), "{text}");
        let loaded = load::<Doc>(&path, &SPEC).unwrap().unwrap();
        assert_eq!(loaded.value, doc);
        assert_eq!(loaded.migrated_from, None);
    }

    #[test]
    fn older_formats_migrate_forward_without_being_rewritten_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.toml");
        fs::write(&path, "version = 1\nname = \"a\"\n").unwrap();

        let loaded = load::<Doc>(&path, &SPEC).unwrap().unwrap();
        assert_eq!(loaded.value.names, vec!["a".to_string()]);
        assert_eq!(loaded.migrated_from, Some(1));
        // An older build syncing this file must still be able to read it.
        assert_eq!(fs::read_to_string(&path).unwrap(), "version = 1\nname = \"a\"\n");
    }

    #[test]
    fn a_hand_written_file_without_a_version_is_format_one() {
        let loaded = parse::<Doc>("name = \"x\"\n", &SPEC).unwrap();
        assert_eq!(loaded.migrated_from, Some(1));
        assert_eq!(loaded.value.names, vec!["x".to_string()]);
    }

    #[test]
    fn a_newer_file_is_refused_and_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.toml");
        let newer = "version = 3\nnames = [\"a\"]\nfuture_field = true\n";
        fs::write(&path, newer).unwrap();

        let err = load::<Doc>(&path, &SPEC).unwrap_err();
        assert!(matches!(err, StateError::NewerVersion { found: 3, supported: 2, .. }), "{err}");
        assert!(err.to_string().contains("newer version of Sluice"));
        assert_eq!(fs::read_to_string(&path).unwrap(), newer);
    }

    #[test]
    fn a_nonsense_version_is_invalid_not_silently_format_one() {
        for bad in ["version = \"2\"\n", "version = 0\n", "version = -1\n"] {
            assert!(matches!(parse::<Doc>(bad, &SPEC), Err(StateError::Invalid { .. })), "{bad}");
        }
    }
}
