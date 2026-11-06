//! Statements that need typed confirmation on a production session (T-061 /
//! D-027): DDL, and DELETE/UPDATE with no WHERE.
//!
//! The classifier works on code only — literals and comments are stripped by
//! the same tokenizer that redacts statements (T-025) — so a string containing
//! `DROP TABLE` neither triggers it nor hides from it. It is **conservative**:
//! a false alarm costs one typed word, a miss can cost a table. So:
//!
//! - an unqualified DELETE/UPDATE is found at *any* nesting depth, which
//!   catches data-modifying CTEs (`WITH d AS (DELETE FROM t) SELECT …`);
//! - `EXPLAIN ANALYZE DELETE …` counts, because ANALYZE executes it;
//! - a `DO` block or `CALL` is flagged, since its body is not inspected;
//! - a statement that cannot be tokenized but names a dangerous keyword is
//!   flagged as unparseable.

use crate::script::{split, ScriptDialect};
use crate::statement_redact::{tokenize, Kind};

/// One statement that needs confirmation, and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dangerous {
    /// The statement's start, whitespace-collapsed and cut to a preview.
    pub statement: String,
    pub reason: String,
}

const DDL: &[&str] = &["CREATE", "ALTER", "DROP", "TRUNCATE", "GRANT", "REVOKE", "COMMENT", "REINDEX", "CLUSTER"];
const PROCEDURAL: &[&str] = &["DO", "CALL"];
/// Words that start a statement and settle what it is. EXPLAIN and ANALYZE
/// are deliberately absent: `EXPLAIN ANALYZE DELETE` must be read as DELETE.
const OTHER_MAIN: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "COPY", "SET", "RESET", "SHOW", "BEGIN", "START", "COMMIT",
    "ROLLBACK", "SAVEPOINT", "RELEASE", "VALUES", "TABLE", "LOCK", "VACUUM", "REFRESH", "LISTEN", "NOTIFY",
    "PREPARE", "EXECUTE", "DEALLOCATE", "DISCARD", "FETCH", "MOVE", "CLOSE", "DECLARE",
];
/// Words before UPDATE/DELETE that make it not a statement: `FOR UPDATE`,
/// `ON DELETE CASCADE`, `DO UPDATE SET` (an upsert touches the conflicting
/// row only), `FOR NO KEY UPDATE`.
const NOT_A_STATEMENT_AFTER: &[&str] = &["FOR", "KEY", "ON", "DO", "NO"];
const PREVIEW_CHARS: usize = 100;

pub fn dangerous_statements(script: &str) -> Vec<Dangerous> {
    let parts = split(script, ScriptDialect::Postgres);
    let spans: Vec<&str> = if parts.confident && !parts.statements.is_empty() {
        parts.statements.iter().map(|s| &script[s.start_byte..s.end_byte]).collect()
    } else {
        vec![script]
    };
    spans.into_iter().filter_map(classify).collect()
}

struct Word {
    text: String,
    depth: i32,
}

fn classify(stmt: &str) -> Option<Dangerous> {
    let preview = || {
        let flat = stmt.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.chars().count() > PREVIEW_CHARS {
            format!("{}…", flat.chars().take(PREVIEW_CHARS).collect::<String>())
        } else {
            flat
        }
    };
    let hit = |reason: String| Some(Dangerous { statement: preview(), reason });

    let Some(segments) = tokenize(stmt, false) else {
        let upper = stmt.to_ascii_uppercase();
        let named = DDL.iter().chain(PROCEDURAL).chain(["DELETE", "UPDATE"].iter()).any(|k| contains_word(&upper, k));
        return if named { hit("could not be parsed, and it names a statement that needs confirmation".into()) } else { None };
    };

    let words = code_words(stmt, &segments);

    // What kind of statement is this? The first word that settles it.
    let main = words.iter().find(|w| {
        w.depth == 0 && (DDL.contains(&w.text.as_str()) || PROCEDURAL.contains(&w.text.as_str()) || OTHER_MAIN.contains(&w.text.as_str()))
    });
    if let Some(m) = main {
        if DDL.contains(&m.text.as_str()) {
            return hit(format!("DDL ({})", m.text));
        }
        if PROCEDURAL.contains(&m.text.as_str()) {
            return hit(format!("{} — its body is not inspected", if m.text == "DO" { "a DO block" } else { "CALL" }));
        }
    }

    // Any DELETE/UPDATE, at any depth, without a WHERE in its own scope.
    for (i, w) in words.iter().enumerate() {
        if w.text != "DELETE" && w.text != "UPDATE" {
            continue;
        }
        if i > 0 && NOT_A_STATEMENT_AFTER.contains(&words[i - 1].text.as_str()) {
            continue;
        }
        let qualified = words[i + 1..]
            .iter()
            .take_while(|x| x.depth >= w.depth)
            .any(|x| x.depth == w.depth && x.text == "WHERE");
        if !qualified {
            return hit(format!("{} without WHERE affects every row", w.text));
        }
    }
    None
}

