//! Query history in SQLite in the data dir (T-033 / D-014).
//!
//! History grows without bound and wants indexed search, which is a database's
//! job rather than a hand-edited file's. It is **durable** — deleting it loses
//! something — so it is versioned like the config file: SQLite's
//! `user_version` pragma is the format number, migrations run forward inside
//! one transaction, and a database stamped by a newer build is refused rather
//! than opened (an older build would otherwise write rows the newer schema
//! does not expect).
//!
//! It is also **machine-local** by design: D-011's portability covers profiles
//! and saved queries, not history.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::StateError;

pub const FILE_NAME: &str = "history.sqlite3";

/// The schema version this build writes.
pub const CURRENT_VERSION: u32 = 1;

/// `MIGRATIONS[n]` upgrades a database at `user_version = n` to `n + 1`.
/// Append only; never edit a shipped step.
const MIGRATIONS: &[&str] = &[
    // 0 → 1: the first schema.
    "CREATE TABLE history (
         id           INTEGER PRIMARY KEY,
         profile_id   TEXT    NOT NULL,
         executed_at  INTEGER NOT NULL,          -- unix milliseconds
         statement    TEXT    NOT NULL,
         -- T-025 / R-030: 1 when a credential literal was replaced before the
         -- statement was stored; such an entry is not replayable verbatim.
         redacted     INTEGER NOT NULL DEFAULT 0,
         outcome      TEXT    NOT NULL,          -- ok | error | cancelled
         duration_ms  INTEGER,
         rows         INTEGER
     );
     CREATE INDEX history_by_profile_time ON history (profile_id, executed_at DESC);
     CREATE INDEX history_by_time ON history (executed_at DESC);",
];

/// One statement as it is stored. `statement` is already redacted (T-025)
/// by the time it gets here — this store never sees the raw text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub profile_id: String,
    pub executed_at: i64,
    pub statement: String,
    pub redacted: bool,
    pub outcome: String,
    pub duration_ms: Option<i64>,
    pub rows: Option<i64>,
}

/// What a caller records; the store assigns the id.
#[derive(Debug, Clone)]
pub struct NewEntry<'a> {
    pub profile_id: &'a str,
    pub executed_at: i64,
    pub statement: &'a str,
    pub redacted: bool,
    pub outcome: &'a str,
    pub duration_ms: Option<i64>,
    pub rows: Option<i64>,
}

pub struct History {
    conn: Connection,
    path: PathBuf,
}

