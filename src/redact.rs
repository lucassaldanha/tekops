//! Anonymises log content for `tekops dump-logs`.
//!
//! Pure: no I/O, no environment reads, no `Command`. The whole engine is a
//! hand-rolled token scanner rather than a set of regexes, for the reason
//! `metrics.rs` hand-rolls a Prometheus exposition parser - this repo halved
//! its binary by dropping ureq's TLS backend and justifies every `Cargo.toml`
//! line in a comment, so a dependency for a handful of shapes is not a trade
//! it wants.
//!
//! Values are replaced with *stable* placeholders (`<peer-1>`, `<ip-3>`)
//! rather than a flat marker. Flat redaction destroys the correlations that
//! make a log diagnosable: "peer 3 disconnected, then reconnected" is often
//! the whole finding.

use std::collections::HashMap;
use std::fmt;

/// What a redacted value was. The variant order is not significant; the
/// placeholder text comes from `prefix`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Ip,
    Peer,
    Enr,
    Pubkey,
    Address,
    User,
    Secret,
    Validator,
    Graffiti,
    Host,
}

impl Category {
    fn prefix(self) -> &'static str {
        match self {
            Category::Ip => "ip",
            Category::Peer => "peer",
            Category::Enr => "enr",
            Category::Pubkey => "pubkey",
            Category::Address => "address",
            Category::User => "user",
            Category::Secret => "secret",
            Category::Validator => "validator",
            Category::Graffiti => "graffiti",
            Category::Host => "host",
        }
    }
}

/// How much was replaced. Counts *distinct* values, not occurrences, so
/// re-seeing one peer a thousand times reports as one.
#[derive(Debug, PartialEq, Eq)]
pub struct Summary {
    pub values: usize,
    pub categories: usize,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} value{} across {} categor{}",
            self.values,
            if self.values == 1 { "" } else { "s" },
            self.categories,
            if self.categories == 1 { "y" } else { "ies" }
        )
    }
}

#[derive(Default)]
pub struct Redactor {
    assigned: HashMap<(Category, String), u32>,
    next: HashMap<Category, u32>,
}

/// The characters that end a token.
///
/// `.` `:` `-` `_` and `@` are deliberately *not* here: they have to stay
/// inside tokens so an IPv4 address, an IPv6 address, a `user:pass@host` and a
/// hostname each arrive as one piece. `/` is here so the addresses inside a
/// multiaddr (`/ip4/1.2.3.4/tcp/9000`) split out on their own; the two things
/// that genuinely need `/` kept (URLs and `/home/<user>` paths) are handled by
/// pre-passes that run before this tokenizer ever sees them.
///
/// `|` is included because Teku puts literal pipes inside its own messages
/// (`Configuration | Network: hoodi`).
fn is_delim(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '/' | ',' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '=' | '<' | '>' | ';' | '|'
        )
}

/// The normalised form of a token when it is being used as an *anchor* for the
/// token after it. Trailing `:` and `.` are punctuation rather than part of the
/// word, and matching is case-insensitive.
fn anchor_key(tok: &str) -> String {
    tok.trim_end_matches([':', '.']).to_ascii_lowercase()
}

impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn summary(&self) -> Summary {
        Summary {
            values: self.assigned.len(),
            categories: self.next.len(),
        }
    }

    /// The placeholder for one value, allocating a new number the first time
    /// this exact value is seen in this category and reusing it thereafter.
    fn token(&mut self, cat: Category, value: &str) -> String {
        let key = (cat, value.to_string());
        let n = match self.assigned.get(&key) {
            Some(n) => *n,
            None => {
                let slot = self.next.entry(cat).or_insert(0);
                *slot += 1;
                let n = *slot;
                self.assigned.insert(key, n);
                n
            }
        };
        format!("<{}-{}>", cat.prefix(), n)
    }

    pub fn redact(&mut self, line: &str) -> String {
        self.redact_tokens(line)
    }

    /// Splits on delimiters, classifies each token, and re-emits every
    /// delimiter untouched, so a line with nothing sensitive in it comes back
    /// byte-identical.
    fn redact_tokens(&mut self, line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut cur = String::new();
        let mut prev: Option<String> = None;
        for c in line.chars() {
            if is_delim(c) {
                if !cur.is_empty() {
                    out.push_str(&self.replace_token(&cur, prev.as_deref()));
                    prev = Some(anchor_key(&cur));
                    cur.clear();
                }
                out.push(c);
            } else {
                cur.push(c);
            }
        }
        if !cur.is_empty() {
            out.push_str(&self.replace_token(&cur, prev.as_deref()));
        }
        out
    }

    fn replace_token(&mut self, tok: &str, _prev: Option<&str>) -> String {
        if tok.starts_with("enr:-") {
            return self.token(Category::Enr, tok);
        }
        tok.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_with_nothing_sensitive_is_returned_unchanged() {
        let mut r = Redactor::new();
        let line = "2026-09-14 21:14:02.117 INFO  - Syncing, 42 peers (slot 12034887)";
        assert_eq!(r.redact(line), line);
    }

    #[test]
    fn punctuation_and_spacing_survive_tokenizing() {
        let mut r = Redactor::new();
        let line = "  a/b,c(d)[e]{f}\"g\"'h'=i<j>k;l|m\t n  ";
        assert_eq!(r.redact(line), line);
    }

    /// The same value must map to the same placeholder for the whole run, and
    /// a different value to a different one. This is the property the whole
    /// pseudonymising design exists for: it is what lets a maintainer follow
    /// "this peer disconnected, then reconnected".
    #[test]
    fn one_value_gets_one_stable_token_across_lines() {
        let mut r = Redactor::new();
        assert_eq!(r.redact("enr:-AAA disconnected"), "<enr-1> disconnected");
        assert_eq!(r.redact("saw enr:-BBB"), "saw <enr-2>");
        assert_eq!(r.redact("enr:-AAA reconnected"), "<enr-1> reconnected");
    }

    #[test]
    fn summary_counts_distinct_values_and_categories() {
        let mut r = Redactor::new();
        r.redact("enr:-AAA enr:-BBB enr:-AAA");
        let s = r.summary();
        assert_eq!(s.values, 2);
        assert_eq!(s.categories, 1);
        assert_eq!(s.to_string(), "2 values across 1 category");
    }

    #[test]
    fn summary_is_singular_for_one_value() {
        let mut r = Redactor::new();
        r.redact("enr:-AAA");
        assert_eq!(r.summary().to_string(), "1 value across 1 category");
    }
}
