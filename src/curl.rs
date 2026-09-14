//! The HTTPS boundary, shelled out to `curl`.
//!
//! tekops drops ureq's TLS backend on purpose (see the dependency comment in
//! Cargo.toml), so anything that has to speak HTTPS - GitHub Releases, for
//! `update` - cannot use the ureq clients and needs a transport from outside
//! this binary. `curl` is documented as a runtime dependency alongside `tail`,
//! `less`, and `tar`.
//!
//! This module exists for the same reason `http.rs` does: transport policy
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

fn curl_argv(url: &str) -> Vec<String> {
    let mut argv = vec![
        "-fsSL".to_string(),
        "--proto".to_string(),
        "=https".to_string(),
    ];
    argv.extend(stall_argv(STALL_TIMEOUT_SECS));
    argv.push(url.to_string());
    argv
}

/// The single HTTPS boundary. Every network read outside the ureq clients goes
/// through here, so the transport policy lives in exactly one place - the same
/// reason `http::agent()` exists for those clients.
pub fn fetch(url: &str) -> Result<Vec<u8>, CurlError> {
    fetch_with(CURL, url)
}

fn fetch_with(program: &str, url: &str) -> Result<Vec<u8>, CurlError> {
    let output = Command::new(program)
        .args(curl_argv(url))
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
    /// objects.githubusercontent.com; `--proto =https` pins every hop of that
    /// redirect chain to https so a redirect cannot downgrade the transport.
    #[test]
    fn curl_argv_pins_the_protocol_and_follows_redirects() {
        let argv = curl_argv("https://example.invalid/x");
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
        let err = fetch_with("tekops-no-such-program-exists", "https://example.invalid/x")
            .expect_err("a missing program must not succeed");
        assert!(matches!(err, CurlError::Missing), "got {err:?}");
        assert!(err.to_string().contains("curl"));
    }

    /// Stands in for curl to prove the plumbing: the child is spawned with our
    /// argv and its stdout is what comes back.
    #[test]
    fn a_successful_program_returns_its_stdout() {
        let out = fetch_with("echo", "https://example.invalid/x").expect("echo should succeed");
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("https://example.invalid/x"), "got {text:?}");
    }

    #[test]
    fn a_nonzero_exit_is_reported_with_the_url() {
        let err = fetch_with("false", "https://example.invalid/x")
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
        let argv = curl_argv("https://example.invalid/x");
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
