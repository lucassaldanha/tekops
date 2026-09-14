//! The HTTPS boundary, shelled out to `curl`.
//!
//! tekops drops ureq's TLS backend on purpose (see the dependency comment in
//! Cargo.toml), so anything that has to speak HTTPS - GitHub Releases for
//! `update`, a gist for `log-level` - cannot use the ureq clients and needs a
//! transport from outside this binary. `curl` is documented as a runtime
//! dependency alongside `tail`, `less`, and `tar`.
//!
//! This module exists for the same reason `http.rs` does: it was `update.rs`'s
//! private plumbing until a second command needed HTTPS, and transport policy
//! belongs in one place rather than in whichever module happened to need it
//! first.

use crate::term::sanitize;
use std::fmt;
use std::io;
use std::process::Command;

const CURL: &str = "curl";

#[derive(Debug)]
pub enum CurlError {
    Missing,
    Failed {
        url: String,
        status: String,
        stderr: String,
        hint: Option<&'static str>,
    },
    Io(String),
}

impl fmt::Display for CurlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CurlError::Missing => {
                write!(
                    f,
                    "curl is required for this command but was not found on PATH"
                )
            }
            CurlError::Failed {
                url,
                status,
                stderr,
                hint,
            } => {
                write!(f, "could not download {url} ({status}): {stderr}")?;
                match hint {
                    Some(hint) => write!(f, "\n{hint}"),
                    None => Ok(()),
                }
            }
            CurlError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

const CONNECT_TIMEOUT_SECS: u64 = 10;
const STALL_TIMEOUT_SECS: u64 = 30;

/// The bound that stops a wedged endpoint from hanging the command forever.
///
/// curl applies no timeout of its own by default, so without this a host that
/// accepts the connection and then never answers leaves the command waiting
/// with no output at all - `-s` suppresses even the progress meter. That is the
/// same failure `http::agent` exists to prevent on the ureq side, and it was
/// reproduced here against a black-hole socket.
///
/// It is a stall bound rather than `--max-time` on purpose: a release tarball
/// is megabytes, and a slow but progressing download over a node's link must
/// not be killed by a wall clock. `--speed-limit 1 --speed-time N` gives up
/// only when nothing arrives for N seconds, which covers both a server that
/// never sends headers and one that dies mid-transfer.
fn stall_argv(stall_secs: u64) -> Vec<String> {
    vec![
        "--connect-timeout".to_string(),
        CONNECT_TIMEOUT_SECS.to_string(),
        "--speed-limit".to_string(),
        "1".to_string(),
        "--speed-time".to_string(),
        stall_secs.to_string(),
    ]
}

fn curl_argv(url: &str, max_bytes: Option<u64>) -> Vec<String> {
    let mut argv = vec![
        "-fsSL".to_string(),
        "--proto".to_string(),
        "=https".to_string(),
    ];
    argv.extend(stall_argv(STALL_TIMEOUT_SECS));
    if let Some(max_bytes) = max_bytes {
        argv.push("--max-filesize".to_string());
        argv.push(max_bytes.to_string());
    }
    argv.push(url.to_string());
    argv
}

/// The single HTTPS boundary. Every network read outside the ureq clients goes
/// through here, so the transport policy lives in exactly one place - the same
/// reason `http::agent()` exists for those clients.
///
/// Uncapped, for a body whose size is not knowable in advance: a release
/// tarball is megabytes today and a ceiling picked now would eventually fail an
/// update for being right about the wrong release. Callers fetching something
/// that is supposed to be small want `fetch_capped` instead.
pub fn fetch(url: &str) -> Result<Vec<u8>, CurlError> {
    fetch_with(CURL, url, None)
}

/// Fetches a body that is expected to be small, refusing anything past
/// `max_bytes`.
///
/// The whole response is held in memory, and tekops runs on the machine running
/// the validator - a mistyped URL pointing at something enormous must not cost
/// that host its RAM. curl checks the declared `Content-Length` first, so an
/// obvious mistake usually costs one round trip rather than the transfer.
pub fn fetch_capped(url: &str, max_bytes: u64) -> Result<Vec<u8>, CurlError> {
    fetch_with(CURL, url, Some(max_bytes))
}

