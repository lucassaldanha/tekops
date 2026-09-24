//! Self-update: replaces the running tekops binary with a build published to
//! GitHub Releases.
//!
//! All HTTPS goes through `curl` rather than through `ureq`, for the reasons
//! `curl.rs` documents; this module owns only what is specific to fetching a
//! release, not the transport itself.

use crate::curl::{self, CurlError};
use crate::term::sanitize;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

const REPO: &str = "lucassaldanha/tekops";

/// The release asset suffix for this build, or `None` on a host tekops does
/// not publish for. Resolved at compile time so an unsupported host fails
/// before any network call rather than 404-ing on a guessed asset name.
///
/// These are `<arch>-<os>`, not cargo target triples: the triple's vendor
/// field ("unknown") says nothing, and "musl" is implied because every Linux
/// build is static. The strings must match the `asset_target` values in
/// `scripts/build-release.sh`, which is the only other place they appear -
/// disagreement between the two makes `tekops update` 404 while the test
/// suite stays green, since the tests compare this constant against itself.
///
/// Note these differ from the names used by releases up to v0.3.1, so
/// `tekops update <version>` cannot reach those. That was the cheapest moment
/// to change it: no released binary had `tekops update` at all.
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub const TARGET: Option<&'static str> = Some("x86_64-linux");
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
pub const TARGET: Option<&'static str> = Some("aarch64-linux");
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
pub const TARGET: Option<&'static str> = Some("aarch64-macos");
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
/// version would create the same "one slot, two meanings" problem that used
/// to break `tekops logs`: its first positional was typed as an enum of
/// source names, so clap rejected any value outside that set before the
/// command ever ran - `tekops logs /var/log/x.log` failed with
/// `invalid value for [SOURCE]`. This mirrors the hand-matching that fixed
/// that, down to the case-sensitivity.
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

#[derive(Debug)]
pub enum UpdateError {
    UnsupportedTarget,
    TarMissing,
    Curl(CurlError),
    Malformed(String),
    ChecksumMissing { asset: String },
    ChecksumMismatch { expected: String, actual: String },
    ExtractFailed(String),
    SmokeTestFailed(String),
    NotWritable { dir: PathBuf, source: String },
    HomebrewManaged,
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
            UpdateError::TarMissing => {
                write!(f, "tar is required for updates but was not found on PATH")
            }
            UpdateError::Curl(e) => write!(f, "{e}"),
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
            UpdateError::HomebrewManaged => write!(
                f,
                "this tekops was installed by Homebrew; run `brew upgrade tekops` instead"
            ),
            UpdateError::Io(msg) => write!(f, "{msg}"),
        }
    }
}

impl From<CurlError> for UpdateError {
    fn from(e: CurlError) -> Self {
        UpdateError::Curl(e)
    }
}

const ASSET_HINT: &str =
    "if the version is real, there may be no asset published for this platform";
const RELEASE_HINT: &str = "the repository may be private, or have no published releases";

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Finds the hash `SHA256SUMS` lists for exactly `asset`.
///
/// Matching is exact rather than by prefix: a prefix match would let a
/// neighbouring asset whose name merely starts with ours satisfy the lookup.
fn parse_sha256sums(text: &str, asset: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (hash, name) = line.split_once(char::is_whitespace)?;
        let name = name.trim_start().trim_start_matches('*');
        (name == asset && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| hash.to_string())
    })
}

/// Note what this does and does not prove: `SHA256SUMS` is fetched from the
/// same release as the tarball, so it catches corruption, truncation, and a
/// wrong-target asset, but not a compromised repository - whoever can replace
/// the tarball can replace the checksum beside it. The channel guarantee is
/// TLS, which is what `--proto =https` in `curl_argv` protects. This is not a
/// signature, and should not be described as one.
fn verify_checksum(tarball: &[u8], sums: &[u8], asset: &str) -> Result<(), UpdateError> {
    let text = String::from_utf8_lossy(sums);
    let expected = parse_sha256sums(&text, asset).ok_or_else(|| UpdateError::ChecksumMissing {
        asset: asset.to_string(),
    })?;
    let actual = sha256_hex(tarball);
    if actual != expected {
        return Err(UpdateError::ChecksumMismatch { expected, actual });
    }
    Ok(())
}

