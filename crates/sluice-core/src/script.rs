//! Dialect-aware script splitting, used **only** for display attribution
//! (T-011, R-040).
//!
//! ## The rule this module exists under
//!
//! **The user's text is never rewritten, re-emitted, or re-assembled.** The
//! script goes to the engine verbatim as a single batch. This module only
//! computes *spans* into that same string, so a splitter bug can never
//! manufacture a syntax error the user did not write — the worst it can do is
//! highlight the wrong text, and even that is guarded (see `attribute`).
//!
//! D-009 rejected in-app SQL parsing as a *control*. R-040's objection is that
//! the result shape then made correctness depend on parsing anyway. The
//! resolution is this split: parsing is advisory, execution is verbatim.
//!
//! ## Why a purpose-built lexer
//!
//! R-040 asks for "a real dialect-aware lexer (`sqlparser-rs` or equivalent)
//! rather than a semicolon scan". This is the "or equivalent": a tokenizer that
//! recognises every construct the risk names, with the corpus below as its
//! evidence. Two reasons over pulling in a parser crate: attribution needs exact
//! **UTF-16** offsets for the webview, which a line/column-based token API would
//! have to reconstruct (adding its own bug surface); and `GO` is a client
//! directive that SQL parsers do not tokenize as a terminator anyway.

use serde::{Deserialize, Serialize};

use crate::outcome::Attribution;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScriptDialect {
    /// Dollar-quoted bodies, nested block comments, `E''` escapes.
    Postgres,
    /// `GO` batch separators, `[bracket]` identifiers.
    TSql,
    /// No statement concept — the whole payload is one unit (Mongo).
    Opaque,
}

/// A statement's extent within the submitted script. Offsets are into the
/// original string; nothing is copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementSpan {
    pub start_utf16: u32,
    pub end_utf16: u32,
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Clone, Debug)]
pub struct SplitResult {
    pub statements: Vec<StatementSpan>,
    /// False when the lexer ended inside an unterminated construct, meaning its
    /// view of the script is out of sync and its spans must not be trusted.
    pub confident: bool,
}

/// Split a script into statement spans.
pub fn split(sql: &str, dialect: ScriptDialect) -> SplitResult {
    if dialect == ScriptDialect::Opaque {
        let (u16_len, byte_len) = (utf16_len(sql), sql.len());
        let trimmed = trim_span(sql, 0, byte_len);
        return SplitResult {
            statements: trimmed
                .map(|(s, e)| {
                    vec![StatementSpan {
                        start_utf16: utf16_len(&sql[..s]),
                        end_utf16: utf16_len(&sql[..e]),
                        start_byte: s,
                        end_byte: e,
                    }]
                })
                .unwrap_or_default(),
            confident: u16_len == u16_len,
        };
    }

    let mut lx = Lexer::new(sql, dialect);
    lx.run()
}

/// Pair engine outcomes with source spans.
///
/// R-040's safety net, and the reason a lexer bug here is cosmetic: if the
/// splitter's statement count disagrees with the engine's outcome count, or the
/// lexer reported low confidence, **every** outcome degrades to ordinal-only
/// attribution with no highlighting — rather than pointing at text that may
/// belong to a different statement.
pub fn attribute(split: &SplitResult, outcome_count: usize) -> Vec<Attribution> {
    let usable = split.confident && split.statements.len() == outcome_count;

    (0..outcome_count)
        .map(|i| {
            if usable {
                let s = split.statements[i];
                Attribution::Span {
                    ordinal: i as u32,
                    start_utf16: s.start_utf16,
                    end_utf16: s.end_utf16,
                }
            } else {
                Attribution::Ordinal { ordinal: i as u32 }
            }
        })
        .collect()
}

fn utf16_len(s: &str) -> u32 {
    s.chars().map(|c| c.len_utf16() as u32).sum()
}

/// Narrow `[start, end)` to exclude surrounding whitespace. Returns `None` if
/// the range is entirely whitespace (an empty statement produces no outcome).
fn trim_span(sql: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let slice = &sql[start..end];
    let lead = slice.len() - slice.trim_start().len();
    let trimmed = slice.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some((start + lead, start + lead + trimmed.len()))
}

struct Lexer<'a> {
    sql: &'a str,
    bytes: &'a [u8],
    dialect: ScriptDialect,
    i: usize,
    stmt_start: usize,
    spans: Vec<StatementSpan>,
    healthy: bool,
}

impl<'a> Lexer<'a> {
    fn new(sql: &'a str, dialect: ScriptDialect) -> Self {
        Self {
            sql,
            bytes: sql.as_bytes(),
            dialect,
            i: 0,
            stmt_start: 0,
            spans: Vec::new(),
            healthy: true,
        }
    }

