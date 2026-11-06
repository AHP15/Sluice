//! Credential redaction across the full error chain (T-024 / R-023, D-017).
//!
//! The structural half of R-023's guard lives elsewhere: connection strings are
//! held decomposed with the password as a [`Secret`], and every app-built
//! display form is rebuilt from components, so the app's own text never holds a
//! password to begin with. This module is the **defence in depth** for text the
//! *driver or engine* produces — which can echo a connection string, quote a
//! `password=` conninfo pair, or embed the URI in a TLS or DNS failure.
//!
//! R-023 is specific about why "replace the known password string" is not
//! enough, and each failure it names has a matcher here:
//!
//! - **encoded forms** — the password percent-encoded (either hex case, `+` for
//!   space), backslash-escaped as libpq conninfo quotes it, quote-doubled as SQL
//!   quotes it, or JSON-escaped;
//! - **split forms** — the password broken across two adjacent fields (the end
//!   of `message` and the start of `detail`), or cut off at the end of a
//!   truncated field;
//! - **the whole chain** — not just `message` but the payload's message, every
//!   `extra` pair (detail, hint, where, the `source()` chain a driver records),
//!   and the hint strings;
//! - **passwords we were never told** — credential *shapes* are redacted
//!   regardless: `scheme://user:pass@` and `password=` / `pwd=` pairs.

use crate::error::DriverError;
use crate::secret::{Secret, REDACTED};

/// Below this a password is too short for the split/truncation heuristics to
/// mean anything; exact and encoded matches still apply.
const MIN_SPLIT_LEN: usize = 4;
/// The shortest truncated prefix treated as a leak.
const MIN_TRUNCATED_PREFIX: usize = 6;

/// Redacts the credentials a session knows about, plus credential shapes.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Vec<Secret<String>>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Redactor({} secret(s))", self.secrets.len())
    }
}

impl Redactor {
    pub fn new(secrets: impl IntoIterator<Item = Secret<String>>) -> Self {
        Self { secrets: secrets.into_iter().filter(|s| !s.expose().is_empty()).collect() }
    }

    /// Redact one piece of text: known secrets in every encoding, then shapes.
    pub fn text(&self, s: &str) -> String {
        let mut out = s.to_string();
        for secret in &self.secrets {
            for v in variants(secret.expose()) {
                if out.contains(&v) {
                    out = out.replace(&v, REDACTED);
                }
            }
        }
        redact_shapes(&out)
    }

    /// Redact every text field of an error, including passwords split across
    /// adjacent fields or truncated at a field's end.
    pub fn error(&self, mut e: DriverError) -> DriverError {
        let mut fields: Vec<&mut String> = Vec::new();
        fields.push(&mut e.message);
        if let Some(m) = e.payload.message.as_mut() {
            fields.push(m);
        }
        for (_, v) in e.payload.extra.iter_mut() {
            fields.push(v);
        }
        for h in [&mut e.hints.constraint, &mut e.hints.column, &mut e.hints.table, &mut e.hints.schema]
            .into_iter()
            .flatten()
        {
            fields.push(h);
        }

        // Split and truncated forms need the raw neighbours, so they run first.
        for secret in &self.secrets {
            redact_split(&mut fields, secret.expose());
        }
        for f in fields.iter_mut() {
            **f = self.text(f);
        }
        e
    }
}

/// The forms a password takes in text produced by drivers, URLs and engines.
/// Longest first, so a longer encoding is replaced before a shorter one inside it.
fn variants(p: &str) -> Vec<String> {
    let mut v = vec![
        p.to_string(),
        percent_encode(p, true, false),
        percent_encode(p, false, false),
        percent_encode(p, true, true),
        p.replace('\\', "\\\\").replace('\'', "\\'"),
        p.replace('\'', "''"),
        json_escape(p),
    ];
    v.retain(|s| !s.is_empty());
    v.sort_by_key(|s| std::cmp::Reverse(s.len()));
    v.dedup();
    v
}

fn percent_encode(p: &str, upper: bool, plus_for_space: bool) -> String {
    let mut out = String::with_capacity(p.len() * 3);
    for b in p.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(b as char);
        } else if plus_for_space && b == b' ' {
            out.push('+');
        } else if upper {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    out
}