/// Upper-cased identifier-like words from code segments, with the paren
/// depth each one sits at.
fn code_words(stmt: &str, segments: &[crate::statement_redact::Seg]) -> Vec<Word> {
    let mut words = Vec::new();
    let mut depth = 0i32;
    for seg in segments {
        if seg.kind != Kind::Code {
            continue;
        }
        let text = &stmt[seg.start..seg.end];
        let mut cur = String::new();
        for c in text.chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                cur.push(c.to_ascii_uppercase());
                continue;
            }
            if !cur.is_empty() {
                words.push(Word { text: std::mem::take(&mut cur), depth });
            }
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
        }
        if !cur.is_empty() {
            words.push(Word { text: cur, depth });
        }
    }
    words
}

fn contains_word(upper: &str, word: &str) -> bool {
    upper
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|w| w == word)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reasons(sql: &str) -> Vec<String> {
        dangerous_statements(sql).into_iter().map(|d| d.reason).collect()
    }

    #[test]
    fn ddl_is_flagged() {
        for sql in [
            "DROP TABLE orders",
            "create index on t (a)",
            "ALTER TABLE t ADD COLUMN x int",
            "TRUNCATE t",
            "grant select on t to bob",
            "/* note */ drop schema s cascade",
        ] {
            assert_eq!(reasons(sql).len(), 1, "{sql}");
            assert!(reasons(sql)[0].starts_with("DDL"), "{sql}");
        }
    }

    #[test]
    fn unqualified_delete_and_update_are_flagged_and_qualified_ones_are_not() {
        assert_eq!(reasons("DELETE FROM orders"), vec!["DELETE without WHERE affects every row"]);
        assert_eq!(reasons("update orders set paid = true"), vec!["UPDATE without WHERE affects every row"]);
        assert!(reasons("DELETE FROM orders WHERE id = 1").is_empty());
        assert!(reasons("UPDATE t SET a = 1 FROM u WHERE t.id = u.id").is_empty());
        assert!(reasons("DELETE FROM t USING u").len() == 1, "USING is not a WHERE");
    }

    /// A WHERE inside a subquery does not qualify the outer statement.
    #[test]
    fn a_nested_where_does_not_qualify_the_outer_statement() {
        assert_eq!(reasons("UPDATE t SET a = (SELECT max(b) FROM u WHERE u.k = 1)").len(), 1);
    }

    #[test]
    fn data_modifying_ctes_and_explain_analyze_are_caught() {
        assert_eq!(reasons("WITH d AS (DELETE FROM t RETURNING *) SELECT count(*) FROM d").len(), 1);
        assert!(reasons("WITH d AS (DELETE FROM t WHERE id < 5 RETURNING *) SELECT * FROM d").is_empty());
        assert_eq!(reasons("EXPLAIN ANALYZE DELETE FROM t").len(), 1);
    }

    #[test]
    fn procedural_blocks_are_flagged_because_they_are_not_inspected() {
        assert!(reasons("DO $$ BEGIN DELETE FROM t; END $$")[0].contains("DO block"));
        assert!(reasons("CALL purge_everything()")[0].contains("CALL"));
    }

    #[test]
    fn keywords_in_literals_comments_and_clauses_do_not_trigger() {
        for sql in [
            "SELECT 'DROP TABLE t; DELETE FROM t'",
            "SELECT 1 -- DELETE FROM t",
            "SELECT * FROM t FOR UPDATE",
            "SELECT * FROM t FOR NO KEY UPDATE",
            "INSERT INTO t VALUES (1) ON CONFLICT (id) DO UPDATE SET a = 2",
            "SELECT \"update\", \"delete\" FROM audit",
            "select * from orders where status = 'drop'",
        ] {
            assert!(reasons(sql).is_empty(), "{sql}: {:?}", reasons(sql));
        }
    }

    #[test]
    fn every_statement_in_a_script_is_checked() {
        let found = dangerous_statements("SELECT 1; DELETE FROM a; UPDATE b SET x = 1 WHERE id = 2; DROP TABLE c;");
        assert_eq!(found.len(), 2);
        assert!(found[0].statement.starts_with("DELETE FROM a"));
        assert!(found[1].statement.starts_with("DROP TABLE c"));
    }

    #[test]
    fn an_unparseable_dangerous_statement_is_flagged() {
        assert_eq!(reasons("DELETE FROM t WHERE name = 'unterminated").len(), 1);
        assert!(reasons("SELECT 'unterminated").is_empty());
    }
}