impl History {
    /// Open (creating if needed) `<data dir>/history.sqlite3`, migrating it
    /// forward. Refuses a database from a newer build.
    pub fn open(data_dir: &Path) -> Result<Self, StateError> {
        std::fs::create_dir_all(data_dir).map_err(|e| StateError::io("query history directory", e))?;
        let path = data_dir.join(FILE_NAME);
        let mut conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&self, e: &NewEntry) -> Result<i64, StateError> {
        self.conn.execute(
            "INSERT INTO history (profile_id, executed_at, statement, redacted, outcome, duration_ms, rows)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![e.profile_id, e.executed_at, e.statement, e.redacted, e.outcome, e.duration_ms, e.rows],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Newest first. `profile_id: None` means every profile; `search` is a
    /// case-insensitive substring match over the stored statement.
    pub fn list(&self, profile_id: Option<&str>, search: Option<&str>, limit: u32) -> Result<Vec<HistoryEntry>, StateError> {
        let pattern = search.filter(|s| !s.is_empty()).map(|s| format!("%{}%", escape_like(s)));
        let mut stmt = self.conn.prepare(
            "SELECT id, profile_id, executed_at, statement, redacted, outcome, duration_ms, rows
               FROM history
              WHERE (?1 IS NULL OR profile_id = ?1)
                AND (?2 IS NULL OR statement LIKE ?2 ESCAPE '\\')
              ORDER BY executed_at DESC, id DESC
              LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![profile_id, pattern, limit], |r| {
            Ok(HistoryEntry {
                id: r.get(0)?,
                profile_id: r.get(1)?,
                executed_at: r.get(2)?,
                statement: r.get(3)?,
                redacted: r.get(4)?,
                outcome: r.get(5)?,
                duration_ms: r.get(6)?,
                rows: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn count(&self) -> Result<u64, StateError> {
        Ok(self.conn.query_row("SELECT count(*) FROM history", [], |r| r.get::<_, i64>(0))? as u64)
    }

}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn user_version(conn: &Connection) -> Result<u32, StateError> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(v.max(0) as u32)
}

fn migrate(conn: &mut Connection) -> Result<(), StateError> {
    let found = user_version(conn)?;
    if found > CURRENT_VERSION {
        return Err(StateError::NewerVersion {
            what: "the query history database".into(),
            found,
            supported: CURRENT_VERSION,
        });
    }
    if found == CURRENT_VERSION {
        return Ok(());
    }
    // One transaction for the whole climb: a failure part-way leaves the
    // database at the version it started at, never between two.
    let tx = conn.transaction()?;
    for (from, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (from + 1) as i64)?;
    }
    tx.commit()?;
    Ok(())
}

/// For tests and the diagnostics view: the stored schema version, without
/// migrating.
pub fn stored_version(path: &Path) -> Result<Option<u32>, StateError> {
    if !path.exists() {
        return Ok(None);
    }
    let conn = Connection::open(path)?;
    let v = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)).optional()?;
    Ok(v.map(|v| v.max(0) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(profile: &'a str, at: i64, sql: &'a str) -> NewEntry<'a> {
        NewEntry { profile_id: profile, executed_at: at, statement: sql, redacted: false, outcome: "ok", duration_ms: Some(3), rows: Some(1) }
    }

    #[test]
    fn a_fresh_database_is_stamped_with_the_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path()).unwrap();
        assert_eq!(user_version(&h.conn).unwrap(), CURRENT_VERSION);
        assert_eq!(stored_version(h.path()).unwrap(), Some(CURRENT_VERSION));
        // Reopening is a no-op, not a second migration.
        drop(h);
        History::open(dir.path()).unwrap();
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        {
            let c = Connection::open(dir.path().join(FILE_NAME)).unwrap();
            c.pragma_update(None, "user_version", (CURRENT_VERSION + 1) as i64).unwrap();
        }
        let err = History::open(dir.path()).err().expect("must refuse");
        assert!(matches!(err, StateError::NewerVersion { .. }), "{err}");
        // And it was not touched.
        assert_eq!(stored_version(&dir.path().join(FILE_NAME)).unwrap(), Some(CURRENT_VERSION + 1));
    }

    #[test]
    fn lists_newest_first_filtered_by_profile_and_search() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path()).unwrap();
        h.record(&entry("a", 1, "select 1")).unwrap();
        h.record(&entry("b", 2, "select 2")).unwrap();
        h.record(&entry("a", 3, "update t set x = 1")).unwrap();

        let all = h.list(None, None, 10).unwrap();
        assert_eq!(all.iter().map(|e| e.executed_at).collect::<Vec<_>>(), vec![3, 2, 1]);
        assert_eq!(h.list(Some("a"), None, 10).unwrap().len(), 2);
        assert_eq!(h.list(None, Some("SELECT"), 10).unwrap().len(), 2);
        assert_eq!(h.count().unwrap(), 3);
    }

    #[test]
    fn search_treats_like_wildcards_literally() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open(dir.path()).unwrap();
        h.record(&entry("a", 1, "select 'x'")).unwrap();
        h.record(&entry("a", 2, "select '100%'")).unwrap();
        assert_eq!(h.list(None, Some("%"), 10).unwrap().len(), 1);
        assert_eq!(h.list(None, Some("_"), 10).unwrap().len(), 0);
    }
}
