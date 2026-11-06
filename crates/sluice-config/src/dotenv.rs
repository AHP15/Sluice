//! `.env` parsing (T-028 / D-026).
//!
//! D-026 keeps `.env` in the automatic, unprompted tier because it is *parsed,
//! never executed*. That is only true if this parser stays a parser, so two
//! things dotenv implementations commonly do are deliberately **not** done here:
//!
//! - **No variable interpolation.** `${OTHER}` and `$OTHER` are left literal.
//!   Interpolation turns a data file into a small evaluated language, and it
//!   reads from the ambient environment — which is how a value the user never
//!   wrote ends up inside their connection string.
//! - **No command substitution.** `$(...)` is literal text. Some `.env` loaders
//!   support it; supporting it here would reintroduce R-001 in the tier D-026
//!   specifically declared safe.
//!
//! The file is read as data, and the only key discovery cares about is a
//! connection URL.

use std::collections::BTreeMap;

/// Parse `.env` content into key/value pairs.
///
/// Supported: `KEY=value`, `export KEY=value`, `#` comments, blank lines,
/// single- and double-quoted values, and `\n`/`\t`/`\"`/`\\` escapes inside
/// double quotes. Later duplicate keys win, matching every dotenv loader.
pub fn parse(content: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);

        let Some((key, value)) = line.split_once('=') else { continue };

        let key = key.trim();
        if !is_valid_key(key) {
            continue;
        }

        out.insert(key.to_string(), parse_value(value.trim()));
    }

    out
}

/// Environment-variable naming rules: a leading letter or underscore, then
/// alphanumerics and underscores. The leading-digit rule matters — `1BAD=x` is
/// not a variable any shell would export, so accepting it here would mean
/// discovery sees keys the surrounding tooling never will.
fn is_valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn parse_value(raw: &str) -> String {
    let mut chars = raw.chars().peekable();

    match chars.peek() {
        // Single quotes are literal in every dotenv dialect — no escapes.
        Some('\'') => {
            let inner: String = chars.skip(1).collect();
            match inner.rfind('\'') {
                Some(end) => inner[..end].to_string(),
                None => inner,
            }
        }
        Some('"') => {
            let inner: String = chars.skip(1).collect();
            let body = match inner.rfind('"') {
                Some(end) => &inner[..end],
                None => &inner[..],
            };
            unescape(body)
        }
        // Unquoted: a `#` preceded by whitespace begins a trailing comment.
        _ => strip_trailing_comment(raw).trim().to_string(),
    }
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            // Unknown escape: keep both characters rather than silently eating
            // the backslash — a password may legitimately contain one.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn strip_trailing_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && i > 0 && (bytes[i - 1] as char).is_whitespace() {
            return &s[..i];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_ordinary_forms() {
        let env = parse(
            r#"
# a comment
DATABASE_URL=postgres://localhost/app
export OTHER=value
QUOTED="hello world"
SINGLE='raw $NOT_INTERPOLATED'

EMPTY=
"#,
        );
        assert_eq!(env["DATABASE_URL"], "postgres://localhost/app");
        assert_eq!(env["OTHER"], "value");
        assert_eq!(env["QUOTED"], "hello world");
        assert_eq!(env["SINGLE"], "raw $NOT_INTERPOLATED");
        assert_eq!(env["EMPTY"], "");
    }

    /// The load-bearing safety property of this module. A `.env` that says
    /// `PASS=${SECRET}` must yield that text, not the ambient `SECRET`.
    #[test]
    fn interpolation_is_not_performed() {
        let env = parse("A=${HOME}\nB=$HOME\nC=$(whoami)\nD=`whoami`");
        assert_eq!(env["A"], "${HOME}");
        assert_eq!(env["B"], "$HOME");
        assert_eq!(env["C"], "$(whoami)");
        assert_eq!(env["D"], "`whoami`");
    }

    #[test]
    fn passwords_with_awkward_characters_survive() {
        let env = parse(r#"DATABASE_URL="postgres://u:p@ss#word@h/db""#);
        assert_eq!(env["DATABASE_URL"], "postgres://u:p@ss#word@h/db");

        // Unquoted, a `#` with leading whitespace is a comment; without, it is
        // part of the value.
        let env = parse("A=has#hash\nB=value # trailing comment");
        assert_eq!(env["A"], "has#hash");
        assert_eq!(env["B"], "value");
    }

    #[test]
    fn escapes_apply_only_inside_double_quotes() {
        let env = parse("A=\"line\\nbreak\"\nB='line\\nbreak'");
        assert_eq!(env["A"], "line\nbreak");
        assert_eq!(env["B"], "line\\nbreak");
    }

    #[test]
    fn later_duplicates_win() {
        let env = parse("A=first\nA=second");
        assert_eq!(env["A"], "second");
    }

    #[test]
    fn malformed_lines_are_skipped_rather_than_aborting_the_file() {
        let env = parse("no_equals_sign\n=novalue\n1BAD=x\nGOOD=yes");
        assert_eq!(env["GOOD"], "yes");
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn an_unterminated_quote_does_not_panic() {
        let env = parse("A=\"unterminated\nB='also");
        assert!(env.contains_key("A"));
        assert!(env.contains_key("B"));
    }
}