/// Unpacks a release tarball into `dir` and returns the path to the binary.
///
/// `tar` is shelled out to rather than pulled in as a crate, consistent with
/// how this binary already depends on `tail`, `less`, and `curl`, and avoiding
/// a tar plus flate2 stack for one call.
fn extract_binary(tarball: &[u8], dir: &Path) -> Result<PathBuf, UpdateError> {
    let archive = dir.join("tekops.tar.gz");
    fs::write(&archive, tarball).map_err(|e| UpdateError::Io(e.to_string()))?;

    let output = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(dir)
        .output()
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => UpdateError::TarMissing,
            _ => UpdateError::Io(e.to_string()),
        })?;

    if !output.status.success() {
        return Err(UpdateError::ExtractFailed(sanitize(
            String::from_utf8_lossy(&output.stderr).trim(),
        )));
    }

    let binary = dir.join("tekops");
    if !binary.is_file() {
        return Err(UpdateError::ExtractFailed(
            "the archive did not contain a tekops binary".to_string(),
        ));
    }
    Ok(binary)
}

/// Runs the downloaded binary before it replaces the working one, so a corrupt
/// or wrong-architecture build is caught while the old binary is still in
/// place. This is not a security control - a hostile asset executes either
/// way; it is a guard against installing something that cannot run.
fn smoke_test(binary: &Path, expected_version: &str) -> Result<(), UpdateError> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| UpdateError::SmokeTestFailed(e.to_string()))?;

    if !output.status.success() {
        return Err(UpdateError::SmokeTestFailed(format!(
            "`tekops --version` exited with {}",
            output.status
        )));
    }

    let reported = sanitize(String::from_utf8_lossy(&output.stdout).trim());
    let expected = format!("tekops {expected_version}");
    if reported != expected {
        return Err(UpdateError::SmokeTestFailed(format!(
            "it reports `{reported}`, expected `{expected}`"
        )));
    }
    Ok(())
}

/// Confirms the directory holding the binary can be written, before anything
/// is downloaded, so a non-root invocation fails in a second rather than after
/// fetching a megabyte.
///
/// It writes a file rather than inspecting permission bits: a path can be
/// unwritable for reasons the bits do not show (read-only mount, immutable
/// flag, SELinux).
fn probe_writable(dir: &Path) -> Result<(), UpdateError> {
    let probe = dir.join(format!(".tekops-probe-{}", process::id()));
    fs::write(&probe, b"").map_err(|e| UpdateError::NotWritable {
        dir: dir.to_path_buf(),
        source: e.to_string(),
    })?;
    let _ = fs::remove_file(&probe);
    Ok(())
}

/// Puts the verified binary in place.
///
/// The copy lands beside `dest` first so the final step is a rename within one
/// filesystem, which is atomic: either the old binary or the new one is there,
/// never a half-written file. Renaming over a running executable is safe on
/// Linux and macOS - the inode stays alive for the running process. If the
/// rename fails, the staged file is removed so a failed update leaves nothing
/// behind in a directory like /usr/local/bin.
fn install_binary(src: &Path, dest: &Path) -> Result<(), UpdateError> {
    let dir = dest
        .parent()
        .ok_or_else(|| UpdateError::Io(format!("{} has no parent directory", dest.display())))?;
    let staged = dir.join(format!(".tekops-update-{}", process::id()));

    let not_writable = |e: io::Error| UpdateError::NotWritable {
        dir: dir.to_path_buf(),
        source: e.to_string(),
    };

    // One cleanup path covering all three steps rather than one per step: the
    // copy itself can fail after creating the file (ENOSPC, a read error on
    // the source), and cleaning up only in the steps after it left a partial
    // .tekops-update-<pid> behind in a directory like /usr/local/bin.
    let staging = (|| {
        fs::copy(src, &staged)?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
        fs::rename(&staged, dest)
    })();
    if let Err(e) = staging {
        let _ = fs::remove_file(&staged);
        return Err(not_writable(e));
    }
    Ok(())
}

