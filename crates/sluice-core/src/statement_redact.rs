//! Credential-bearing statements on the history and diagnostics paths (T-025 /
//! R-030, D-014, D-020).
//!
//! The trust boundary keeps *stored* credentials out of the webview, but the
//! query editor accepts credentials as ordinary query text:
//! `CREATE ROLE x PASSWORD '…'`, `ALTER LOGIN … WITH PASSWORD = '…'`,
//! `db.createUser({pwd: "…"})`, a conninfo inside `dblink(…)`, a
//! `postgres_fdw` user mapping's `OPTIONS (password '…')`. That text never took
//! the credential code path, so neither `Secret` nor the boundary
//! [`Redactor`](crate::Redactor) can see it. The guard therefore lives on the
//! paths that persist statement text — query history and verbose logs — and
//! this is the one function both call.
//!
//! **Conservative by construction.** Precision is a nicety; a leak is not:
//!
//! 1. *Targeted*: a string literal that follows a password keyword
//!    (`PASSWORD`, `ENCRYPTED PASSWORD`, `IDENTIFIED BY`, `pwd:`, `@rmtpassword =`,
//!    `PASSWORD(…)`) is replaced with `'[redacted]'`.
//! 2. *Shapes*: [`redact_shapes`] then removes `password=`/`pwd=` pairs and
//!    URL userinfo wherever they appear — which is what catches a conninfo or
//!    URL inside a `dblink`/`postgres_fdw` literal.
//! 3. *Fallback*: if the statement mentions a password keyword but neither
//!    rule found anything, **every** string literal is redacted, and so is any
//!    comment that mentions one.
//! 4. *Unparseable*: if the statement mentions a password keyword and cannot
//!    be tokenized (an unterminated quote, say), the whole text is replaced.
//!
//! Whatever was changed, [`StoredStatement::redacted`] is `true`, so the UI can
//! say that entry is not replayable as written.

use crate::redact::redact_shapes;
use crate::secret::REDACTED;

/// A statement as it may be persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredStatement {
    pub text: String,
    /// Something was replaced, so `text` will not re-run as the user wrote it.
    pub redacted: bool,
}

/// Replacement for a statement too malformed to redact precisely.
pub const UNPARSEABLE: &str =
    "-- [redacted] statement mentioned a password and could not be parsed safely, so it was not stored";

/// Keywords that mark a statement as possibly credential-bearing. Matched
/// case-insensitively as substrings, so `@rmtpassword`, `old_password` and
/// `PASSWORD(` all count.
const KEYWORDS: &[&str] = &["password", "passwd", "pwd", "identified by"];

/// Redact credential literals from a statement before it is stored or logged.
pub fn redact_statement(sql: &str) -> StoredStatement {
    let js = looks_like_mongo_shell(sql);
    let mentions = mentions_keyword(sql);

    let segments = match tokenize(sql, js) {
        Some(s) => s,
        None if mentions => return StoredStatement { text: UNPARSEABLE.to_string(), redacted: true },
        // No keyword and not tokenizable: shapes still apply.
        None => return finish(sql, redact_shapes(sql)),
    };

    if !mentions {
        return finish(sql, redact_shapes(sql));
    }

    // 1. Targeted: the literal right after a keyword.
    let mut out = String::with_capacity(sql.len());
    let mut armed = false;
    let mut targeted = false;
    for seg in &segments {
        let text = &sql[seg.start..seg.end];
        match seg.kind {
            Kind::Code => {
                armed = ends_with_keyword(text) || (armed && is_separator_only(text));
                out.push_str(text);
            }
            k if k.is_string(js) => {
                if armed {
                    out.push_str(&quoted(k));
                    targeted = true;
                    armed = false;
                } else {
                    // A quoted key — `"pwd": "…"` — arms the next literal.
                    armed = KEYWORDS.iter().any(|kw| unquote(text).eq_ignore_ascii_case(kw));
                    out.push_str(text);
                }
            }
            _ => {
                // Identifiers and comments neither arm nor disarm.
                out.push_str(text);
            }
        }
    }

    // 2. Shapes.
    let shaped = redact_shapes(&out);
    if targeted || shaped != out {
        return finish(sql, shaped);
    }

    // 3. Fallback: a keyword with nothing recognisable after it.
    let mut out = String::with_capacity(sql.len());
    for seg in &segments {
        let text = &sql[seg.start..seg.end];
        match seg.kind {
            k if k.is_string(js) => out.push_str(&quoted(k)),
            Kind::LineComment if mentions_keyword(text) => out.push_str(&format!("-- {REDACTED}")),
            Kind::BlockComment if mentions_keyword(text) => out.push_str(&format!("/* {REDACTED} */")),
            _ => out.push_str(text),
        }
    }
    finish(sql, redact_shapes(&out))
}