    fn run(mut self) -> SplitResult {
        while self.i < self.bytes.len() {
            let b = self.bytes[self.i];
            match b {
                b'-' if self.peek(1) == Some(b'-') => self.line_comment(),
                b'/' if self.peek(1) == Some(b'*') => self.block_comment(),
                b'\'' => self.single_quoted(),
                b'"' => self.delimited(b'"'),
                b'[' if self.dialect == ScriptDialect::TSql => self.bracket_ident(),
                b'$' if self.dialect == ScriptDialect::Postgres => {
                    if !self.dollar_quoted() {
                        self.i += 1;
                    }
                }
                b';' => {
                    self.push_span(self.i);
                    self.i += 1;
                    self.stmt_start = self.i;
                }
                b'g' | b'G' if self.dialect == ScriptDialect::TSql && self.at_go_separator() => {
                    // GO is a client directive, not a statement terminator: it
                    // ends the batch and is not itself sent as a statement.
                    self.push_span(self.i);
                    self.skip_to_end_of_line();
                    self.stmt_start = self.i;
                }
                _ => self.i += 1,
            }
        }

        // Trailing statement without a terminator.
        self.push_span(self.bytes.len());

        SplitResult { statements: self.spans, confident: self.healthy }
    }

    fn peek(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.i + n).copied()
    }

    fn push_span(&mut self, end: usize) {
        if let Some((s, e)) = trim_span(self.sql, self.stmt_start, end) {
            // A span consisting only of comments is not a statement.
            if !is_only_comments(&self.sql[s..e], self.dialect) {
                self.spans.push(StatementSpan {
                    start_utf16: utf16_len(&self.sql[..s]),
                    end_utf16: utf16_len(&self.sql[..e]),
                    start_byte: s,
                    end_byte: e,
                });
            }
        }
    }

    fn line_comment(&mut self) {
        self.i += 2;
        while self.i < self.bytes.len() && self.bytes[self.i] != b'\n' {
            self.i += 1;
        }
    }

    /// Postgres block comments nest; T-SQL's do not.
    fn block_comment(&mut self) {
        let nests = self.dialect == ScriptDialect::Postgres;
        self.i += 2;
        let mut depth = 1usize;
        while self.i < self.bytes.len() {
            if self.bytes[self.i] == b'*' && self.peek(1) == Some(b'/') {
                self.i += 2;
                depth -= 1;
                if depth == 0 {
                    return;
                }
            } else if nests && self.bytes[self.i] == b'/' && self.peek(1) == Some(b'*') {
                self.i += 2;
                depth += 1;
            } else {
                self.i += 1;
            }
        }
        self.healthy = false; // unterminated comment
    }

    /// `'...'` with `''` escaping. Postgres `E'...'` additionally honours
    /// backslash escapes, so `E'\''` is a single quote inside the literal.
    fn single_quoted(&mut self) {
        let backslash_escapes = self.dialect == ScriptDialect::Postgres && self.preceded_by_e_prefix();
        self.i += 1;
        while self.i < self.bytes.len() {
            match self.bytes[self.i] {
                b'\\' if backslash_escapes => self.i += 2,
                b'\'' if self.peek(1) == Some(b'\'') => self.i += 2,
                b'\'' => {
                    self.i += 1;
                    return;
                }
                _ => self.i += 1,
            }
        }
        self.healthy = false; // unterminated literal
    }

    /// Whether the quote at `self.i` is introduced by an `E` prefix, as in
    /// `E'...'`, and that `E` is not part of a longer identifier.
    fn preceded_by_e_prefix(&self) -> bool {
        if self.i == 0 {
            return false;
        }
        let prev = self.bytes[self.i - 1];
        if prev != b'E' && prev != b'e' {
            return false;
        }
        match self.i.checked_sub(2).map(|j| self.bytes[j]) {
            Some(c) if c.is_ascii_alphanumeric() || c == b'_' => false,
            _ => true,
        }
    }

    /// `"..."` / `` `...` `` with doubling as the escape.
    fn delimited(&mut self, q: u8) {
        self.i += 1;
        while self.i < self.bytes.len() {
            if self.bytes[self.i] == q {
                if self.peek(1) == Some(q) {
                    self.i += 2;
                    continue;
                }
                self.i += 1;
                return;
            }
            self.i += 1;
        }
        self.healthy = false;
    }

    /// T-SQL `[ident]`, where `]]` is an escaped bracket.
    fn bracket_ident(&mut self) {
        self.i += 1;
        while self.i < self.bytes.len() {
            if self.bytes[self.i] == b']' {
                if self.peek(1) == Some(b']') {
                    self.i += 2;
                    continue;
                }
                self.i += 1;
                return;
            }
            self.i += 1;
        }
        self.healthy = false;
    }

    /// `$$ ... $$` or `$tag$ ... $tag$`. Returns false if this `$` does not open
    /// a dollar quote (e.g. the `$1` of a bind parameter), leaving `i` untouched.
    fn dollar_quoted(&mut self) -> bool {
        let start = self.i;
        let mut j = start + 1;
        while j < self.bytes.len() {
            let c = self.bytes[j];
            if c == b'$' {
                break;
            }
            let ok = if j == start + 1 {
                c.is_ascii_alphabetic() || c == b'_'
            } else {
                c.is_ascii_alphanumeric() || c == b'_'
            };
            if !ok {
                return false;
            }
            j += 1;
        }
        if j >= self.bytes.len() {
            return false;
        }

        let tag = &self.sql[start..=j]; // includes both '$'
        match self.sql[j + 1..].find(tag) {
            Some(rel) => {
                self.i = j + 1 + rel + tag.len();
                true
            }
            None => {
                // Unterminated dollar quote: consume the rest and flag it, so
                // attribution degrades rather than splitting inside a body.
                self.i = self.bytes.len();
                self.healthy = false;
                true
            }
        }
    }

    /// `GO` alone on a line, optionally followed by a repeat count.
    fn at_go_separator(&self) -> bool {
        // Must be at the start of a line, ignoring leading whitespace.
        let mut k = self.i;
        while k > 0 {
            match self.bytes[k - 1] {
                b' ' | b'\t' => k -= 1,
                b'\n' | b'\r' => break,
                _ => return false,
            }
        }
        if k != 0 && !matches!(self.bytes[k - 1], b'\n' | b'\r') {
            return false;
        }

        let rest = &self.sql[self.i..];
        if !rest.get(..2).is_some_and(|s| s.eq_ignore_ascii_case("go")) {
            return false;
        }
        // Nothing but whitespace / a digit count until end of line.
        let tail: &str = rest[2..].split(['\n', '\r']).next().unwrap_or("");
        tail.trim().chars().all(|c| c.is_ascii_digit())
    }

    fn skip_to_end_of_line(&mut self) {
        while self.i < self.bytes.len() && self.bytes[self.i] != b'\n' {
            self.i += 1;
        }
        if self.i < self.bytes.len() {
            self.i += 1;
        }
    }
}