/// Whether `exe`, already canonicalized, is a binary Homebrew installed.
///
/// Replacing one in place would leave Homebrew recording a version that is no
/// longer there, and replacing the `bin/` symlink instead would put an
/// unmanaged file in Homebrew's prefix. Either way the next `brew upgrade`
/// is working against a lie. Every Homebrew prefix keeps kegs at
/// `<prefix>/Cellar/<formula>/<version>/`, so that pair is the tell.
pub fn homebrew_managed(exe: &Path) -> bool {
    let parts: Vec<_> = exe.components().map(|c| c.as_os_str()).collect();
    parts
        .windows(2)
        .any(|w| w[0] == "Cellar" && w[1] == "tekops")
}

/// The tekops build that is running, which is a different question from
/// `tekops version` (the running Teku's version, read from the metrics
/// endpoint).
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub current: String,
    pub latest: String,
    pub update_available: bool,
}

#[derive(Deserialize)]
struct ReleaseJson {
    tag_name: String,
}

fn tag_from_release_json(body: &[u8]) -> Result<String, UpdateError> {
    let release: ReleaseJson = serde_json::from_slice(body)
        .map_err(|e| UpdateError::Malformed(format!("release metadata: {e}")))?;
    Ok(normalize_version(&release.tag_name))
}

/// Asks GitHub what the newest release is and compares it to this build.
///
/// "Available" means "different from what is running", not "greater than":
/// tekops has no version-ordering logic and does not need any, since the only
/// way to end up below the latest release is to be behind it.
pub fn check<F>(fetch: F) -> Result<CheckResult, UpdateError>
where
    F: Fn(&str) -> Result<Vec<u8>, CurlError>,
{
    let body = fetch(&latest_release_url()).map_err(|e| curl::with_hint(e, RELEASE_HINT))?;
    let latest = tag_from_release_json(&body)?;
    let current = current_version().to_string();
    Ok(CheckResult {
        update_available: latest != current,
        current,
        latest,
    })
}