fn finish(original: &str, text: String) -> StoredStatement {
    StoredStatement { redacted: text != original, text }
}

fn mentions_keyword(s: &str) -> bool {
    let lower = normalize_ws(&s.to_ascii_lowercase());
    KEYWORDS.iter().any(|k| lower.contains(k))
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `db.createUser(…)`, `db.getSiblingDB("x").updateUser(…)`, `use admin` —
/// Mongo shell syntax, where double quotes delimit strings, not identifiers.
fn looks_like_mongo_shell(s: &str) -> bool {
    let t = s.trim_start();
    t.starts_with("db.") || s.contains("createUser") || s.contains("updateUser")
}

/// Does this code fragment end in a password keyword, ignoring the
/// punctuation that may sit between a keyword and its value?
fn ends_with_keyword(code: &str) -> bool {
    let lower = normalize_ws(&code.to_ascii_lowercase());
    let trimmed = lower.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '=' | ':' | '(' | ','));
    KEYWORDS.iter().any(|k| trimmed.ends_with(k))
}

/// Whitespace and the punctuation between a keyword and its value — so
/// `PASSWORD /* c */ '…'` and `pwd : "…"` stay armed across the gap.
fn is_separator_only(code: &str) -> bool {
    code.chars().all(|c| c.is_whitespace() || matches!(c, '=' | ':' | '('))
}

fn unquote(s: &str) -> &str {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`')
}

fn quoted(k: Kind) -> String {
    match k {
        Kind::Double => format!("\"{REDACTED}\""),
        Kind::Backtick => format!("`{REDACTED}`"),
        _ => format!("'{REDACTED}'"),
    }
}

// --------------------------------------------------------------- tokenizer

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Code,
    /// `'…'`, including `E'…'` and `N'…'` (the prefix stays in `Code`).
    Single,
    /// `$tag$…$tag$` (Postgres).
    Dollar,
    /// `"…"` — an identifier in SQL, a string in the Mongo shell.
    Double,
    /// `` `…` `` — a string in the Mongo shell.
    Backtick,
    LineComment,
    BlockComment,
}

impl Kind {
    fn is_string(self, js: bool) -> bool {
        match self {
            Kind::Single | Kind::Dollar => true,
            Kind::Double | Kind::Backtick => js,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Seg {
    kind: Kind,
    start: usize,
    end: usize,
}

/// Split into code, literals and comments. `None` on an unterminated
/// literal or block comment — the caller decides how conservative to be.
fn tokenize(sql: &str, js: bool) -> Option<Vec<Seg>> {
    let b = sql.as_bytes();
    let mut segs = Vec::new();
    let mut i = 0;
    let mut code_start = 0;

    let push_code = |segs: &mut Vec<Seg>, from: usize, to: usize| {
        if to > from {
            segs.push(Seg { kind: Kind::Code, start: from, end: to });
        }
    };

    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        let (kind, end) = if c == b'-' && next == Some(b'-') || (js && c == b'/' && next == Some(b'/')) {
            let end = sql[i..].find('\n').map(|n| i + n).unwrap_or(b.len());
            (Kind::LineComment, end)
        } else if c == b'/' && next == Some(b'*') {
            let end = sql[i + 2..].find("*/").map(|n| i + 2 + n + 2)?;
            (Kind::BlockComment, end)
        } else if c == b'\'' {
            // `E'…'` strings honour backslash escapes; in the shell, all do.
            let backslash = js || (i > 0 && matches!(b[i - 1], b'e' | b'E') && !is_ident(b.get(i.wrapping_sub(2)).copied()));
            (Kind::Single, quoted_end(b, i, b'\'', backslash)?)
        } else if c == b'"' {
            (Kind::Double, quoted_end(b, i, b'"', js)?)
        } else if js && c == b'`' {
            (Kind::Backtick, quoted_end(b, i, b'`', true)?)
        } else if !js && c == b'$' && !is_ident(if i > 0 { Some(b[i - 1]) } else { None }) {
            match dollar_tag(sql, i) {
                Some(tag) => {
                    let body = i + tag.len();
                    let close = sql[body..].find(tag).map(|n| body + n + tag.len())?;
                    (Kind::Dollar, close)
                }
                None => {
                    i += 1;
                    continue;
                }
            }
        } else {
            i += 1;
            continue;
        };
        push_code(&mut segs, code_start, i);
        segs.push(Seg { kind, start: i, end });
        i = end;
        code_start = end;
    }
    push_code(&mut segs, code_start, b.len());
    Some(segs)
}

