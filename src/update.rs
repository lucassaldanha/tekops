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
}
