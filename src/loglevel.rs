//! What `tekops log-level` is being asked to send, and where that came from.
//!
//! The level can be typed (`tekops log-level debug`) or fetched from a URL
//! holding the request body a Teku maintainer prepared - typically a gist,
//! since the whole point is handing an operator a set of logger names they
//! could not have worked out themselves.
//!
//! Everything here except `load` is a pure function over its arguments, the
//! same split `logs::resolve_log_target` and `completions::plan` use: the
//! argument handling, the URL rewriting and the body parsing are all testable
//! with no network and no environment.

use crate::curl::{self, CurlError};
use crate::term::sanitize;
use serde::Deserialize;
use std::fmt;

/// What the single positional of `tekops log-level` asked for.
#[derive(Debug, PartialEq)]
pub enum LogLevelTarget {
    /// A level typed on the command line, with any `--filter` values.
    Direct(LogLevelSpec),
    /// A URL to fetch the request body from.
    Url(String),
}

/// The body of a `PUT /teku/v1/admin/log_level` request, from either source.
#[derive(Debug, PartialEq)]
pub struct LogLevelSpec {
    pub level: String,
    pub log_filter: Option<Vec<String>>,
}

#[derive(Debug)]
pub enum LogLevelError {
    UnsupportedScheme(String),
    FilterWithUrl,
    Fetch(CurlError),
    Malformed {
        url: String,
        message: String,
    },
    /// Reading the answer to the confirmation prompt failed. The prompt lives
    /// in `cli.rs`, but the error vocabulary for the command lives here, the
    /// same way `CompletionError::Io` covers `confirm_install`.
    Io(String),
}

impl LogLevelError {
    /// The one constructor for a parse failure, so the sanitizing happens by
    /// construction rather than at each call site - the same shape
    /// `doctor::Finding::new` uses. serde quotes the offending key back, and
    /// that key arrived over the network on its way to a terminal.
    fn malformed(url: &str, message: impl fmt::Display) -> Self {
        LogLevelError::Malformed {
            url: url.to_string(),
            message: sanitize(&message.to_string()),
        }
    }
}

impl fmt::Display for LogLevelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogLevelError::UnsupportedScheme(url) => {
                write!(f, "only https urls can be fetched: {url}")
            }
            LogLevelError::FilterWithUrl => write!(
                f,
                "--filter cannot be combined with a url: the fetched body carries its own log_filter"
            ),
            LogLevelError::Fetch(e) => write!(f, "{e}"),
            LogLevelError::Malformed { url, message } => {
                write!(f, "{url} did not serve a log level request body: {message}")
            }
            LogLevelError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<CurlError> for LogLevelError {
    fn from(e: CurlError) -> Self {
        LogLevelError::Fetch(e)
    }
}

/// Splits the one positional of `tekops log-level` into a level or a URL.
///
/// The discriminator is `://`: no log level contains one. This mirrors
/// `resolve_logs_target` and `resolve_update_target`, which exist because an
/// optional positional alongside subcommands is what made `tekops logs
/// /var/log/x.log` fail with `invalid value for [SOURCE]`.
pub fn resolve_log_level_target(
    arg: String,
    log_filter: Vec<String>,
) -> Result<LogLevelTarget, LogLevelError> {
    if !arg.contains("://") {
        return Ok(LogLevelTarget::Direct(LogLevelSpec {
            level: arg,
            log_filter: (!log_filter.is_empty()).then_some(log_filter),
        }));
    }
    if !log_filter.is_empty() {
        return Err(LogLevelError::FilterWithUrl);
    }
    if !arg.starts_with("https://") {
        return Err(LogLevelError::UnsupportedScheme(arg));
    }
    Ok(LogLevelTarget::Url(arg))
}

