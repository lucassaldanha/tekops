//! Guards `scripts/render-formula.sh`, which writes the Homebrew formula the
//! release workflow pushes to `lucassaldanha/homebrew-tekops`.
//!
//! Lives here rather than as a `scripts/test-*.sh` because those are not run by
//! any gate, and a formula that points at a wrong asset or checksum fails for
//! every `brew install` while the release itself looks fine.

use std::path::Path;
use std::process::{Command, Output};

const ASSETS: [&str; 3] = ["aarch64-macos", "x86_64-linux", "aarch64-linux"];

fn sha(n: u8) -> String {
    format!("{n:x}").repeat(64)[..64].to_string()
}

fn sums_for(version: &str, targets: &[&str]) -> String {
    targets
        .iter()
        .enumerate()
        .map(|(i, target)| format!("{}  tekops-v{version}-{target}.tar.gz\n", sha(i as u8 + 1)))
        .collect()
}

fn render(version: &str, sums: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SHA256SUMS");
    std::fs::write(&path, sums).unwrap();
    Command::new("bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/render-formula.sh"))
        .arg(version)
        .arg(&path)
        .output()
        .unwrap()
}

fn rendered(version: &str, sums: &str) -> String {
    let out = render(version, sums);
    assert!(
        out.status.success(),
        "render-formula.sh failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn each_asset_url_is_followed_by_its_own_checksum() {
    let formula = rendered("1.2.3", &sums_for("1.2.3", &ASSETS));
    let lines: Vec<&str> = formula.lines().map(str::trim).collect();

    for (i, target) in ASSETS.iter().enumerate() {
        let url = format!(
            "url \"https://github.com/lucassaldanha/tekops/releases/download/v1.2.3/tekops-v1.2.3-{target}.tar.gz\""
        );
        let at = lines
            .iter()
            .position(|l| *l == url)
            .unwrap_or_else(|| panic!("no url line for {target} in:\n{formula}"));
        assert_eq!(
            lines[at + 1],
            format!("sha256 \"{}\"", sha(i as u8 + 1)),
            "checksum after the {target} url is not its own"
        );
    }
}

#[test]
fn the_version_is_stated_once_without_a_leading_v() {
    let formula = rendered("v1.2.3", &sums_for("1.2.3", &ASSETS));
    assert!(formula.contains("version \"1.2.3\""), "{formula}");
    assert!(!formula.contains("vv1.2.3"), "{formula}");
}

/// A release missing one platform must not produce a formula that installs on
/// the others and 404s on that one.
#[test]
fn a_missing_asset_fails_rather_than_rendering() {
    let out = render("1.2.3", &sums_for("1.2.3", &ASSETS[..2]));
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "wrote a partial formula");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("aarch64-linux"),
        "error does not name the missing asset"
    );
}

/// Checksums for a different version sitting in the same file are not a match.
#[test]
fn checksums_for_another_version_do_not_count() {
    let out = render("1.2.3", &sums_for("1.2.4", &ASSETS));
    assert!(!out.status.success());
}

/// The formula names release assets by the `<arch>-<os>` strings
/// `scripts/build-release.sh` publishes; nothing else connects the two, and a
/// target added there without the formula would install nowhere new.
#[test]
fn the_formula_covers_exactly_the_published_targets() {
    let published: Vec<&str> = include_str!("../scripts/build-release.sh")
        .lines()
        .filter_map(|l| l.trim().strip_prefix("asset_target=\""))
        .filter_map(|l| l.strip_suffix('"'))
        .collect();
    let mut published = published;
    published.sort_unstable();

    let mut expected = ASSETS.to_vec();
    expected.sort_unstable();
    assert_eq!(published, expected, "build-release.sh targets changed");

    let formula = rendered("1.2.3", &sums_for("1.2.3", &ASSETS));
    for target in published {
        assert!(
            formula.contains(&format!("-{target}.tar.gz\"")),
            "formula has no url for {target}"
        );
    }
}