/// True when the slice contains no SQL outside comments.
fn is_only_comments(s: &str, dialect: ScriptDialect) -> bool {
    let mut lx = Lexer::new(s, dialect);
    let mut saw_code = false;
    while lx.i < lx.bytes.len() {
        match lx.bytes[lx.i] {
            b'-' if lx.peek(1) == Some(b'-') => lx.line_comment(),
            b'/' if lx.peek(1) == Some(b'*') => lx.block_comment(),
            c if (c as char).is_whitespace() => lx.i += 1,
            _ => {
                saw_code = true;
                break;
            }
        }
    }
    !saw_code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str, d: ScriptDialect) -> Vec<&str> {
        split(sql, d)
            .statements
            .iter()
            .map(|s| &sql[s.start_byte..s.end_byte])
            .collect()
    }

    // ---- the corpus R-040 names, case by case -------------------------------

    #[test]
    fn semicolons_inside_string_literals_do_not_split() {
        let sql = "select 'a;b' ; select 2";
        assert_eq!(texts(sql, ScriptDialect::Postgres), vec!["select 'a;b'", "select 2"]);
    }

    #[test]
    fn doubled_quotes_inside_literals_are_escapes_not_terminators() {
        let sql = "select 'it''s; fine'; select 2";
        assert_eq!(texts(sql, ScriptDialect::Postgres), vec!["select 'it''s; fine'", "select 2"]);
    }

    #[test]
    fn semicolons_inside_quoted_identifiers_do_not_split() {
        let sql = r#"select "we;ird" from t; select 2"#;
        assert_eq!(texts(sql, ScriptDialect::Postgres), vec![r#"select "we;ird" from t"#, "select 2"]);
    }

    #[test]
    fn semicolons_inside_comments_do_not_split() {
        let sql = "select 1 -- a ; comment\n; select 2";
        assert_eq!(texts(sql, ScriptDialect::Postgres), vec!["select 1 -- a ; comment", "select 2"]);
    }

    #[test]
    fn block_comments_nest_in_postgres() {
        let sql = "select 1 /* outer /* inner ; */ still ; comment */ ; select 2";
        assert_eq!(texts(sql, ScriptDialect::Postgres).len(), 2);
    }

    /// The headline case from R-040: a naive split truncates the function body
    /// and sends the server a syntax error on a script that runs fine in psql.
    #[test]
    fn dollar_quoted_bodies_are_not_split() {
        let sql = "create function f() returns int as $$ begin ; return 1 ; end $$ language plpgsql; select 2";
        let got = texts(sql, ScriptDialect::Postgres);
        assert_eq!(got.len(), 2, "got: {got:?}");
        assert!(got[0].contains("begin ; return 1 ; end"));
        assert_eq!(got[1], "select 2");
    }

    #[test]
    fn tagged_dollar_quotes_are_not_split() {
        let sql = "select $tag$ a ; b $tag$; select 2";
        assert_eq!(texts(sql, ScriptDialect::Postgres), vec!["select $tag$ a ; b $tag$", "select 2"]);
    }

    #[test]
    fn bind_parameters_are_not_mistaken_for_dollar_quotes() {
        let sql = "select $1, $2 from t; select 3";
        assert_eq!(texts(sql, ScriptDialect::Postgres), vec!["select $1, $2 from t", "select 3"]);
    }

    #[test]
    fn e_strings_honour_backslash_escapes() {
        let sql = r"select E'a\'; b'; select 2";
        assert_eq!(texts(sql, ScriptDialect::Postgres), vec![r"select E'a\'; b'", "select 2"]);
    }

    #[test]
    fn tsql_go_separates_batches_and_is_not_itself_a_statement() {
        let sql = "select 1\nGO\nselect 2\nGO 3\nselect 3";
        assert_eq!(texts(sql, ScriptDialect::TSql), vec!["select 1", "select 2", "select 3"]);
    }

    #[test]
    fn go_inside_a_line_is_not_a_separator() {
        let sql = "select 'GO' as go_col; select 2";
        assert_eq!(texts(sql, ScriptDialect::TSql), vec!["select 'GO' as go_col", "select 2"]);
    }

    #[test]
    fn tsql_bracket_identifiers_may_contain_semicolons() {
        let sql = "select [we;ird] from t; select 2";
        assert_eq!(texts(sql, ScriptDialect::TSql), vec!["select [we;ird] from t", "select 2"]);
    }

    #[test]
    fn empty_and_comment_only_segments_produce_no_statement() {
        let sql = "select 1;; -- trailing\n;\n/* just a comment */;select 2";
        assert_eq!(texts(sql, ScriptDialect::Postgres), vec!["select 1", "select 2"]);
    }

    // ---- attribution and the safety net ------------------------------------

    #[test]
    fn spans_are_utf16_offsets_for_the_webview() {
        // "🐈" is 4 bytes, 1 char, 2 UTF-16 units.
        let sql = "select '🐈'; select 2";
        let r = split(sql, ScriptDialect::Postgres);
        let second = r.statements[1];
        assert_eq!(&sql[second.start_byte..second.end_byte], "select 2");
        // Verify the utf16 offset really indexes the same place a JS string would.
        let utf16: Vec<u16> = sql.encode_utf16().collect();
        let prefix = String::from_utf16(&utf16[..second.start_utf16 as usize]).unwrap();
        assert_eq!(prefix, "select '🐈'; ");
    }

    #[test]
    fn matching_counts_produce_span_attribution() {
        let r = split("select 1; select 2", ScriptDialect::Postgres);
        let a = attribute(&r, 2);
        assert!(matches!(a[0], Attribution::Span { ordinal: 0, .. }));
        assert!(matches!(a[1], Attribution::Span { ordinal: 1, .. }));
    }

    /// The guard that makes a lexer bug cosmetic instead of destructive.
    #[test]
    fn count_disagreement_degrades_every_outcome_to_ordinal_only() {
        let r = split("select 1; select 2", ScriptDialect::Postgres);
        let a = attribute(&r, 3); // engine reported three outcomes, we found two
        assert_eq!(a.len(), 3);
        assert!(
            a.iter().all(|x| matches!(x, Attribution::Ordinal { .. })),
            "a disagreement must drop highlighting for ALL outcomes, not just the extra one"
        );
    }

    #[test]
    fn unterminated_constructs_mark_the_split_unconfident() {
        assert!(!split("select 'unterminated", ScriptDialect::Postgres).confident);
        assert!(!split("select $$ unterminated", ScriptDialect::Postgres).confident);
        assert!(!split("select 1 /* unterminated", ScriptDialect::Postgres).confident);

        let r = split("select 'oops", ScriptDialect::Postgres);
        assert!(attribute(&r, 1).iter().all(|x| matches!(x, Attribution::Ordinal { .. })));
    }

    #[test]
    fn opaque_dialect_treats_the_whole_payload_as_one_unit() {
        let sql = r#"{"count": "c", "$db": "x"}"#;
        let r = split(sql, ScriptDialect::Opaque);
        assert_eq!(r.statements.len(), 1);
        assert_eq!(&sql[r.statements[0].start_byte..r.statements[0].end_byte], sql);
    }
}