fn fetch_with(program: &str, url: &str, max_bytes: Option<u64>) -> Result<Vec<u8>, CurlError> {
    let output = Command::new(program)
        .args(curl_argv(url, max_bytes))
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => CurlError::Missing,
            _ => CurlError::Io(e.to_string()),
        })?;

    if !output.status.success() {
        return Err(CurlError::Failed {
            url: url.to_string(),
            status: output.status.to_string(),
            stderr: sanitize(String::from_utf8_lossy(&output.stderr).trim()),
            hint: None,
        });
    }
    Ok(output.stdout)
}

/// Attaches the reading that fits the hop that failed.
///
/// The same curl failure means different things at different URLs: a 404 on an
/// asset points at the platform, a 404 on the release API points at the
/// repository. Hanging one hint off every curl failure - which is what this
/// used to do - sends the operator looking in the wrong place, and a private
/// repo is exactly the case that produces the wrong one.
pub fn with_hint(err: CurlError, hint: &'static str) -> CurlError {
    match err {
        CurlError::Failed {
            url,
            status,
            stderr,
            ..
        } => CurlError::Failed {
            url,
            status,
            stderr,
            hint: Some(hint),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-f` turns an HTTP error status into a nonzero exit instead of a body
    /// of HTML; `-L` is required because GitHub redirects release downloads to
    /// objects.githubusercontent.com (and a gist page URL to
    /// gist.githubusercontent.com); `--proto =https` pins every hop of that
    /// redirect chain to https so a redirect cannot downgrade the transport.
    #[test]
    fn curl_argv_pins_the_protocol_and_follows_redirects() {
        let argv = curl_argv("https://example.invalid/x", None);
        assert!(argv.contains(&"-fsSL".to_string()), "argv was {argv:?}");
        let proto = argv
            .iter()
            .position(|a| a == "--proto")
            .expect("no --proto");
        assert_eq!(argv[proto + 1], "=https");
        assert_eq!(argv.last().unwrap(), "https://example.invalid/x");
    }

    #[test]
    fn a_missing_curl_is_reported_as_such() {
        let err = fetch_with(
            "tekops-no-such-program-exists",
            "https://example.invalid/x",
            None,
        )
        .expect_err("a missing program must not succeed");
        assert!(matches!(err, CurlError::Missing), "got {err:?}");
        assert!(err.to_string().contains("curl"));
    }

    /// Stands in for curl to prove the plumbing: the child is spawned with our
    /// argv and its stdout is what comes back.
    #[test]
    fn a_successful_program_returns_its_stdout() {
        let out =
            fetch_with("echo", "https://example.invalid/x", None).expect("echo should succeed");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("https://example.invalid/x"), "got {text:?}");
    }

    #[test]
    fn a_nonzero_exit_is_reported_with_the_url() {
        let err = fetch_with("false", "https://example.invalid/x", None)
            .expect_err("a failing program must not succeed");
        assert!(matches!(err, CurlError::Failed { .. }), "got {err:?}");
        assert!(err.to_string().contains("https://example.invalid/x"));
    }

    /// curl's stderr is untrusted text on its way to the operator's terminal,
    /// the same class of input `map_ureq_error` sanitizes.
    #[test]
    fn curl_stderr_is_sanitized_into_the_error() {
        let err = CurlError::Failed {
            url: "https://example.invalid/x".into(),
            status: "exit status: 22".into(),
            stderr: crate::term::sanitize("boom\u{1b}[2Jgone"),
            hint: None,
        };
        assert!(
            !err.to_string().contains('\u{1b}'),
            "escape survived: {err}"
        );
    }

    /// A failure that is not a curl failure has no hop to describe, so
    /// `with_hint` must leave it alone rather than reshaping it.
    #[test]
    fn with_hint_passes_other_errors_through() {
        let err = with_hint(CurlError::Missing, "a hint");
        assert!(matches!(err, CurlError::Missing), "got {err:?}");
    }

    /// curl applies no timeout of its own, so the argv this binary ships has to
    /// carry the bound - the same invariant `production_agent_has_a_bounded_timeout`
    /// asserts for the ureq side.
    #[test]
    fn curl_argv_carries_a_stall_bound() {
        let argv = curl_argv("https://example.invalid/x", None);
        let at = |flag: &str| {
            argv.iter()
                .position(|a| a == flag)
                .map(|i| argv[i + 1].clone())
                .unwrap_or_else(|| panic!("{flag} missing from {argv:?}"))
        };
        assert_eq!(at("--connect-timeout"), CONNECT_TIMEOUT_SECS.to_string());
        assert_eq!(at("--speed-limit"), "1");
        assert_eq!(at("--speed-time"), STALL_TIMEOUT_SECS.to_string());

        // Read the bound back off the argv rather than off the constant, so a
        // value that curl would treat as "no bound" cannot ship unnoticed.
        let stall: u64 = at("--speed-time")
            .parse()
            .expect("speed-time must be a number");
        assert!(
            stall > 0 && stall <= 120,
            "{stall}s is not a useful stall bound"
        );
    }

    /// A release tarball has no knowable ceiling, so the uncapped fetch must
    /// not grow one - a cap here would turn some future larger release into a
    /// failed update.
    #[test]
    fn the_uncapped_fetch_sets_no_size_limit() {
        let argv = curl_argv("https://example.invalid/x", None);
        assert!(
            !argv.contains(&"--max-filesize".to_string()),
            "argv was {argv:?}"
        );
    }

    #[test]
    fn a_capped_fetch_carries_the_limit() {
        let argv = curl_argv("https://example.invalid/x", Some(4096));
        let at = argv
            .iter()
            .position(|a| a == "--max-filesize")
            .unwrap_or_else(|| panic!("--max-filesize missing from {argv:?}"));
        assert_eq!(argv[at + 1], "4096");
        assert_eq!(argv.last().unwrap(), "https://example.invalid/x");
    }

    /// The cap is added to the same argv every fetch gets, not to a fresh one.
    /// Building the capped variant separately would drop the stall bound, and
    /// the black-hole host that hung `tekops update` forever would hang the
    /// gist fetch instead - the same bug at a new call site.
    #[test]
    fn a_capped_fetch_keeps_the_stall_bound_and_the_protocol_pin() {
        let argv = curl_argv("https://example.invalid/x", Some(4096));
        for flag in [
            "-fsSL",
            "--proto",
            "--connect-timeout",
            "--speed-limit",
            "--speed-time",
        ] {
            assert!(
                argv.contains(&flag.to_string()),
                "{flag} missing from {argv:?}"
            );
        }
    }

    /// Pins the curl behaviour the cap relies on: an oversized body is refused
    /// rather than truncated or silently accepted. It drives the real binary
    /// over plain http with an injected cap, for the same reason
    /// `a_wedged_endpoint_gives_up_instead_of_hanging` does - the shipped
    /// `--proto =https` would reject an http test URL before the cap was ever
    /// reached, and the assertion would then pass without testing anything.
    #[test]
    fn curl_refuses_a_body_past_the_cap() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            while let Ok((mut conn, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = conn.read(&mut scratch);
                let body = "x".repeat(10_000);
                let _ = conn.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });

        let url = format!("http://127.0.0.1:{port}/x");
        let run = |cap: &str| {
            Command::new(CURL)
                .args(["-fsS", "--proto", "=http", "--max-filesize", cap])
                .arg(&url)
                .output()
                .expect("curl should be on PATH for this suite")
        };

        let under = run("20000");
        assert!(
            under.status.success() && under.stdout.len() == 10_000,
            "a body under the cap must arrive whole"
        );
        assert!(
            !run("100").status.success(),
            "a body past the cap must not look like a success"
        );
    }

    /// The real repro: a host that accepts the connection and then never
    /// answers. Without the stall bound curl waits forever and `-s` hides even
    /// the progress meter, so the command looks frozen.
    ///
    /// It runs over plain http with a short injected stall, mirroring
    /// `agent_with_timeout` in `http.rs`. The shipped `--proto =https` would
    /// make curl reject an http test URL instantly and the deadline below would
    /// then pass without ever exercising the timeout.
    #[test]
    fn a_wedged_endpoint_gives_up_instead_of_hanging() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            while let Ok((conn, _)) = listener.accept() {
                held.push(conn);
            }
        });

        let started = std::time::Instant::now();
        let status = Command::new(CURL)
            .args(["-fsS", "--proto", "=http"])
            .args(stall_argv(1))
            .arg(format!("http://127.0.0.1:{port}/x"))
            .output()
            .expect("curl should be on PATH for this suite")
            .status;
        let elapsed = started.elapsed();

        assert!(
            !status.success(),
            "a silent endpoint must not look like a success"
        );
        assert!(
            elapsed.as_secs() < 10,
            "curl hung on a silent endpoint for {elapsed:?}"
        );
    }
}