/// Rewrites a gist page URL to the URL that serves the file's bytes.
///
/// `https://gist.github.com/<owner>/<id>` serves HTML; appending `/raw`
/// redirects to `gist.githubusercontent.com` and serves the file as
/// `text/plain` - verified against real gists, along with the fact that a
/// multi-file gist serves only its first file that way, which is why USAGE.md
/// says to put one file in the gist.
///
/// A fragment or query has to go first: a link copied from the browser for one
/// file of a multi-file gist carries `#file-something-json`, and appending
/// `/raw` after that would ask for a path that does not exist.
///
/// Anything that is not a gist page URL is returned untouched. tekops accepts
/// any https URL that serves the body, and rewriting one it does not recognise
/// would be a guess about someone else's URL scheme.
pub fn raw_url(url: &str) -> String {
    const PAGE_HOST: &str = "https://gist.github.com/";

    // Only a URL we recognise gets trimmed: a query string elsewhere may be
    // the whole point of the link, as it is on a signed URL.
    if url.strip_prefix(PAGE_HOST).is_none() {
        return url.to_string();
    }

    let trimmed = url.split_once('#').map_or(url, |(before, _)| before);
    let trimmed = trimmed
        .split_once('?')
        .map_or(trimmed, |(before, _)| before);
    let trimmed = trimmed.trim_end_matches('/');

    if trimmed.split('/').any(|segment| segment == "raw") {
        return trimmed.to_string();
    }
    format!("{trimmed}/raw")
}

/// Parses the body a URL served into the request tekops will send.
///
/// `deny_unknown_fields` is load-bearing: a gist with `log_filters` or `levl`
/// in it would otherwise apply a body missing the very thing it was written to
/// carry, and report success. Teku's endpoint takes these two keys and nothing
/// else, so an unknown one is always a mistake worth stopping on.
///
/// The message text is sanitized because serde quotes the offending key back,
/// and that key came off the network - the same channel class `term::sanitize`
/// exists for.
pub fn parse_spec(body: &[u8], url: &str) -> Result<LogLevelSpec, LogLevelError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        level: String,
        log_filter: Option<Vec<String>>,
    }

    let parsed: Body =
        serde_json::from_slice(body).map_err(|e| LogLevelError::malformed(url, e))?;
    if parsed.level.trim().is_empty() {
        return Err(LogLevelError::malformed(url, "level is empty"));
    }
    Ok(LogLevelSpec {
        level: parsed.level,
        // An empty list means the same thing as a missing key, and has to
        // arrive as the same `None`: `set_log_level` omits `log_filter`
        // entirely for a global change rather than sending `[]`.
        log_filter: parsed.log_filter.filter(|f| !f.is_empty()),
    })
}

/// A log level request body is a handful of logger names. The cap is generous
/// for that and still small enough that a URL pointing somewhere unexpected
/// costs a validator host nothing.
const MAX_BODY_BYTES: u64 = 64 * 1024;

/// Fetches a URL and parses what it served into a request body.
///
/// The one piece of I/O in this module, kept to a wrapper around the two pure
/// functions above - the same `collect`/parser split `host.rs` uses.
pub fn load(url: &str) -> Result<LogLevelSpec, LogLevelError> {
    load_with(|u| curl::fetch_capped(u, MAX_BODY_BYTES), url)
}

