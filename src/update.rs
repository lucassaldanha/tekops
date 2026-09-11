//! Self-update: replaces the running tekops binary with a build published to
//! GitHub Releases.
//!
//! All HTTPS goes through `curl` rather than through `ureq`. tekops drops
//! ureq's TLS backend on purpose (see the dependency comment in Cargo.toml),
//! and GitHub is HTTPS-only, so the transport has to come from somewhere that
//! is not this binary. `curl` is documented as a runtime dependency alongside
//! `tail`, `less`, and `tar`.
#![allow(dead_code)]
// Removed in the final task of the self-update work, once cli.rs consumes every item here.

use crate::term::sanitize;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::Command;

const REPO: &str = "lucassaldanha/tekops";

/// The release target triple for this build, or `None` on a host tekops does
/// not publish for. Resolved at compile time so an unsupported host fails
/// before any network call rather than 404-ing on a guessed asset name.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub const TARGET: Option<&'static str> = Some("x86_64-unknown-linux-musl");
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
pub const TARGET: Option<&'static str> = Some("aarch64-unknown-linux-musl");
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
pub const TARGET: Option<&'static str> = Some("aarch64-apple-darwin");
#[cfg(not(any(
    all(target_arch = "x86_64", target_os = "linux"),
    all(target_arch = "aarch64", target_os = "linux"),
    all(target_arch = "aarch64", target_os = "macos"),
)))]
pub const TARGET: Option<&'static str> = None;

/// What the single positional of `tekops update` asked for.
#[derive(Debug, PartialEq)]
pub enum UpdateTarget {
    /// No argument: check, then ask before installing.
    Prompt,
    Check,
    Latest,
    Version(String),
}

/// Splits the one positional of `tekops update`.
///
/// `check` and `latest` are matched by hand rather than declared as clap
/// subcommands. Declaring them as subcommands alongside an optional positional
/// version reproduces exactly the ambiguity that made `tekops logs
/// /var/log/x.log` fail with `invalid value for [SOURCE]` - see
/// `resolve_logs_target`, which this mirrors, down to the case-sensitivity.
pub fn resolve_update_target(arg: Option<String>) -> UpdateTarget {
    match arg.as_deref() {
        None => UpdateTarget::Prompt,
        Some("check") => UpdateTarget::Check,
        Some("latest") => UpdateTarget::Latest,
        Some(version) => UpdateTarget::Version(normalize_version(version)),
    }
}

/// Tags carry a `v` prefix; every internal comparison and format string
/// assumes it is absent.
pub fn normalize_version(version: &str) -> String {
    version.strip_prefix('v').unwrap_or(version).to_string()
}

