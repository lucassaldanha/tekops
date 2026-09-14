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
/// `.` `:` `-` and `_` are deliberately *not* here: they have to stay inside
/// tokens so an IPv4 address, an IPv6 address and a hostname each arrive as
/// one piece. `/` is here so the addresses inside a multiaddr
/// (`/ip4/1.2.3.4/tcp/9000`) split out on their own; the two things that
/// genuinely need `/` kept (URLs and `/home/<user>` paths) are handled by
/// pre-passes that run before this tokenizer ever sees them.
///
/// `@` *is* a delimiter, which is not obvious: keeping it would make
/// `user:pass@host` one token, but `redact_urls` has already replaced any
/// userinfo by the time this runs, so all keeping it achieves is gluing a
/// leading `@` onto the host and stopping that host being recognised as an IP.
///
/// `|` is included because Teku puts literal pipes inside its own messages
/// (`Configuration | Network: hoodi`).
fn is_delim(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '/' | ','
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '"'
                | '\''
                | '='
                | '<'
                | '>'
                | ';'
                | '|'
                | '@'
        )
}

/// The normalised form of a token when it is being used as an *anchor* for the
/// token after it. Trailing `:` and `.` are punctuation rather than part of the
/// word, and matching is case-insensitive.
fn anchor_key(tok: &str) -> String {
    tok.trim_end_matches([':', '.']).to_ascii_lowercase()
}

/// Four dot-separated decimal groups, each at most 255.
fn is_ipv4(s: &str) -> bool {
    let mut groups = 0;
    for p in s.split('.') {
        groups += 1;
        if groups > 4
            || p.is_empty()
            || p.len() > 3
            || !p.chars().all(|c| c.is_ascii_digit())
            || p.parse::<u16>().map_or(true, |n| n > 255)
        {
            return false;
        }
    }
    groups == 4
}