fn load_with<F>(fetch: F, url: &str) -> Result<LogLevelSpec, LogLevelError>
where
    F: Fn(&str) -> Result<Vec<u8>, CurlError>,
{
    let body = fetch(&raw_url(url))?;
    // Reported against the URL the operator typed, not the rewritten one they
    // have never seen.
    parse_spec(&body, url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_level_is_sent_as_typed() {
        let target = resolve_log_level_target("DEBUG".to_string(), Vec::new())
            .expect("a level is a valid argument");
        assert_eq!(
            target,
            LogLevelTarget::Direct(LogLevelSpec {
                level: "DEBUG".to_string(),
                log_filter: None,
            })
        );
    }

    /// An empty `--filter` list must leave `log_filter` absent rather than
    /// empty, since the request body omits the key entirely for a global
    /// change - see `BeaconClient::set_log_level`.
    #[test]
    fn filters_scope_a_typed_level() {
        let target = resolve_log_level_target(
            "DEBUG".to_string(),
            vec!["tech.pegasys.teku.sync".to_string()],
        )
        .expect("a level with filters is valid");
        assert_eq!(
            target,
            LogLevelTarget::Direct(LogLevelSpec {
                level: "DEBUG".to_string(),
                log_filter: Some(vec!["tech.pegasys.teku.sync".to_string()]),
            })
        );
    }

    #[test]
    fn an_https_argument_is_a_url() {
        let target = resolve_log_level_target(
            "https://gist.github.com/someone/abc123".to_string(),
            Vec::new(),
        )
        .expect("an https url is a valid argument");
        assert_eq!(
            target,
            LogLevelTarget::Url("https://gist.github.com/someone/abc123".to_string())
        );
    }

    /// curl is invoked with `--proto =https` and would reject this itself, but
    /// with a message about protocols that reads like a tekops bug rather than
    /// like a URL the operator should look at again.
    #[test]
    fn a_url_that_is_not_https_is_refused_before_curl_sees_it() {
        let err = resolve_log_level_target("http://example.com/body.json".to_string(), Vec::new())
            .expect_err("plain http must not be accepted");
        assert!(
            matches!(&err, LogLevelError::UnsupportedScheme(url) if url == "http://example.com/body.json"),
            "got {err:?}"
        );
    }

    #[test]
    fn a_gist_page_url_is_rewritten_to_its_raw_form() {
        assert_eq!(
            raw_url("https://gist.github.com/someone/abc123"),
            "https://gist.github.com/someone/abc123/raw"
        );
    }

    /// The link the browser copies for one file of a multi-file gist. The
    /// fragment has to be dropped before `/raw` is appended, or the resulting
    /// path is nonsense.
    #[test]
    fn a_fragment_is_dropped_before_the_raw_suffix() {
        assert_eq!(
            raw_url("https://gist.github.com/someone/abc123#file-log-level-json"),
            "https://gist.github.com/someone/abc123/raw"
        );
    }

    #[test]
    fn a_query_string_is_dropped_before_the_raw_suffix() {
        assert_eq!(
            raw_url("https://gist.github.com/someone/abc123?foo=bar"),
            "https://gist.github.com/someone/abc123/raw"
        );
    }

    #[test]
    fn a_gist_url_that_is_already_raw_is_left_alone() {
        assert_eq!(
            raw_url("https://gist.github.com/someone/abc123/raw"),
            "https://gist.github.com/someone/abc123/raw"
        );
        assert_eq!(
            raw_url("https://gist.github.com/someone/abc123/raw/log-level.json"),
            "https://gist.github.com/someone/abc123/raw/log-level.json"
        );
    }

    /// The host that actually serves the bytes. Appending `/raw` again would
    /// 404.
    #[test]
    fn a_raw_gist_host_url_is_left_alone() {
        let url = "https://gist.githubusercontent.com/someone/abc123/raw/x/log-level.json";
        assert_eq!(raw_url(url), url);
    }

    /// tekops accepts any https URL that serves the body. Rewriting a URL it
    /// does not recognise would be a guess about someone else's paths.
    #[test]
    fn a_non_gist_url_is_left_alone() {
        let url = "https://example.com/teku/log-level.json";
        assert_eq!(raw_url(url), url);
    }

    #[test]
    fn a_body_with_a_level_and_filters_parses() {
        let spec = parse_spec(
            br#"{"level": "DEBUG", "log_filter": ["tech.pegasys.teku.sync"]}"#,
            "https://example.com/x",
        )
        .expect("a well-formed body must parse");
        assert_eq!(
            spec,
            LogLevelSpec {
                level: "DEBUG".to_string(),
                log_filter: Some(vec!["tech.pegasys.teku.sync".to_string()]),
            }
        );
    }

    #[test]
    fn a_body_with_only_a_level_leaves_the_filter_absent() {
        let spec = parse_spec(br#"{"level": "INFO"}"#, "https://example.com/x")
            .expect("a global change is a valid body");
        assert_eq!(spec.log_filter, None);
    }

    #[test]
    fn a_body_without_a_level_is_refused() {
        let err = parse_spec(
            br#"{"log_filter": ["tech.pegasys.teku.sync"]}"#,
            "https://example.com/x",
        )
        .expect_err("a body with no level must not parse");
        assert!(err.to_string().contains("level"), "got {err}");
    }

    /// The whole failure this guards against: a body that looks like it scopes
    /// the change but does not, applied and reported as a success.
    #[test]
    fn a_body_with_an_unknown_key_is_refused() {
        let err = parse_spec(
            br#"{"level": "DEBUG", "log_filters": ["tech.pegasys.teku.sync"]}"#,
            "https://example.com/x",
        )
        .expect_err("an unknown key must not parse");
        assert!(err.to_string().contains("log_filters"), "got {err}");
    }

    /// What a page URL that was not rewritten to its raw form actually serves.
    #[test]
    fn a_body_that_is_not_json_is_refused() {
        let err = parse_spec(b"<!DOCTYPE html><html>", "https://example.com/x")
            .expect_err("html must not parse");
        assert!(
            err.to_string().contains("https://example.com/x"),
            "the url the operator typed must be in the message: {err}"
        );
    }

    /// serde quotes the offending key back, and that key came off the network.
    #[test]
    fn a_parse_failure_cannot_carry_escape_sequences_to_the_terminal() {
        let err = parse_spec(
            b"{\"level\": \"DEBUG\", \"boom\\u001b[2Jgone\": 1}",
            "https://example.com/x",
        )
        .expect_err("an unknown key must not parse");
        assert!(
            !err.to_string().contains('\u{1b}'),
            "escape survived: {err:?}"
        );
    }

    /// A level of "" would reach Teku as a 400 that says nothing about where
    /// it came from. Absent and empty are not the same answer, the same
    /// distinction `require_metric` draws in `metrics.rs`.
    #[test]
    fn a_body_with_an_empty_level_is_refused() {
        let err = parse_spec(br#"{"level": "   "}"#, "https://example.com/x")
            .expect_err("an empty level must not parse");
        assert!(err.to_string().contains("level"), "got {err}");
    }

    #[test]
    fn load_fetches_the_raw_url_and_parses_the_body() {
        let asked: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let spec = load_with(
            |url| {
                asked.borrow_mut().push(url.to_string());
                Ok(br#"{"level": "TRACE"}"#.to_vec())
            },
            "https://gist.github.com/someone/abc123",
        )
        .expect("a well-formed body must load");

        assert_eq!(spec.level, "TRACE");
        assert_eq!(
            asked.into_inner(),
            vec!["https://gist.github.com/someone/abc123/raw".to_string()],
            "the page url must be rewritten before it is fetched"
        );
    }

    #[test]
    fn a_fetch_failure_propagates() {
        let err = load_with(|_| Err(CurlError::Missing), "https://example.com/x")
            .expect_err("a fetch failure must not be swallowed");
        assert!(
            matches!(err, LogLevelError::Fetch(CurlError::Missing)),
            "got {err:?}"
        );
    }

    /// The message has to name the URL the operator typed, not the rewritten
    /// one they have never seen.
    #[test]
    fn a_malformed_body_is_reported_against_the_url_that_was_typed() {
        let err = load_with(
            |_| Ok(b"<!DOCTYPE html>".to_vec()),
            "https://gist.github.com/someone/abc123",
        )
        .expect_err("html must not load");
        assert!(
            err.to_string()
                .contains("https://gist.github.com/someone/abc123")
                && !err.to_string().contains("/raw"),
            "got {err}"
        );
    }

    /// `set_log_level` omits the key entirely for a global change rather than
    /// sending `null` or `[]`, so an empty list in the body has to arrive here
    /// as the same absence a missing key produces.
    #[test]
    fn an_empty_filter_list_in_the_body_reads_as_a_global_change() {
        let spec = parse_spec(
            br#"{"level": "INFO", "log_filter": []}"#,
            "https://example.com/x",
        )
        .expect("an empty list is a valid body");
        assert_eq!(spec.log_filter, None);
    }

    /// The fetched body carries its own `log_filter`. Honouring one of the two
    /// and discarding the other silently is the failure this repo keeps
    /// designing against, so neither wins - the combination is an error.
    #[test]
    fn a_url_cannot_be_combined_with_filters() {
        let err = resolve_log_level_target(
            "https://gist.github.com/someone/abc123".to_string(),
            vec!["tech.pegasys.teku.sync".to_string()],
        )
        .expect_err("--filter alongside a url must not be accepted");
        assert!(matches!(err, LogLevelError::FilterWithUrl), "got {err:?}");
    }
}