/// JSON string escaping, without pulling serde_json into the contract crate.
fn json_escape(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for c in p.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// A password broken across adjacent fields, or truncated at a field's end.
fn redact_split(fields: &mut [&mut String], p: &str) {
    let chars: Vec<(usize, char)> = p.char_indices().collect();
    if chars.len() < MIN_SPLIT_LEN {
        return;
    }
    // Byte offsets of every interior char boundary.
    let cuts: Vec<usize> = chars.iter().skip(1).map(|(i, _)| *i).collect();

    for i in 0..fields.len() {
        // Split: this field ends with a head, the next begins with the tail.
        if i + 1 < fields.len() {
            for &k in &cuts {
                let (head, tail) = (&p[..k], &p[k..]);
                if head.chars().count() < 2 || tail.chars().count() < 2 {
                    continue;
                }
                let next_trimmed = fields[i + 1].trim_start().to_string();
                if fields[i].ends_with(head) && next_trimmed.starts_with(tail) {
                    let new_len = fields[i].len() - head.len();
                    fields[i].truncate(new_len);
                    fields[i].push_str(REDACTED);
                    let lead = fields[i + 1].len() - next_trimmed.len();
                    let rest = next_trimmed[tail.len()..].to_string();
                    let ws = fields[i + 1][..lead].to_string();
                    *fields[i + 1] = format!("{ws}{REDACTED}{rest}");
                    break;
                }
            }
        }
        // Truncation: the field ends with a long enough prefix, optionally
        // followed by an ellipsis.
        let body = fields[i].trim_end_matches(['.', '…']).to_string();
        for &k in cuts.iter().rev() {
            let head = &p[..k];
            if head.chars().count() < MIN_TRUNCATED_PREFIX {
                break;
            }
            if body.ends_with(head) && !body.ends_with(REDACTED) {
                let keep = body.len() - head.len();
                let suffix = fields[i][body.len()..].to_string();
                *fields[i] = format!("{}{REDACTED}{suffix}", &body[..keep]);
                break;
            }
        }
    }
}

/// Redact credential *shapes*, for passwords the redactor was never told:
/// URL userinfo (`scheme://user:pass@host`) and key/value pairs
/// (`password=…`, `pwd=…`, quoted or not, conninfo or ODBC style).
pub fn redact_shapes(s: &str) -> String {
    redact_kv(&redact_url_userinfo(s))
}

fn redact_url_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        // Userinfo ends at the first '@' before any '/', whitespace or quote.
        let end = tail.find(|c: char| c == '/' || c.is_whitespace() || c == '"' || c == '\'').unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(atpos) => {
                let creds = &tail[..atpos];
                match creds.find(':') {
                    Some(colon) => {
                        out.push_str(&creds[..colon]);
                        out.push(':');
                        out.push_str(REDACTED);
                    }
                    None => out.push_str(creds),
                }
                out.push('@');
                rest = &tail[atpos + 1..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

fn redact_kv(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let key_len = ["password", "pwd"]
            .iter()
            .find(|k| lower[i..].starts_with(*k) && word_start(&lower, i))
            .map(|k| k.len());
        if let Some(klen) = key_len {
            let mut j = i + klen;
            while s[j..].starts_with([' ', '\t']) {
                j += 1;
            }
            if s[j..].starts_with('=') {
                j += 1;
                while s[j..].starts_with([' ', '\t']) {
                    j += 1;
                }
                let value_end = value_end(s, j);
                out.push_str(&s[i..j]);
                out.push_str(REDACTED);
                i = value_end;
                continue;
            }
        }
        let c = s[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn word_start(lower: &str, i: usize) -> bool {
    i == 0 || !lower[..i].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Where a `key=value` value ends: a matching quote (honouring `\'` and `''`),
/// else whitespace or `;`.
fn value_end(s: &str, start: usize) -> usize {
    let rest = &s[start..];
    // A conninfo quoted inside a SQL literal doubles its quotes:
    // `dblink('… password=''q'' …')`. The value runs to the closing `''`.
    if let Some(inner) = rest.strip_prefix("''") {
        return match inner.find("''") {
            Some(e) => start + 2 + e + 2,
            None => s.len(),
        };
    }
    if let Some(q) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') {
        let mut it = rest.char_indices().skip(1).peekable();
        while let Some((idx, c)) = it.next() {
            if c == '\\' {
                it.next();
                continue;
            }
            if c == q {
                if it.peek().is_some_and(|(_, n)| *n == q) {
                    it.next();
                    continue;
                }
                return start + idx + c.len_utf8();
            }
        }
        return s.len();
    }
    rest.find(|c: char| c.is_whitespace() || c == ';').map(|e| start + e).unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{EnginePayload, ErrorKind};

    const PW: &str = "p@ss w:rd/'é\"x";

    fn r() -> Redactor {
        Redactor::new([Secret::new(PW.to_string())])
    }

    /// Every encoding R-023 names, absent from the serialized error.
    fn assert_clean(e: &DriverError) {
        let json = format!("{e:?}");
        for v in variants(PW) {
            assert!(!json.contains(&v), "variant {v:?} survived in {json}");
        }
    }

    #[test]
    fn every_encoding_of_a_known_password_is_redacted() {
        for v in variants(PW) {
            let e = DriverError::new(ErrorKind::ConnectionFailed, format!("failed: {v} end"));
            let out = r().error(e);
            assert_eq!(out.message, format!("failed: {REDACTED} end"), "variant {v:?}");
        }
    }

    #[test]
    fn the_whole_chain_is_walked_not_just_message() {
        let e = DriverError::new(ErrorKind::ConnectionFailed, "connect failed")
            .with_payload(EnginePayload {
                message: Some(format!("raw driver text with {PW}")),
                extra: vec![
                    ("detail".into(), format!("detail {}", percent_encode(PW, true, false))),
                    ("hint".into(), format!("hint '{}'", PW.replace('\'', "''"))),
                    ("source.1".into(), format!("caused by: {}", json_escape(PW))),
                ],
                ..Default::default()
            });
        assert_clean(&r().error(e));
    }

    #[test]
    fn a_password_split_across_message_and_detail_is_redacted() {
        let (head, tail) = PW.split_at(5);
        let e = DriverError::new(ErrorKind::ConnectionFailed, format!("could not use {head}"))
            .with_payload(EnginePayload {
                extra: vec![("detail".into(), format!("  {tail} rejected"))],
                ..Default::default()
            });
        let out = r().error(e);
        assert!(!out.message.contains(head), "{}", out.message);
        assert!(!out.payload.extra[0].1.contains(tail), "{}", out.payload.extra[0].1);
        assert!(out.payload.extra[0].1.ends_with(" rejected"));
    }

    #[test]
    fn a_truncated_password_at_a_field_end_is_redacted() {
        let long = Redactor::new([Secret::new("correcthorsebattery".to_string())]);
        let out = long.error(DriverError::new(ErrorKind::ConnectionFailed, "password was correcthor..."));
        assert_eq!(out.message, format!("password was {REDACTED}..."));
    }

    #[test]
    fn short_passwords_skip_split_heuristics_but_are_still_matched_exactly() {
        let r = Redactor::new([Secret::new("abc".to_string())]);
        assert_eq!(r.text("x abc y"), format!("x {REDACTED} y"));
        // "ab" at the end of one field is not treated as a split of "abc".
        let out = r.error(DriverError::new(ErrorKind::Unknown, "tab"));
        assert_eq!(out.message, "tab");
    }

    /// Passwords nobody told us about: the shapes.
    #[test]
    fn credential_shapes_are_redacted_without_knowing_the_password() {
        let cases = [
            ("postgres://app:s3cr3t@db:5432/x", format!("postgres://app:{REDACTED}@db:5432/x")),
            ("dbname=x password=hunter2 host=h", format!("dbname=x password={REDACTED} host=h")),
            ("password = 'two words\\' here' sslmode=disable", format!("password = {REDACTED} sslmode=disable")),
            ("Server=h;User Id=sa;Password=Pa;ss;Encrypt=yes", format!("Server=h;User Id=sa;Password={REDACTED};ss;Encrypt=yes")),
            ("PWD=abc123;", format!("PWD={REDACTED};")),
            ("dblink('host=h password=''q'' user=u')", format!("dblink('host=h password={REDACTED} user=u')")),
        ];
        for (input, want) in cases {
            assert_eq!(redact_shapes(input), want, "{input}");
        }
    }

    /// Ordinary text containing the *word* password is left alone.
    #[test]
    fn ordinary_messages_are_untouched() {
        for s in [
            "password authentication failed for user \"postgres\"",
            "relation \"passwords\" does not exist",
            "see https://www.postgresql.org/docs/ for details",
            "could not connect to server: Connection refused",
        ] {
            assert_eq!(r().text(s), s);
        }
    }

    #[test]
    fn the_redactor_never_prints_its_secrets() {
        assert!(!format!("{:?}", r()).contains("p@ss"));
    }
}
