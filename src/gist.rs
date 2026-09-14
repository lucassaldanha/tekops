//! GitHub gist semantics for `tekops dump-logs --gist`.
//!
//! Request shape, auth material and response parsing. **No transport policy
//! lives here**: tekops builds ureq with no TLS backend on purpose, so every
//! HTTPS call goes through `curl.rs`, and this module hands it a URL, a config
//! file, a body file and the headers it wants. `curl::post_argv` owns the
//! protocol pin, the stall bound, `--data-binary` and the absent `-f`.

use crate::curl;
use crate::term::sanitize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

pub const GIST_API_URL: &str = "https://api.github.com/gists";

const DESCRIPTION: &str = "tekops dump-logs";

/// What GitHub's API wants on the request. Semantics, so they live here rather
/// than in the transport.
const GIST_HEADERS: [&str; 2] = [
    "Accept: application/vnd.github+json",
    "Content-Type: application/json",
];

#[derive(Debug)]
pub enum GistError {
    MissingToken,
    EmptyToken,
    MalformedToken,
    Rejected { status: String, message: String },
    Malformed(String),
    Transport(String),
}

impl fmt::Display for GistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GistError::MissingToken => write!(
                f,
                "--gist needs a GitHub token\n\
                 hint: set $GITHUB_TOKEN (or $GH_TOKEN) to a personal access token with the\n\
                 'gist' scope, or drop --gist to write a local file"
            ),
            GistError::EmptyToken => write!(f, "the GitHub token is set but empty"),
            GistError::MalformedToken => write!(
                f,
                "the GitHub token contains a quote, backslash or control character"
            ),
            GistError::Rejected { status, message } => {
                write!(f, "github rejected the gist (HTTP {status}): {message}")
            }
            GistError::Malformed(m) => write!(f, "could not read github's response: {m}"),
            GistError::Transport(m) => write!(f, "could not upload the gist: {m}"),
        }
    }
}

#[derive(Serialize)]
struct GistFile {
    content: String,
}

#[derive(Serialize)]
struct GistRequest {
    description: &'static str,
    public: bool,
    files: BTreeMap<String, GistFile>,
}

/// The request body.
///
/// `public: false` is a secret gist: unlisted and not indexed, still reachable
/// by anyone holding the link. That is what "paste this to a maintainer"
/// needs, and it is the only mode offered - there is deliberately no
/// `--public`.
///
/// Built with serde rather than `format!`, which is the repo's standing rule
/// and matters more here than anywhere else: the payload is an entire log
/// file, so hand-building it would be a guaranteed quoting bug.
pub fn gist_body(filename: &str, content: &str) -> Result<String, GistError> {
    let mut files = BTreeMap::new();
    files.insert(
        filename.to_string(),
        GistFile {
            content: content.to_string(),
        },
    );
    serde_json::to_string(&GistRequest {
        description: DESCRIPTION,
        public: false,
        files,
    })
    .map_err(|e| GistError::Malformed(e.to_string()))
}

/// The curl config file carrying the `Authorization` header.
///
/// This file is the entire reason the token never reaches argv:
/// `/proc/<pid>/cmdline` is world-readable on Linux, so `-H "Authorization:
/// Bearer ..."` would publish the token to every user on the node for the life
/// of the request - and the node's other tenant is a validator. Callers must
/// create it with mode 0600.
pub fn config_file_contents(token: &str) -> String {
    format!("header = \"Authorization: Bearer {token}\"\n")
}

/// Rejects a token that would break, or escape, the config file's quoting.
///
/// A newline is the sharp one: it would let a mis-set variable append a second
/// directive to curl's config.
pub fn validate_token(token: &str) -> Result<(), GistError> {
    if token.is_empty() {
        return Err(GistError::EmptyToken);
    }
    if token
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control())
    {
        return Err(GistError::MalformedToken);
    }
    Ok(())
}

