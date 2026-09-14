//! GitHub gist semantics for `tekops dump-logs --gist`.
//!
//! Request shape, auth material and response parsing. The transport belongs to
//! the shared curl boundary, because tekops builds ureq with no TLS backend on
//! purpose and every HTTPS call shells out to curl - see the `upload_argv`
//! note for why a copy of that policy currently sits here and when it leaves.

use crate::term::sanitize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::Command;

pub const GIST_API_URL: &str = "https://api.github.com/gists";

const DESCRIPTION: &str = "tekops dump-logs";

const CURL: &str = "curl";

/// Seconds to wait for the connection, and seconds of zero progress before
/// giving up.
///
/// TRANSPORT SHIM, TEMPORARY. These constants and `upload_argv` are a local
/// copy of update.rs's policy, written here only because `src/curl.rs` does
/// not exist in this branch yet - issue #11 lands it first. Once it does,
/// `upload_argv` moves into it as `post_argv` and everything in this block is
/// deleted. tekops must have exactly one transport boundary; this is not the
/// final state.
const CONNECT_TIMEOUT_SECS: u64 = 10;
const STALL_TIMEOUT_SECS: u64 = 30;

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

/// The argv for the upload, with the token deliberately absent.
///
/// `-sS` rather than `-f`, and that is a decision rather than an omission.
/// `-f` makes curl discard the response body on an HTTP error, and GitHub's
/// body is exactly where "Bad credentials" and the missing-scope message live.
/// Dropping it means curl exits 0 on a 4xx and **this caller owns deciding
/// what a non-2xx means** - `parse_gist_response` does, treating anything but
/// 201 as a rejection carrying GitHub's own text. `-w` appends the status code
/// on its own final line for it to split back off. `--fail-with-body` would
/// give both at once but needs curl >= 7.76, and Debian 11 ships 7.74.
///
/// `--data-binary`, never `-d`: `-d @file` strips CR and LF out of the file's
/// content. That is documented behaviour of the form, and while it is usually
/// survivable for JSON, since those bytes are inter-token whitespace, this
/// body is built from arbitrary log text and "usually" is not a property to
/// rest a log dump on.
pub fn upload_argv(config: &Path, body: &Path) -> Vec<String> {
    vec![
        "-sS".to_string(),
        "-X".to_string(),
        "POST".to_string(),
        "--proto".to_string(),
        "=https".to_string(),
        "--connect-timeout".to_string(),
        CONNECT_TIMEOUT_SECS.to_string(),
        "--speed-limit".to_string(),
        "1".to_string(),
        "--speed-time".to_string(),
        STALL_TIMEOUT_SECS.to_string(),
        "-H".to_string(),
        "Accept: application/vnd.github+json".to_string(),
        "-H".to_string(),
        "Content-Type: application/json".to_string(),
        "--config".to_string(),
        config.display().to_string(),
        "--data-binary".to_string(),
        format!("@{}", body.display()),
        "-w".to_string(),
        "\n%{http_code}".to_string(),
        GIST_API_URL.to_string(),
    ]
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

    let out = Command::new(CURL)
        .args(upload_argv(&config_path, &body_path))
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                GistError::Transport("`curl` was not found on this host".to_string())
            }
            _ => GistError::Transport(e.to_string()),
        })?;

    if !out.status.success() {
        return Err(GistError::Transport(format!(
            "curl exited {}: {}",
            out.status,
            sanitize(String::from_utf8_lossy(&out.stderr).trim())
        )));
    }
    parse_gist_response(&String::from_utf8_lossy(&out.stdout))
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

    /// The single most important property in this module. `/proc/<pid>/cmdline`
    /// is world-readable on Linux, so a token in argv is a token published to
    /// every user on the node.
    #[test]
    fn the_token_never_appears_in_argv() {
        let argv = upload_argv(Path::new("/tmp/cfg"), Path::new("/tmp/body"));
        let joined = argv.join(" ").to_lowercase();
        assert!(!joined.contains("ghp_"));
        assert!(!joined.contains("authorization"));
        assert!(!joined.contains("bearer"));
    }

    #[test]
    fn upload_argv_posts_the_body_from_a_file_and_asks_for_the_status() {
        let argv = upload_argv(Path::new("/tmp/cfg"), Path::new("/tmp/body"));
        let at = |flag: &str| {
            argv.iter()
                .position(|a| a == flag)
                .map(|i| argv[i + 1].clone())
                .unwrap_or_else(|| panic!("no {flag}"))
        };
        assert_eq!(at("--config"), "/tmp/cfg");
        assert_eq!(at("-X"), "POST");
        assert!(argv.contains(&GIST_API_URL.to_string()));
        // No -f: it would discard the error body we parse.
        assert!(!argv.iter().any(|a| a == "-f" || a == "-fsS"));
        // The status has to come back in-band.
        assert!(argv.iter().any(|a| a.contains("%{http_code}")));
    }

    /// `-d @file` strips CR and LF out of the file's content - documented
    /// behaviour of that form. For well-formed JSON it is usually survivable,
    /// since those are inter-token whitespace, but this payload is built from
    /// arbitrary log text. `--data-binary` sends the bytes untouched.
    #[test]
    fn the_body_is_sent_binary_so_newlines_are_not_stripped() {
        let argv = upload_argv(Path::new("/tmp/cfg"), Path::new("/tmp/body"));
        assert!(argv.contains(&"--data-binary".to_string()));
        assert!(argv.contains(&"@/tmp/body".to_string()));
        assert!(!argv.iter().any(|a| a == "-d"));
    }

    /// The two invariants that must survive being copied onto a new verb. The
    /// protocol pin is what stops a redirect downgrading to http; the stall
    /// bound is what stops a black-hole host hanging the process forever,
    /// since -s suppresses even the progress meter.
    #[test]
    fn the_post_keeps_the_protocol_pin_and_the_stall_bound() {
        let argv = upload_argv(Path::new("/tmp/cfg"), Path::new("/tmp/body"));
        let at = |flag: &str| {
            argv.iter()
                .position(|a| a == flag)
                .map(|i| argv[i + 1].clone())
                .unwrap_or_else(|| panic!("no {flag}"))
        };
        assert_eq!(at("--proto"), "=https");
        assert_eq!(at("--connect-timeout"), CONNECT_TIMEOUT_SECS.to_string());
        assert_eq!(at("--speed-limit"), "1");
        assert_eq!(at("--speed-time"), STALL_TIMEOUT_SECS.to_string());
    }
}