fn is_ident(c: Option<u8>) -> bool {
    c.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// End (exclusive) of a quoted run starting at `start`. SQL doubles the quote
/// to escape it; `backslash` additionally honours `\`.
fn quoted_end(b: &[u8], start: usize, q: u8, backslash: bool) -> Option<usize> {
    let mut i = start + 1;
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            if !backslash && b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// `$$` or `$tag$` at `i`, if one starts there.
fn dollar_tag(sql: &str, i: usize) -> Option<&str> {
    let rest = &sql[i + 1..];
    let close = rest.find('$')?;
    let tag = &rest[..close];
    if tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !tag.starts_with(|c: char| c.is_ascii_digit()) {
        Some(&sql[i..i + close + 2])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(sql: &str) -> StoredStatement {
        redact_statement(sql)
    }

    fn assert_hidden(sql: &str, secret: &str) -> StoredStatement {
        let out = r(sql);
        assert!(!out.text.contains(secret), "leaked `{secret}`:\n  in:  {sql}\n  out: {}", out.text);
        assert!(out.redacted, "not flagged: {sql}");
        out
    }

    #[test]
    fn create_and_alter_role_user_login_password() {
        let out = assert_hidden("CREATE ROLE app WITH LOGIN PASSWORD 'hunter2' VALID UNTIL '2027-01-01'", "hunter2");
        assert_eq!(out.text, "CREATE ROLE app WITH LOGIN PASSWORD '[redacted]' VALID UNTIL '2027-01-01'");
        assert_hidden("alter user app encrypted password 's3cr3t!'", "s3cr3t!");
        assert_hidden("ALTER ROLE app PASSWORD $$dollar$pw$$", "dollar$pw");
        assert_hidden("ALTER ROLE app PASSWORD $q$quoted'pw$q$", "quoted'pw");
        assert_hidden("ALTER ROLE app PASSWORD E'back\\'slash'", "back\\'slash");
        assert_hidden("ALTER ROLE app PASSWORD 'it''s secret'", "s secret");
        // SQL Server and MySQL spellings.
        assert_hidden("CREATE LOGIN app WITH PASSWORD = N'Tr0ub4dor&3', CHECK_POLICY = OFF", "Tr0ub4dor");
        assert_hidden("ALTER LOGIN app WITH PASSWORD = 'new1' OLD_PASSWORD = 'old1'", "old1");
        assert_hidden("CREATE USER 'a'@'%' IDENTIFIED BY 'mysqlpw'", "mysqlpw");
        assert_hidden("SET PASSWORD FOR app = PASSWORD('legacy')", "legacy");
    }

    #[test]
    fn mongo_create_and_update_user() {
        let out = assert_hidden(r#"db.createUser({ user: "app", pwd: "m0ng0pw", roles: ["readWrite"] })"#, "m0ng0pw");
        assert!(out.text.contains(r#"user: "app""#), "only the password is redacted: {}", out.text);
        assert_hidden(r#"db.updateUser("app", { "pwd": 'quoted-key' })"#, "quoted-key");
        assert_hidden("db.getSiblingDB('admin').createUser({user:'x', pwd:`tmpl`})", "tmpl");
        // passwordPrompt() carries no literal; nothing to hide, nothing broken.
        let out = r(r#"db.createUser({ user: "app", pwd: passwordPrompt(), roles: [] })"#);
        assert!(out.text.contains("passwordPrompt()"));
    }

    #[test]
    fn dblink_and_postgres_fdw_connection_strings() {
        let out = assert_hidden(
            "SELECT * FROM dblink('host=10.0.0.5 dbname=prod user=etl password=fdwpw', 'select 1') AS t(x int)",
            "fdwpw",
        );
        assert!(out.text.contains("host=10.0.0.5"), "{}", out.text);
        assert_hidden("SELECT dblink_connect('conn', 'postgresql://etl:urlpw@10.0.0.5/prod')", "urlpw");
        assert_hidden(
            "CREATE USER MAPPING FOR app SERVER remote OPTIONS (user 'etl', password 'mappw')",
            "mappw",
        );
        assert_hidden("ALTER USER MAPPING FOR app SERVER remote OPTIONS (SET password 'mappw2')", "mappw2");
        assert_hidden(
            "EXEC sp_addlinkedsrvlogin @rmtsrvname = 'REMOTE', @useself = 'false', @rmtuser = 'sa', @rmtpassword = 'lnkpw'",
            "lnkpw",
        );
    }

    #[test]
    fn an_unparseable_statement_with_a_password_keyword_is_redacted_whole() {
        let out = r("ALTER ROLE app PASSWORD 'unterminated");
        assert_eq!(out.text, UNPARSEABLE);
        assert!(out.redacted);
        let out = r("create user x /* password: hunter2");
        assert_eq!(out.text, UNPARSEABLE);
    }

    #[test]
    fn a_keyword_with_no_recognisable_value_redacts_every_literal() {
        let out = assert_hidden("SELECT set_config('app.secret', 'k3y', false) -- password rotation", "k3y");
        assert!(!out.text.contains("rotation"), "{}", out.text);
        assert_hidden("/* the password is hunter2 */ SELECT 1", "hunter2");
    }

    #[test]
    fn ordinary_statements_are_stored_verbatim() {
        for sql in [
            "select * from orders where id = 42",
            "insert into t (a) values ('hello'), ('it''s')",
            "select $$body$$, \"Quoted Ident\" from x",
            "select 'unterminated", // no keyword: shapes only, never a whole-text wipe
            "",
        ] {
            let out = r(sql);
            assert_eq!(out.text, sql);
            assert!(!out.redacted, "{sql}");
        }
    }

    #[test]
    fn a_url_with_a_password_is_redacted_even_without_a_keyword() {
        assert_hidden("select 'postgres://u:urlonly@h/db'", "urlonly");
    }

    /// In SQL, double quotes are identifiers — a column literally named
    /// "password" is not a value and must not swallow the next literal.
    #[test]
    fn sql_identifiers_are_not_treated_as_strings() {
        let out = r(r#"select "password" from users where name = 'bob'"#);
        // The keyword is present and nothing targeted it, so the fallback
        // redacts literals — conservative — but the identifier itself stays.
        assert!(out.text.contains(r#""password""#), "{}", out.text);
        assert!(!out.text.contains("bob"));
    }
}