/// Downloads, verifies, smoke-tests, and installs one specific version.
///
/// Ordering is load-bearing: the writability probe runs before any download so
/// a non-root invocation fails immediately, and every check happens in a
/// TempDir so `dest` is untouched until the single rename at the end. Every
/// early return above that rename leaves a working tekops in place.
pub fn install<F>(fetch: F, version: &str, dest: &Path) -> Result<(), UpdateError>
where
    F: Fn(&str) -> Result<Vec<u8>, CurlError>,
{
    let target = TARGET.ok_or(UpdateError::UnsupportedTarget)?;
    let dir = dest
        .parent()
        .ok_or_else(|| UpdateError::Io(format!("{} has no parent directory", dest.display())))?;
    probe_writable(dir)?;

    let asset = asset_name(version, target);
    let tarball =
        fetch(&download_url(version, &asset)).map_err(|e| curl::with_hint(e, ASSET_HINT))?;
    // No hint on SHA256SUMS: the URL in the message already says what is
    // missing, and a release that has the tarball but not its checksums is not
    // a platform problem.
    let sums = fetch(&download_url(version, "SHA256SUMS"))?;
    verify_checksum(&tarball, &sums, &asset)?;

    let staging = tempfile::tempdir().map_err(|e| UpdateError::Io(e.to_string()))?;
    let binary = extract_binary(&tarball, staging.path())?;
    smoke_test(&binary, version)?;
    install_binary(&binary, dest)
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
        assert_eq!(
            resolve_update_target(Some("check".into())),
            UpdateTarget::Check
        );
        assert_eq!(
            resolve_update_target(Some("latest".into())),
            UpdateTarget::Latest
        );
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

    /// Reserved-word matching is case-sensitive, mirroring how `tekops logs`
    /// used to hand-match its own source argument before that concept was
    /// removed.
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
            asset_name("0.4.0", "aarch64-macos"),
            "tekops-v0.4.0-aarch64-macos.tar.gz"
        );
    }

    /// Guards the one thing no other test can: that this constant still spells
    /// the asset name `scripts/build-release.sh` actually publishes. The two
    /// live in different languages and nothing but this test connects them, so
    /// a rename on either side that forgets the other lands here rather than in
    /// a 404 during a real update.
    #[test]
    fn target_matches_the_names_build_release_publishes() {
        let script = include_str!("../scripts/build-release.sh");
        let target = TARGET.expect("test host must be a release target");
        assert!(
            script.contains(&format!("asset_target=\"{target}\"")),
            "build-release.sh publishes no asset named {target}"
        );
    }

    #[test]
    fn a_binary_in_the_homebrew_cellar_is_homebrew_managed() {
        for path in [
            "/opt/homebrew/Cellar/tekops/0.11.0/bin/tekops",
            "/usr/local/Cellar/tekops/0.11.0/bin/tekops",
            "/home/linuxbrew/.linuxbrew/Cellar/tekops/0.11.0/bin/tekops",
        ] {
            assert!(homebrew_managed(Path::new(path)), "{path}");
        }
    }

    /// Only the resolved Cellar path counts. The `bin/` symlink is what a
    /// caller gets from `current_exe` before canonicalizing, and a hand-copied
    /// binary in the same prefix is not Homebrew's to manage.
    #[test]
    fn anything_outside_the_cellar_is_not_homebrew_managed() {
        for path in [
            "/usr/local/bin/tekops",
            "/opt/homebrew/bin/tekops",
            "/home/me/Cellar/other/bin/tekops",
            "/home/me/tekops/Cellar/bin/tekops",
        ] {
            assert!(!homebrew_managed(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn the_homebrew_refusal_says_what_to_run_instead() {
        let msg = UpdateError::HomebrewManaged.to_string();
        assert!(msg.contains("brew upgrade tekops"), "{msg}");
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

    /// The platform hint belongs to the asset download only. Attached to the
    /// release-metadata call it points the operator at the wrong cause, which
    /// is what a private repo produced in practice.
    #[test]
    fn each_hop_carries_the_hint_that_fits_it() {
        let raw = || CurlError::Failed {
            url: "https://example.invalid/x".into(),
            status: "exit status: 22".into(),
            stderr: "404".into(),
            hint: None,
        };
        let bare = raw().to_string();
        assert!(
            !bare.contains("this platform"),
            "unhinted failure claimed a cause: {bare}"
        );

        let asset = curl::with_hint(raw(), ASSET_HINT).to_string();
        assert!(
            asset.contains("no asset published for this platform"),
            "got {asset}"
        );
        assert!(!asset.contains("private"), "got {asset}");

        let release = curl::with_hint(raw(), RELEASE_HINT).to_string();
        assert!(release.contains("private"), "got {release}");
        assert!(!release.contains("this platform"), "got {release}");
    }

    const SUMS: &str = concat!(
        "1111111111111111111111111111111111111111111111111111111111111111  tekops-v0.4.0-aarch64-macos.tar.gz\n",
        "2222222222222222222222222222222222222222222222222222222222222222  tekops-v0.4.0-x86_64-linux.tar.gz\n",
    );

    #[test]
    fn sha256_of_the_empty_input_is_the_known_vector() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn the_matching_line_is_found() {
        assert_eq!(
            parse_sha256sums(SUMS, "tekops-v0.4.0-x86_64-linux.tar.gz").as_deref(),
            Some("2222222222222222222222222222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn an_absent_asset_is_none() {
        assert_eq!(parse_sha256sums(SUMS, "tekops-v0.9.9-riscv.tar.gz"), None);
    }

    /// A blank or truncated line must be skipped, not crash and not be
    /// mistaken for a hash.
    #[test]
    fn malformed_lines_are_skipped() {
        let text = "\n\nnot-a-hash  tekops.tar.gz\nsomething\n";
        assert_eq!(parse_sha256sums(text, "tekops.tar.gz"), None);
    }

    /// Matching must be exact. A prefix match would let
    /// `tekops-v0.4.0-aarch64-macos.tar.gz.sig` satisfy a request for the
    /// tarball.
    #[test]
    fn a_filename_that_merely_starts_with_ours_does_not_match() {
        let text =
            "3333333333333333333333333333333333333333333333333333333333333333  tekops.tar.gz.sig\n";
        assert_eq!(parse_sha256sums(text, "tekops.tar.gz"), None);
    }

    /// sha256sum writes binary-mode entries as "<hash> *<name>".
    #[test]
    fn a_binary_mode_star_is_tolerated() {
        let text =
            "4444444444444444444444444444444444444444444444444444444444444444 *tekops.tar.gz\n";
        assert_eq!(
            parse_sha256sums(text, "tekops.tar.gz").as_deref(),
            Some("4444444444444444444444444444444444444444444444444444444444444444")
        );
    }

    #[test]
    fn a_matching_checksum_verifies() {
        let body = b"the tarball bytes";
        let sums = format!("{}  tekops.tar.gz\n", sha256_hex(body));
        assert!(verify_checksum(body, sums.as_bytes(), "tekops.tar.gz").is_ok());
    }

    #[test]
    fn a_mismatched_checksum_is_rejected_with_both_hashes() {
        let sums = format!("{}  tekops.tar.gz\n", "5".repeat(64));
        let err = verify_checksum(b"the tarball bytes", sums.as_bytes(), "tekops.tar.gz")
            .expect_err("a mismatch must not verify");
        assert!(
            matches!(err, UpdateError::ChecksumMismatch { .. }),
            "got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains(&"5".repeat(64)),
            "expected hash missing from {msg}"
        );
        assert!(
            msg.contains(&sha256_hex(b"the tarball bytes")),
            "actual hash missing from {msg}"
        );
    }

    #[test]
    fn an_unlisted_asset_is_rejected() {
        let err = verify_checksum(b"x", SUMS.as_bytes(), "tekops-v9.9.9-nope.tar.gz")
            .expect_err("an unlisted asset must not verify");
        assert!(
            matches!(err, UpdateError::ChecksumMissing { .. }),
            "got {err:?}"
        );
    }

    /// Builds a tarball shaped like a real release asset: `tekops`,
    /// `README.md`, and `LICENSE` flat at the archive root.
    fn release_tarball(script: &str) -> Vec<u8> {
        let stage = tempfile::tempdir().unwrap();
        let binary = stage.path().join("tekops");
        std::fs::write(&binary, script).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(stage.path().join("README.md"), "readme").unwrap();
        std::fs::write(stage.path().join("LICENSE"), "license").unwrap();

        let out = stage.path().join("out.tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&out)
            .arg("-C")
            .arg(stage.path())
            .args(["tekops", "README.md", "LICENSE"])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(&out).unwrap()
    }

    fn version_script(output: &str) -> String {
        format!("#!/bin/sh\necho '{output}'\n")
    }

    #[test]
    fn the_binary_is_extracted_from_a_release_shaped_tarball() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = release_tarball(&version_script("tekops 0.3.0"));
        let binary = extract_binary(&tarball, dir.path()).expect("extraction should succeed");
        assert_eq!(binary.file_name().unwrap(), "tekops");
        assert!(binary.is_file());
    }

    #[test]
    fn an_archive_without_a_tekops_binary_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        std::fs::write(stage.path().join("README.md"), "readme").unwrap();
        let out = stage.path().join("out.tar.gz");
        std::process::Command::new("tar")
            .arg("-czf")
            .arg(&out)
            .arg("-C")
            .arg(stage.path())
            .arg("README.md")
            .status()
            .unwrap();
        let tarball = std::fs::read(&out).unwrap();

        let err = extract_binary(&tarball, dir.path()).expect_err("must reject");
        assert!(matches!(err, UpdateError::ExtractFailed(_)), "got {err:?}");
    }

    #[test]
    fn garbage_that_is_not_a_tarball_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = extract_binary(b"not a tarball at all", dir.path()).expect_err("must reject");
        assert!(matches!(err, UpdateError::ExtractFailed(_)), "got {err:?}");
    }

    #[test]
    fn a_binary_reporting_the_expected_version_passes_the_smoke_test() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = release_tarball(&version_script("tekops 0.3.0"));
        let binary = extract_binary(&tarball, dir.path()).unwrap();
        assert!(smoke_test(&binary, "0.3.0").is_ok());
    }

    /// Catches a corrupt or wrong-architecture build before it replaces a
    /// working binary. Not a security control: the downloaded binary runs
    /// either way.
    #[test]
    fn a_binary_reporting_the_wrong_version_fails_the_smoke_test() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = release_tarball(&version_script("tekops 9.9.9"));
        let binary = extract_binary(&tarball, dir.path()).unwrap();
        let err = smoke_test(&binary, "0.3.0").expect_err("must reject");
        assert!(
            matches!(err, UpdateError::SmokeTestFailed(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn a_binary_that_exits_nonzero_fails_the_smoke_test() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = release_tarball("#!/bin/sh\nexit 1\n");
        let binary = extract_binary(&tarball, dir.path()).unwrap();
        let err = smoke_test(&binary, "0.3.0").expect_err("must reject");
        assert!(
            matches!(err, UpdateError::SmokeTestFailed(_)),
            "got {err:?}"
        );
    }

    /// The smoke-tested binary's stdout is untrusted and goes into an error
    /// that gets printed to the terminal.
    #[test]
    fn smoke_test_output_is_sanitized_into_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let tarball = release_tarball("#!/bin/sh\nprintf 'tekops \\033[2J9.9.9\\n'\n");
        let binary = extract_binary(&tarball, dir.path()).unwrap();
        let err = smoke_test(&binary, "0.3.0").expect_err("must reject");
        assert!(
            !err.to_string().contains('\u{1b}'),
            "escape survived: {err}"
        );
    }

    #[test]
    fn a_writable_directory_passes_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        assert!(probe_writable(dir.path()).is_ok());
    }

    /// The probe writes rather than inspecting permission bits, because a path
    /// can be unwritable for reasons the bits do not show (read-only mount,
    /// immutable flag, SELinux).
    #[test]
    fn a_read_only_directory_fails_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = probe_writable(dir.path());
        // Restore before asserting so the TempDir can always clean itself up.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.expect_err("a read-only directory must fail the probe");
        assert!(
            matches!(err, UpdateError::NotWritable { .. }),
            "got {err:?}"
        );
        assert!(err.to_string().contains("sudo"), "no sudo hint in: {err}");
    }

    #[test]
    fn the_probe_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        probe_writable(dir.path()).unwrap();
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn install_replaces_the_destination_and_leaves_it_executable() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new");
        let dest = dir.path().join("tekops");
        std::fs::write(&src, b"new binary").unwrap();
        std::fs::write(&dest, b"old binary").unwrap();

        install_binary(&src, &dest).expect("install should succeed");

        assert_eq!(std::fs::read(&dest).unwrap(), b"new binary");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "mode was {mode:o}");
    }

    #[test]
    fn install_into_a_read_only_directory_fails_without_touching_the_destination() {
        let outer = tempfile::tempdir().unwrap();
        let src = outer.path().join("new");
        std::fs::write(&src, b"new binary").unwrap();

        let target_dir = outer.path().join("bin");
        std::fs::create_dir(&target_dir).unwrap();
        let dest = target_dir.join("tekops");
        std::fs::write(&dest, b"old binary").unwrap();
        std::fs::set_permissions(&target_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = install_binary(&src, &dest);
        let contents = std::fs::read(&dest).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&target_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        std::fs::set_permissions(&target_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result.expect_err("a read-only directory must fail the install");
        assert!(
            matches!(err, UpdateError::NotWritable { .. }),
            "got {err:?}"
        );
        assert_eq!(contents, b"old binary", "the existing binary was damaged");
        assert_eq!(
            leftovers,
            ["tekops"],
            "staged debris left behind: {leftovers:?}"
        );
    }

    /// The other side of the install: the copy succeeds and the rename fails.
    /// The read-only case above never gets past the copy, so without this the
    /// cleanup that runs after a staged file actually exists is never executed.
    #[test]
    fn a_failed_rename_removes_the_staged_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("new");
        std::fs::write(&src, b"new binary").unwrap();

        // A non-empty directory where the binary should be: the copy and the
        // chmod both succeed, then the rename cannot replace it.
        let dest = dir.path().join("tekops");
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("occupied"), b"x").unwrap();

        let err = install_binary(&src, &dest).expect_err("the rename must fail");
        assert!(
            matches!(err, UpdateError::NotWritable { .. }),
            "got {err:?}"
        );

        let staged: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".tekops-update-"))
            .collect();
        assert!(staged.is_empty(), "staged file left behind: {staged:?}");
    }

    /// A fetch stand-in that serves a canned release: the API body, the
    /// tarball, and SHA256SUMS, keyed by URL suffix.
    fn canned_fetch(
        version: &str,
        tarball: Vec<u8>,
        sums: String,
    ) -> impl Fn(&str) -> Result<Vec<u8>, CurlError> {
        let version = version.to_string();
        move |url: &str| {
            if url.ends_with("/releases/latest") {
                Ok(format!(r#"{{"tag_name":"v{version}"}}"#).into_bytes())
            } else if url.ends_with("SHA256SUMS") {
                Ok(sums.clone().into_bytes())
            } else if url.ends_with(".tar.gz") {
                Ok(tarball.clone())
            } else {
                panic!("unexpected url {url}")
            }
        }
    }

    fn canned_release(version: &str) -> (Vec<u8>, String) {
        let tarball = release_tarball(&version_script(&format!("tekops {version}")));
        let asset = asset_name(version, TARGET.expect("test host must be a release target"));
        let sums = format!("{}  {asset}\n", sha256_hex(&tarball));
        (tarball, sums)
    }

    #[test]
    fn check_reports_a_newer_release_as_available() {
        let fetch = canned_fetch("99.0.0", Vec::new(), String::new());
        let result = check(fetch).expect("check should succeed");
        assert_eq!(result.latest, "99.0.0");
        assert_eq!(result.current, current_version());
        assert!(result.update_available);
    }

    #[test]
    fn check_reports_the_running_version_as_up_to_date() {
        let fetch = canned_fetch(current_version(), Vec::new(), String::new());
        let result = check(fetch).expect("check should succeed");
        assert!(!result.update_available);
    }

    #[test]
    fn check_rejects_a_malformed_release_body() {
        let err = check(|_: &str| Ok(b"not json".to_vec())).expect_err("must reject");
        assert!(matches!(err, UpdateError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn a_fetch_failure_propagates() {
        let err = check(|_: &str| Err(CurlError::Missing)).expect_err("must propagate");
        assert!(
            matches!(err, UpdateError::Curl(CurlError::Missing)),
            "got {err:?}"
        );
    }

    #[test]
    fn install_places_the_verified_binary_at_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("tekops");
        std::fs::write(&dest, b"old binary").unwrap();
        let (tarball, sums) = canned_release("0.3.0");

        install(canned_fetch("0.3.0", tarball, sums), "0.3.0", &dest)
            .expect("install should succeed");

        let installed = std::fs::read_to_string(&dest).unwrap();
        assert!(installed.contains("tekops 0.3.0"), "got {installed:?}");
    }

    #[test]
    fn install_aborts_on_a_checksum_mismatch_without_touching_the_destination() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("tekops");
        std::fs::write(&dest, b"old binary").unwrap();
        let (tarball, _) = canned_release("0.3.0");
        let asset = asset_name("0.3.0", TARGET.unwrap());
        let wrong_sums = format!("{}  {asset}\n", "6".repeat(64));

        let err = install(canned_fetch("0.3.0", tarball, wrong_sums), "0.3.0", &dest)
            .expect_err("a mismatch must abort");

        assert!(
            matches!(err, UpdateError::ChecksumMismatch { .. }),
            "got {err:?}"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"old binary");
    }

    #[test]
    fn install_aborts_when_the_asset_is_not_listed_in_sha256sums() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("tekops");
        std::fs::write(&dest, b"old binary").unwrap();
        let (tarball, _) = canned_release("0.3.0");
        let sums = "7777777777777777777777777777777777777777777777777777777777777777  something-else.tar.gz\n".to_string();

        let err = install(canned_fetch("0.3.0", tarball, sums), "0.3.0", &dest)
            .expect_err("an unlisted asset must abort");

        assert!(
            matches!(err, UpdateError::ChecksumMissing { .. }),
            "got {err:?}"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"old binary");
    }

    #[test]
    fn install_aborts_when_the_downloaded_binary_reports_the_wrong_version() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("tekops");
        std::fs::write(&dest, b"old binary").unwrap();

        // A tarball built for 9.9.9, offered as if it were 0.3.0.
        let tarball = release_tarball(&version_script("tekops 9.9.9"));
        let asset = asset_name("0.3.0", TARGET.unwrap());
        let sums = format!("{}  {asset}\n", sha256_hex(&tarball));

        let err = install(canned_fetch("0.3.0", tarball, sums), "0.3.0", &dest)
            .expect_err("a version mismatch must abort");

        assert!(
            matches!(err, UpdateError::SmokeTestFailed(_)),
            "got {err:?}"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"old binary");
    }
}