/// Hex groups separated by colons.
///
/// The second condition is load-bearing rather than pedantry. A Teku console
/// timestamp's time field is `01:16:54`, which is two colons and three groups
/// that are all valid hexadecimal - a rule of "two or more colons and every
/// group is hex" redacts every timestamp in the file. Requiring either a `::`
/// run or the full eight-group (seven-colon) form rejects it, and no real IPv6
/// address lacks both.
fn is_ipv6(s: &str) -> bool {
    let colons = s.chars().filter(|c| *c == ':').count();
    if colons < 2 {
        return false;
    }
    if !s.contains("::") && colons != 7 {
        return false;
    }
    if !s.chars().any(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    s.split(':')
        .all(|g| g.len() <= 4 && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Base58 as Bitcoin and libp2p define it: alphanumeric minus the four glyphs
/// that are easy to confuse (`0`, `O`, `I`, `l`).
fn is_base58(c: char) -> bool {
    c.is_ascii_alphanumeric() && !matches!(c, '0' | 'O' | 'I' | 'l')
}

/// The multihash prefixes libp2p peer ids actually appear with. Teku uses
/// secp256k1 (`16Uiu2HA`); `12D3KooW` is ed25519 and `Qm` the older sha256
/// form, both of which turn up in peer lists from other clients.
const PEER_PREFIXES: [&str; 3] = ["16Uiu2HA", "12D3KooW", "Qm"];

/// The length bound is what keeps this from matching ordinary prose that
/// happens to start with `Qm`. Real peer ids are 44 to 53 characters.
fn is_peer_id(s: &str) -> bool {
    (40..=60).contains(&s.len())
        && PEER_PREFIXES.iter().any(|p| s.starts_with(p))
        && s.chars().all(is_base58)
}

/// Splits a trailing `:<digits>` port off a token, if there is one.
fn split_host_port(tok: &str) -> Option<(&str, &str)> {
    let (host, port) = tok.rsplit_once(':')?;
    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((host, port))
}

/// Classification that depends only on the token's own shape.
fn shape_of(tok: &str) -> Option<Category> {
    if tok.starts_with("enr:-") {
        return Some(Category::Enr);
    }
    if is_ipv4(tok) || is_ipv6(tok) {
        return Some(Category::Ip);
    }
    if is_peer_id(tok) {
        return Some(Category::Peer);
    }
    // Exact lengths, not ranges. `0x` + 96 hex is uniquely a BLS pubkey and
    // `0x` + 40 hex is uniquely an execution address, so both are safe on
    // shape alone. `0x` + 64 hex is NOT: that is also every block root and
    // state root in the file, which appear on nearly every line and are what
    // make the dump readable. See `block_and_state_roots_are_preserved`.
    if let Some(h) = tok.strip_prefix("0x") {
        if h.chars().all(|c| c.is_ascii_hexdigit()) {
            if h.len() == 96 {
                return Some(Category::Pubkey);
            }
            if h.len() == 40 {
                return Some(Category::Address);
            }
        }
    }
    None
}

/// Keywords that mark the *next* token as a validator index.
const VALIDATOR_ANCHORS: [&str; 5] = [
    "validator",
    "validators",
    "index",
    "validator_index",
    "validatorindex",
];

/// Keywords that mark the next token as a hostname.
///
/// `node` is deliberately absent even though it looks like an obvious member.
/// Teku writes it constantly in ordinary prose ("node is syncing", "node
/// started"), so anchoring on it would redact the following word line after
/// line.
const HOST_ANCHORS: [&str; 2] = ["host", "hostname"];

/// Whether a URL path segment looks like a credential rather than a word.
///
/// Long, alphanumeric-plus-`-_`, and mixing digits and letters: that is the
/// shape of an Infura project id, an Alchemy key or a signed token, and it is
/// not the shape of `eth`, `v1`, `node` or `syncing`.
fn is_opaque_secret(seg: &str) -> bool {
    seg.len() >= 20
        && seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && seg.chars().any(|c| c.is_ascii_digit())
        && seg.chars().any(|c| c.is_ascii_alphabetic())
}

/// The path prefixes whose next segment is a username.
const HOME_PREFIXES: [&str; 2] = ["/home/", "/Users/"];

/// Classification that depends on the preceding token.
///
/// Needed because a validator index has no shape of its own - it is a bare
/// integer, indistinguishable from a slot, an epoch, a port or a count. The
/// keyword before it is the only signal available.
fn anchored(tok: &str, prev: Option<&str>) -> Option<Category> {
    let prev = prev?;
    if prev == "graffiti" {
        return Some(Category::Graffiti);
    }
    if VALIDATOR_ANCHORS.contains(&prev)
        && !tok.is_empty()
        && tok.chars().all(|c| c.is_ascii_digit())
    {
        return Some(Category::Validator);
    }
    if HOST_ANCHORS.contains(&prev) && !tok.chars().all(|c| c.is_ascii_digit()) {
        return Some(Category::Host);
    }
    None
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

    /// URLs first, then home paths, then the tokenizer.
    ///
    /// Both pre-passes need `/` to still be part of the text, which the
    /// tokenizer destroys. They emit `<secret-1>`-style placeholders whose
    /// `<` and `>` are delimiters, so the tokenizer re-splits them into inert
    /// words and leaves them alone.
    pub fn redact(&mut self, line: &str) -> String {
        let urls = self.redact_urls(line);
        let homes = self.redact_home_paths(&urls);
        self.redact_tokens(&homes)
    }

    /// Replaces the username segment of `/home/<user>/…` and `/Users/<user>/…`.
    ///
    /// A pre-pass rather than a tokenizer rule, because `/` has to stay a
    /// delimiter for multiaddrs, which means the tokenizer can never see a
    /// path as one piece. Running before it also keeps the rule precise: only
    /// a segment directly following one of these two literal prefixes is a
    /// username, so the word after a bare "home" in prose is untouched.
    fn redact_home_paths(&mut self, line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        while let Some((at, plen)) = HOME_PREFIXES
            .iter()
            .filter_map(|p| rest.find(p).map(|i| (i, p.len())))
            .min_by_key(|(i, _)| *i)
        {
            let seg_start = at + plen;
            let seg_end = rest[seg_start..]
                .find(|c: char| c == '/' || is_delim(c))
                .map(|i| seg_start + i)
                .unwrap_or(rest.len());
            out.push_str(&rest[..seg_start]);
            let seg = &rest[seg_start..seg_end];
            if !seg.is_empty() {
                let replacement = self.token(Category::User, seg);
                out.push_str(&replacement);
            }
            rest = &rest[seg_end..];
        }
        out.push_str(rest);
        out
    }

    /// Replaces credentials inside URLs.
    ///
    /// Also a pre-pass, and for a sharper reason than home paths: the secret
    /// is a *path segment*, and the tokenizer splits on `/`, so by the time it
    /// runs the segment has lost the context that made it a secret. Judging a
    /// long opaque token by shape alone anywhere in a line would be far too
    /// eager and would eventually eat a hash that matters.
    fn redact_urls(&mut self, line: &str) -> String {
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(pos) = rest.find("://") {
            // Walk back over the scheme by char, never by byte, so a
            // multi-byte character before the scheme cannot split a boundary.
            let mut scheme_start = pos;
            for (i, c) in rest[..pos].char_indices().rev() {
                if c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-' {
                    scheme_start = i;
                } else {
                    break;
                }
            }
            let after = pos + 3;
            let end = rest[after..]
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(
                            c,
                            '"' | '\'' | '<' | '>' | ',' | ';' | ')' | ']' | '}' | '|'
                        )
                })
                .map(|i| after + i)
                .unwrap_or(rest.len());
            out.push_str(&rest[..scheme_start]);
            let url = rest[scheme_start..end].to_string();
            out.push_str(&self.redact_one_url(&url));
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }

    fn redact_one_url(&mut self, url: &str) -> String {
        let Some((scheme, remainder)) = url.split_once("://") else {
            return url.to_string();
        };
        let auth_end = remainder.find('/').unwrap_or(remainder.len());
        let (authority, path) = remainder.split_at(auth_end);
        // The host is left for the tokenizer, which redacts it only if it is
        // an IP. Which provider a node talks to is diagnostic; the key is not.
        let authority = match authority.rsplit_once('@') {
            Some((userinfo, host)) if !userinfo.is_empty() => {
                format!("{}@{}", self.token(Category::Secret, userinfo), host)
            }
            _ => authority.to_string(),
        };
        let mut path_out = String::new();
        for (i, seg) in path.split('/').enumerate() {
            if i > 0 {
                path_out.push('/');
            }
            if is_opaque_secret(seg) {
                let replacement = self.token(Category::Secret, seg);
                path_out.push_str(&replacement);
            } else {
                path_out.push_str(seg);
            }
        }
        format!("{scheme}://{authority}{path_out}")
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

    fn replace_token(&mut self, tok: &str, prev: Option<&str>) -> String {
        // Checked before the plain shapes: `10.0.0.5:8551` is a single token
        // (`:` is not a delimiter, so IPv6 survives), and only the address
        // half of it is sensitive.
        if let Some((host, port)) = split_host_port(tok) {
            if is_ipv4(host) {
                return format!("{}:{}", self.token(Category::Ip, host), port);
            }
        }
        if let Some(cat) = shape_of(tok) {
            return self.token(cat, tok);
        }
        if let Some(cat) = anchored(tok, prev) {
            return self.token(cat, tok);
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

    #[test]
    fn ipv4_addresses_are_redacted() {
        let mut r = Redactor::new();
        assert_eq!(
            r.redact("peer at 93.184.216.34 left"),
            "peer at <ip-1> left"
        );
    }

    /// The addresses inside a multiaddr have to come out even though the whole
    /// multiaddr reads as one word: `/` is a delimiter precisely so they do.
    #[test]
    fn ipv4_inside_a_multiaddr_is_redacted_and_the_ports_survive() {
        let mut r = Redactor::new();
        assert_eq!(
            r.redact("/ip4/93.184.216.34/tcp/9000"),
            "/ip4/<ip-1>/tcp/9000"
        );
    }

    /// `1.2.3.4:9000` is one token, since `:` is deliberately not a delimiter.
    /// The port is not sensitive and must survive.
    #[test]
    fn an_ipv4_with_a_port_keeps_the_port() {
        let mut r = Redactor::new();
        assert_eq!(r.redact("dialing 10.0.0.5:8551"), "dialing <ip-1>:8551");
    }

    #[test]
    fn ipv6_addresses_are_redacted() {
        let mut r = Redactor::new();
        assert_eq!(r.redact("peer at 2001:db8::1 left"), "peer at <ip-1> left");
        assert_eq!(
            r.redact("2001:0db8:0000:0000:0000:ff00:0042:8329"),
            "<ip-2>"
        );
    }

    /// The dangerous IPv6 false positive. `01:16:54` is two colons and three
    /// groups that are all valid hex, so a naive "colons plus hex" rule eats
    /// every Teku timestamp. Requiring either a `::` run or the full
    /// seven-colon form is what rejects it.
    #[test]
    fn a_bare_clock_time_is_not_mistaken_for_ipv6() {
        let mut r = Redactor::new();
        assert_eq!(r.redact("at 01:16:54 done"), "at 01:16:54 done");
        assert_eq!(r.redact("at 01:16:54.217 done"), "at 01:16:54.217 done");
    }

    #[test]
    fn libp2p_peer_ids_are_redacted() {
        let mut r = Redactor::new();
        let secp = "16Uiu2HAmPk9dR3aBcDeFgHjKmNpQrStUvWxYz1234567";
        let ed = "12D3KooWPk9dR3aBcDeFgHjKmNpQrStUvWxYz12345678";
        assert_eq!(r.redact(&format!("peer {secp}")), "peer <peer-1>");
        assert_eq!(r.redact(&format!("peer {ed}")), "peer <peer-2>");
    }

    /// A word that merely starts with the same letters, or is too short, is
    /// not a peer id. Over-matching here would eat ordinary log prose.
    #[test]
    fn short_or_non_base58_words_are_not_peer_ids() {
        let mut r = Redactor::new();
        assert_eq!(r.redact("Qm is short"), "Qm is short");
        assert_eq!(r.redact("16Uiu2HAshort"), "16Uiu2HAshort");
    }

    fn hex(n: usize) -> String {
        "a1b2c3d4".repeat(n / 8)
    }

    #[test]
    fn bls_pubkeys_are_redacted() {
        let mut r = Redactor::new();
        let pk = format!("0x{}", hex(96));
        assert_eq!(
            r.redact(&format!("validator {pk} active")),
            "validator <pubkey-1> active"
        );
    }

    #[test]
    fn execution_addresses_are_redacted() {
        let mut r = Redactor::new();
        let addr = format!("0x{}", hex(40));
        assert_eq!(
            r.redact(&format!("fee recipient {addr}")),
            "fee recipient <address-1>"
        );
    }

    /// The single most destructive over-match available in this file. A block
    /// root and a state root are both `0x` + 64 hex, they appear on nearly
    /// every line, and redacting them makes the dump worthless. Pubkeys (96)
    /// and addresses (40) are matched on *exact* length for this reason.
    #[test]
    fn block_and_state_roots_are_preserved() {
        let mut r = Redactor::new();
        let root = format!("0x{}", hex(64));
        let line = format!("head {root} finalized {root}");
        assert_eq!(r.redact(&line), line);
        assert_eq!(r.summary().values, 0);
    }

    #[test]
    fn validator_indices_are_redacted_when_a_keyword_anchors_them() {
        let mut r = Redactor::new();
        assert_eq!(
            r.redact("Validator 471293 missed"),
            "Validator <validator-1> missed"
        );
        assert_eq!(r.redact("index=471293"), "index=<validator-1>");
        assert_eq!(
            r.redact("validator_index: 8"),
            "validator_index: <validator-2>"
        );
    }

    /// Indices have no distinguishing shape - they are bare integers, exactly
    /// like slots, epochs, ports and counts. Anchoring is the only thing that
    /// separates them, so an unanchored integer must never be touched.
    #[test]
    fn unanchored_integers_are_preserved() {
        let mut r = Redactor::new();
        let line = "slot 12034887 epoch 376090 peers 42 took 1503 ms on port 9000";
        assert_eq!(r.redact(line), line);
        assert_eq!(r.summary().values, 0);
    }

    #[test]
    fn graffiti_is_redacted() {
        let mut r = Redactor::new();
        assert_eq!(
            r.redact("graffiti: \"my-node\""),
            "graffiti: \"<graffiti-1>\""
        );
    }

    #[test]
    fn hostnames_are_redacted_when_anchored() {
        let mut r = Redactor::new();
        assert_eq!(r.redact("host=validator-box-01"), "host=<host-1>");
        assert_eq!(r.redact("hostname: beacon-1"), "hostname: <host-2>");
    }

    /// `node` is deliberately NOT a host anchor even though it reads like an
    /// obvious one: Teku writes "node" constantly in ordinary prose, and
    /// anchoring on it redacts the next word every time.
    #[test]
    fn node_is_not_a_host_anchor() {
        let mut r = Redactor::new();
        assert_eq!(r.redact("node is syncing"), "node is syncing");
    }

    #[test]
    fn a_username_in_a_home_path_is_redacted_and_the_rest_of_the_path_survives() {
        let mut r = Redactor::new();
        assert_eq!(
            r.redact("data dir /home/lucas/teku/beacon"),
            "data dir /home/<user-1>/teku/beacon"
        );
        assert_eq!(
            r.redact("/Users/lucas/Library/teku"),
            "/Users/<user-1>/Library/teku"
        );
    }

    #[test]
    fn a_bare_home_prefix_with_no_segment_is_left_alone() {
        let mut r = Redactor::new();
        assert_eq!(r.redact("under /home/ somewhere"), "under /home/ somewhere");
    }

    /// The sleeper case. A hosted execution endpoint carries its API key in
    /// the URL path, and Teku logs its EL endpoint at startup.
    #[test]
    fn an_api_key_in_a_url_path_is_redacted_and_the_provider_survives() {
        let mut r = Redactor::new();
        assert_eq!(
            r.redact("eth1-endpoint https://mainnet.infura.io/v3/9f2c1a4b7d3e5f6a8b9c0d1e2f3a4b5c"),
            "eth1-endpoint https://mainnet.infura.io/v3/<secret-1>"
        );
    }

    #[test]
    fn url_userinfo_is_redacted_and_the_host_is_still_classified() {
        let mut r = Redactor::new();
        assert_eq!(
            r.redact("connecting to http://user:hunter2@10.0.0.5:8551"),
            "connecting to http://<secret-1>@<ip-1>:8551"
        );
    }

    /// Ordinary URL path words must survive, or every log line mentioning an
    /// endpoint turns to soup.
    #[test]
    fn ordinary_url_paths_are_preserved() {
        let mut r = Redactor::new();
        let line = "GET http://localhost:5051/eth/v1/node/syncing";
        assert_eq!(r.redact(line), line);
        assert_eq!(r.summary().values, 0);
    }

    #[test]
    fn trailing_punctuation_after_a_url_is_not_swallowed() {
        let mut r = Redactor::new();
        assert_eq!(
            r.redact("see (http://localhost:5051/eth), ok"),
            "see (http://localhost:5051/eth), ok"
        );
    }

    /// Everything a maintainer needs in order to read the dump at all.
    ///
    /// Over-redaction and under-redaction are both failures of this module,
    /// and this is the over-redaction half. If any of these starts being
    /// replaced, the command has stopped being useful even though it is still
    /// "safe".
    #[test]
    fn diagnostic_values_are_never_redacted() {
        let cases = [
            "2026-09-14 21:14:02.117 INFO  - Syncing",
            "01:16:54.217 WARN  - slow",
            "teku/v25.1.0",
            "besu/v24.9.1/linux-x86_64/openjdk-java-21",
            "slot 12034887 epoch 376090",
            "peers 42 in 8 out",
            "took 1503 ms, 4096 bytes",
            "port 9000 tcp 9001 udp",
            "0xa1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4",
            "java.lang.IllegalStateException: not ready",
            "tech.pegasys.teku.networking.p2p.libp2p.LibP2PNetwork",
            "Configuration | Network: hoodi",
            "GET /eth/v1/node/syncing 200",
            "MemAvailable: 16384 kB",
            "node is syncing",
        ];
        for case in cases {
            let mut r = Redactor::new();
            assert_eq!(
                r.redact(case),
                case,
                "should not have been redacted: {case}"
            );
            assert_eq!(r.summary().values, 0, "unexpected redaction in: {case}");
        }
    }

    /// The mirror of the table above: each category still fires. Guards
    /// against "fixing" an over-match by quietly disabling a rule outright.
    #[test]
    fn every_category_still_fires() {
        let mut r = Redactor::new();
        r.redact("93.184.216.34");
        r.redact("2001:db8::1");
        r.redact("16Uiu2HAmPk9dR3aBcDeFgHjKmNpQrStUvWxYz1234567");
        r.redact("enr:-AAA");
        r.redact(&format!("0x{}", hex(96)));
        r.redact(&format!("0x{}", hex(40)));
        r.redact("/home/lucas/x");
        r.redact("https://h.io/v3/9f2c1a4b7d3e5f6a8b9c0d1e");
        r.redact("validator 12");
        r.redact("graffiti: hi");
        r.redact("host=box");
        assert_eq!(r.summary().categories, 10);
    }
}