/// Parses curl's combined output: the response body, then a final line holding
/// the HTTP status.
pub fn parse_gist_response(raw: &str) -> Result<String, GistError> {
    let trimmed = raw.trim_end_matches('\n');
    let (body, status) = trimmed.rsplit_once('\n').unwrap_or(("", trimmed));
    let status = status.trim();

    if status != "201" {
        let message = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_else(|| "no message".to_string());
        return Err(GistError::Rejected {
            status: sanitize(status),
            message: sanitize(&message),
        });
    }

    let v: Value =
        serde_json::from_str(body).map_err(|e| GistError::Malformed(sanitize(&e.to_string())))?;
    v.get("html_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| GistError::Malformed("response contained no html_url".to_string()))
}

/// Creates the two 0600 temp files, runs curl, parses the answer.
///
/// Both the auth header and the body go through files rather than argv: the
/// first because argv is world-readable, the second because the payload is an
/// entire log file and would exceed `ARG_MAX` regardless.
pub fn upload(token: &str, filename: &str, content: &str) -> Result<String, GistError> {
    validate_token(token)?;
    let body = gist_body(filename, content)?;

    let dir = tempfile::Builder::new()
        .prefix("tekops-gist-")
        .tempdir()
        .map_err(|e| GistError::Transport(e.to_string()))?;
    let config_path = dir.path().join("curl.cfg");
    let body_path = dir.path().join("body.json");
    write_private(&config_path, &config_file_contents(token))?;
    write_private(&body_path, &body)?;

    let out = curl::post_file(GIST_API_URL, &config_path, &body_path, &GIST_HEADERS)
        .map_err(|e| GistError::Transport(e.to_string()))?;
    parse_gist_response(&String::from_utf8_lossy(&out))
}

fn write_private(path: &Path, content: &str) -> Result<(), GistError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| GistError::Transport(e.to_string()))?;
    f.write_all(content.as_bytes())
        .map_err(|e| GistError::Transport(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_is_a_secret_gist_with_one_named_file() {
        let body = gist_body("tekops-dump.txt", "line one\nline two\n").unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["public"], Value::Bool(false));
        assert_eq!(
            v["files"]["tekops-dump.txt"]["content"],
            "line one\nline two\n"
        );
    }

    /// The payload is a whole log file. Hand-building this with `format!`
    /// would be a guaranteed quoting bug, which is why the repo's rule is
    /// serde everywhere for JSON.
    #[test]
    fn content_with_quotes_and_newlines_round_trips() {
        let nasty = "he said \"hi\"\n\tand \\escaped\\ it\n";
        let body = gist_body("f.txt", nasty).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["files"]["f.txt"]["content"], nasty);
    }

    #[test]
    fn the_token_goes_in_the_config_file() {
        let cfg = config_file_contents("ghp_secret");
        assert!(cfg.contains("Authorization: Bearer ghp_secret"));
        assert!(cfg.starts_with("header = "));
    }

    /// A quote, a backslash or a newline in the token would either break
    /// curl's config quoting or let a mis-set variable append another config
    /// directive.
    #[test]
    fn a_token_that_could_break_the_config_file_is_rejected() {
        assert!(validate_token("ghp_ok123").is_ok());
        assert!(validate_token("").is_err());
        assert!(validate_token("gh\"p").is_err());
        assert!(validate_token("gh\\p").is_err());
        assert!(validate_token("ghp\nheader = \"X: y\"").is_err());
    }

    #[test]
    fn a_201_response_yields_the_html_url() {
        let raw = "{\"html_url\":\"https://gist.github.com/u/abc123\"}\n201";
        assert_eq!(
            parse_gist_response(raw).unwrap(),
            "https://gist.github.com/u/abc123"
        );
    }

    /// The reason `-f` is not used: GitHub puts the actionable text in the
    /// body, and `-f` throws the body away.
    #[test]
    fn a_rejection_surfaces_githubs_own_message() {
        let raw = "{\"message\":\"Bad credentials\"}\n401";
        let err = parse_gist_response(raw).unwrap_err();
        assert!(err.to_string().contains("401"));
        assert!(err.to_string().contains("Bad credentials"));
    }

    #[test]
    fn a_201_without_an_html_url_is_malformed_rather_than_a_success() {
        let raw = "{\"id\":\"abc\"}\n201";
        assert!(parse_gist_response(raw).is_err());
    }

    /// An error body is attacker-influenced text on its way to a terminal.
    #[test]
    fn a_rejection_message_is_sanitized() {
        let raw = "{\"message\":\"a\u{1b}[31mred\"}\n422";
        let err = parse_gist_response(raw).unwrap_err().to_string();
        assert!(!err.contains('\u{1b}'));
    }
}