fn latest_release_url() -> String {
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

/// Reconstructs the name `scripts/build-release.sh` gives a tarball, rather
/// than parsing the release's asset list: the naming is ours and fixed, so an
/// extra API round trip would buy nothing.
fn asset_name(version: &str, target: &str) -> String {
    format!("tekops-v{version}-{target}.tar.gz")
}

fn download_url(version: &str, file: &str) -> String {
    format!("https://github.com/{REPO}/releases/download/v{version}/{file}")
}

const CURL: &str = "curl";

#[derive(Debug)]
pub enum UpdateError {
    UnsupportedTarget,
    CurlMissing,
    TarMissing,
    CurlFailed { url: String, status: String, stderr: String },
    Malformed(String),
    ChecksumMissing { asset: String },
    ChecksumMismatch { expected: String, actual: String },
    ExtractFailed(String),
    SmokeTestFailed(String),
    NotWritable { dir: PathBuf, source: String },
    Io(String),
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateError::UnsupportedTarget => write!(
                f,
                "no tekops release is published for this platform ({} {})",
                std::env::consts::ARCH,
                std::env::consts::OS
            ),
            UpdateError::CurlMissing => {
                write!(f, "curl is required for updates but was not found on PATH")
            }
            UpdateError::TarMissing => {
                write!(f, "tar is required for updates but was not found on PATH")
            }
            UpdateError::CurlFailed { url, status, stderr } => write!(
                f,
                "could not download {url} ({status}): {stderr}\n\
                 if the version is real, there may be no asset published for this platform"
            ),
            UpdateError::Malformed(msg) => write!(f, "GitHub returned malformed data: {msg}"),
            UpdateError::ChecksumMissing { asset } => {
                write!(f, "SHA256SUMS does not list {asset}; refusing to install an unlisted asset")
            }
            UpdateError::ChecksumMismatch { expected, actual } => write!(
                f,
                "checksum mismatch: expected {expected}, got {actual}; the download was not installed"
            ),
            UpdateError::ExtractFailed(msg) => write!(f, "could not unpack the release: {msg}"),
            UpdateError::SmokeTestFailed(msg) => {
                write!(f, "the downloaded binary failed its check, nothing was installed: {msg}")
            }
            UpdateError::NotWritable { dir, source } => write!(
                f,
                "cannot write to {}: {source}\ntry re-running under sudo",
                dir.display()
            ),
            UpdateError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

fn curl_argv(url: &str) -> Vec<String> {
    vec![
        "-fsSL".to_string(),
        "--proto".to_string(),
        "=https".to_string(),
        url.to_string(),
    ]
}

/// The single HTTPS boundary. Every network read in this module goes through
/// here, so the transport policy lives in exactly one place - the same reason
/// `http::agent()` exists for the ureq clients.
pub(crate) fn fetch(url: &str) -> Result<Vec<u8>, UpdateError> {
    fetch_with(CURL, url)
}

fn fetch_with(program: &str, url: &str) -> Result<Vec<u8>, UpdateError> {
    let output = Command::new(program)
        .args(curl_argv(url))
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => UpdateError::CurlMissing,
            _ => UpdateError::Io(e.to_string()),
        })?;

    if !output.status.success() {
        return Err(UpdateError::CurlFailed {
            url: url.to_string(),
            status: output.status.to_string(),
            stderr: sanitize(String::from_utf8_lossy(&output.stderr).trim()),
        });
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_argument_means_prompt() {
        assert_eq!(resolve_update_target(None), UpdateTarget::Prompt);
    }

    #[test]
    fn check_and_latest_are_reserved_words() {
        assert_eq!(resolve_update_target(Some("check".into())), UpdateTarget::Check);
        assert_eq!(resolve_update_target(Some("latest".into())), UpdateTarget::Latest);
    }

    #[test]
    fn anything_else_is_a_version() {
        assert_eq!(
            resolve_update_target(Some("0.3.0".into())),
            UpdateTarget::Version("0.3.0".into())
        );
    }

    #[test]
    fn a_leading_v_is_normalized_away() {
        assert_eq!(
            resolve_update_target(Some("v0.3.0".into())),
            UpdateTarget::Version("0.3.0".into())
        );
        assert_eq!(normalize_version("v1.2.3"), "1.2.3");
        assert_eq!(normalize_version("1.2.3"), "1.2.3");
    }

    /// Reserved-word matching is case-sensitive, mirroring `resolve_logs_target`.
    #[test]
    fn reserved_words_are_case_sensitive() {
        assert_eq!(
            resolve_update_target(Some("Check".into())),
            UpdateTarget::Version("Check".into())
        );
    }

    /// Nonsense arrives as a version and fails later against a real release,
    /// rather than being second-guessed here. There is one positional and no
    /// path handling in this command, so there is nothing to disambiguate.
    #[test]
    fn a_path_shaped_argument_is_treated_as_a_version() {
        assert_eq!(
            resolve_update_target(Some("/var/log/x.log".into())),
            UpdateTarget::Version("/var/log/x.log".into())
        );
    }

    #[test]
    fn asset_name_matches_what_build_release_produces() {
        assert_eq!(
            asset_name("0.3.1", "aarch64-apple-darwin"),
            "tekops-v0.3.1-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn urls_point_at_the_right_release() {
        assert_eq!(
            download_url("0.3.1", "SHA256SUMS"),
            "https://github.com/lucassaldanha/tekops/releases/download/v0.3.1/SHA256SUMS"
        );
        assert_eq!(
            latest_release_url(),
            "https://api.github.com/repos/lucassaldanha/tekops/releases/latest"
        );
    }

    /// This build must be one of the three published targets, or `tekops
    /// update` could only ever 404. A non-release host is a compile-time fact,
    /// so this asserts the constant resolved at all on whatever runs the suite.
    #[test]
    fn target_is_known_on_supported_hosts() {
        if cfg!(any(
            all(target_arch = "x86_64", target_os = "linux"),
            all(target_arch = "aarch64", target_os = "linux"),
            all(target_arch = "aarch64", target_os = "macos"),
        )) {
            assert!(TARGET.is_some());
        }
    }

    /// `-f` turns an HTTP error status into a nonzero exit instead of a body
    /// of HTML; `-L` is required because GitHub redirects release downloads to
    /// objects.githubusercontent.com; `--proto =https` pins every hop of that
    /// redirect chain to https so a redirect cannot downgrade the transport.
    #[test]
    fn curl_argv_pins_the_protocol_and_follows_redirects() {
        let argv = curl_argv("https://example.invalid/x");
        assert!(argv.contains(&"-fsSL".to_string()), "argv was {argv:?}");
        let proto = argv.iter().position(|a| a == "--proto").expect("no --proto");
        assert_eq!(argv[proto + 1], "=https");
        assert_eq!(argv.last().unwrap(), "https://example.invalid/x");
    }

    #[test]
    fn a_missing_curl_is_reported_as_such() {
        let err = fetch_with("tekops-no-such-program-exists", "https://example.invalid/x")
            .expect_err("a missing program must not succeed");
        assert!(matches!(err, UpdateError::CurlMissing), "got {err:?}");
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
        assert!(matches!(err, UpdateError::CurlFailed { .. }), "got {err:?}");
        assert!(err.to_string().contains("https://example.invalid/x"));
    }

    /// curl's stderr is untrusted text on its way to the operator's terminal,
    /// the same class of input `map_ureq_error` sanitizes.
    #[test]
    fn curl_stderr_is_sanitized_into_the_error() {
        let err = UpdateError::CurlFailed {
            url: "https://example.invalid/x".into(),
            status: "exit status: 22".into(),
            stderr: crate::term::sanitize("boom\u{1b}[2Jgone"),
        };
        assert!(!err.to_string().contains('\u{1b}'), "escape survived: {err}");
    }
}
