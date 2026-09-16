//! The `~/.config/tekops/config.toml` file: what it can say, and where it is.
//!
//! Same pure/IO split as `host.rs` and `completions.rs`. `parse` and `path`
//! are pure and take every input as a parameter, so the whole surface is
//! testable with no environment races and no files on disk; `load` is the only
//! function here that touches a filesystem.
//!
//! The file carries exactly the eight settings that already have `$TEKOPS_*`
//! variables, and nothing else. Every key is a variable is a flag, which is
//! the one sentence that makes the feature explainable. Per-command defaults
//! (`lines`, `json`) are deliberately absent: they have no variable, so they
//! would break that rule, and they would need a way to tell "the operator
//! typed the default" from "the operator typed nothing", which clap's
//! `default_value_t` cannot express. A GitHub token is deliberately absent
//! too - `GITHUB_TOKEN`/`GH_TOKEN` are shared conventions with the `gh` CLI
//! rather than tekops settings, and `gist.rs` already works to keep that
//! credential out of `/proc/<pid>/cmdline` on a machine running a validator.

use crate::stack::Stack;
use crate::term::sanitize;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

const CONFIG_DIR: &str = "tekops";
const CONFIG_FILE: &str = "config.toml";

/// Everything the config file is allowed to say.
///
/// `deny_unknown_fields` is load-bearing, the same call `loglevel.rs`'s
/// `parse_spec` makes: a file containing `api_ur1` must fail loudly rather
/// than apply a config missing the one setting it was written to carry.
///
/// `stack` deserializes to `Stack`, not `String`, so a bad value is rejected
/// at parse time with the valid values named. `Stack`'s per-variant
/// `#[serde(rename)]` already matches its `#[value(name)]` exactly (see
/// `serde_matches_clap_values_for_every_variant`), so the config spelling and
/// the `--stack` spelling cannot drift apart.
///
/// `Serialize` is never used by the binary. It exists so the sample-file sync
/// test can enumerate this struct's field set and compare it against
/// `config.example.toml`.
#[derive(Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub stack: Option<Stack>,
    pub api_url: Option<String>,
    pub bn_metric_url: Option<String>,
    pub vc_metric_url: Option<String>,
    /// Deprecated alias for `bn_metric_url`.
    ///
    /// Retained rather than removed because `deny_unknown_fields` would make
    /// its removal reject every existing config file that still carries it.
    ///
    /// Note that this key changed meaning: it used to resolve to the validator
    /// client. The unprefixed spelling now means the beacon node, agreeing
    /// with `api_url`, which is likewise unprefixed and likewise the beacon
    /// node's. `doctor`'s "metrics layout" check exists to catch a file
    /// written under the old meaning.
    pub metric_url: Option<String>,
    pub container: Option<String>,
    pub logs_file: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
}

/// Why a config file could not be turned into a `Config`.
///
/// Both variants name the path, because the operator's next action is opening
/// that file and the message is the only thing telling them which one.
#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    Malformed { path: PathBuf, message: String },
    Unreadable { path: PathBuf, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Malformed { path, message } => {
                write!(f, "could not parse {}: {message}", path.display())
            }
            ConfigError::Unreadable { path, message } => {
                write!(f, "could not read {}: {message}", path.display())
            }
        }
    }
}

/// Where the config file would be, if there is anywhere for it to be.
///
/// `$XDG_CONFIG_HOME` beats `$HOME/.config`. `None` means neither variable is
/// set, which is "no config file" rather than an error: a config that cannot
/// be located must not stop a command that was never going to read it.
///
/// A non-absolute `$XDG_CONFIG_HOME` is treated as unset, which the XDG base
/// directory spec requires and which the empty string is the common way to hit:
/// scripts and systemd units routinely "unset" a variable by exporting it
/// empty. Honouring it would resolve the config to a *cwd-relative*
/// `tekops/config.toml`, so tekops would silently ignore the operator's real
/// `~/.config/tekops/config.toml` and load whatever happened to sit in the
/// directory they ran from - and on a malformed one, exit 1 before dispatch.
pub fn path(xdg_config: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    xdg_config
        .filter(|p| p.is_absolute())
        .map(Path::to_path_buf)
        .or_else(|| home.map(|h| h.join(".config")))
        .map(|base| base.join(CONFIG_DIR).join(CONFIG_FILE))
}

/// Parses config text. `path` is carried only so errors can name the file.
///
/// The error message is sanitized because serde quotes the offending key back,
/// and that key came out of a file this binary did not author - the same
/// reasoning as `LogLevelError::malformed`.
pub fn parse(text: &str, path: &Path) -> Result<Config, ConfigError> {
    toml::from_str(text).map_err(|e| ConfigError::Malformed {
        path: path.to_path_buf(),
        message: sanitize(&e.to_string()),
    })
}

/// Reads and parses the config file, if there is one.
///
/// An absent file is an empty config. Anything else that stops the read is an
/// error: a directory where a file was expected, or a file that cannot be
/// opened, is an operator mistake worth naming rather than silently treating
/// as "unconfigured".
pub fn load(path: Option<&Path>) -> Result<Config, ConfigError> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text, path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(ConfigError::Unreadable {
            path: path.to_path_buf(),
            message: sanitize(&e.to_string()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::Stack;

    #[test]
    fn a_full_config_parses_every_key() {
        let text = r#"
stack = "eth-docker"
api_url = "http://localhost:5051"
bn_metric_url = "http://localhost:8008/metrics"
vc_metric_url = "http://localhost:8009/metrics"
metric_url = "http://localhost:8008/metrics"
container = "eth-docker-consensus-1"
logs_file = "/var/log/teku/teku.log"
data_dir = "/var/lib/teku"
"#;
        let cfg = parse(text, Path::new("/x/config.toml")).unwrap();
        assert_eq!(cfg.stack, Some(Stack::EthDocker));
        assert_eq!(cfg.api_url.as_deref(), Some("http://localhost:5051"));
        assert_eq!(
            cfg.bn_metric_url.as_deref(),
            Some("http://localhost:8008/metrics")
        );
        assert_eq!(
            cfg.vc_metric_url.as_deref(),
            Some("http://localhost:8009/metrics")
        );
        assert_eq!(
            cfg.metric_url.as_deref(),
            Some("http://localhost:8008/metrics")
        );
        assert_eq!(cfg.container.as_deref(), Some("eth-docker-consensus-1"));
        assert_eq!(cfg.logs_file, Some(PathBuf::from("/var/log/teku/teku.log")));
        assert_eq!(cfg.data_dir, Some(PathBuf::from("/var/lib/teku")));
    }

    /// The deprecated key must keep parsing. `deny_unknown_fields` means
    /// dropping it from the struct would make every existing config file fail
    /// outright, so it stays a real field for as long as it is documented.
    ///
    /// Which endpoint it feeds is decided by the resolution ladder, not here;
    /// see `resolve_bn_metric_url` in `cli.rs` and the test that pins the
    /// meaning change.
    #[test]
    fn the_deprecated_metric_url_key_still_parses() {
        let cfg = parse(
            "metric_url = \"http://localhost:8008/metrics\"\n",
            Path::new("/x"),
        )
        .unwrap();
        assert_eq!(
            cfg.metric_url.as_deref(),
            Some("http://localhost:8008/metrics")
        );
        assert_eq!(cfg.bn_metric_url, None);
        assert_eq!(cfg.vc_metric_url, None);
    }

    #[test]
    fn an_empty_file_is_an_empty_config() {
        assert_eq!(parse("", Path::new("/x")).unwrap(), Config::default());
    }

    /// A comment-only file is the state a freshly copied sample is left in.
    #[test]
    fn a_comment_only_file_is_an_empty_config() {
        let cfg = parse("# nothing set\n", Path::new("/x")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    /// `deny_unknown_fields` is the whole point: a file with a typo must not
    /// apply a change missing the very thing it was written to carry.
    #[test]
    fn an_unknown_key_is_rejected_and_named() {
        let err = parse("api_ur1 = \"http://x\"\n", Path::new("/x")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("api_ur1"), "{msg}");
    }

    /// The error names the file the operator has to open, not just the error.
    #[test]
    fn a_parse_error_names_the_config_path() {
        let err = parse("stack = ", Path::new("/home/op/.config/tekops/config.toml")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/home/op/.config/tekops/config.toml"), "{msg}");
    }

    /// Deserializing to `Stack` rather than `String` is what makes a bad value
    /// fatal at parse time instead of silently dropped.
    #[test]
    fn a_bad_stack_value_is_rejected() {
        let err = parse("stack = \"eth_docker\"\n", Path::new("/x")).unwrap_err();
        assert!(err.to_string().contains("eth_docker"), "{err}");
    }

    /// The flag spelling and the config spelling must not drift apart.
    #[test]
    fn stack_accepts_exactly_the_flag_spellings() {
        for (text, want) in [
            ("bare-metal", Stack::BareMetal),
            ("eth-docker", Stack::EthDocker),
            ("rocketpool", Stack::RocketPool),
        ] {
            let cfg = parse(&format!("stack = \"{text}\"\n"), Path::new("/x")).unwrap();
            assert_eq!(cfg.stack, Some(want), "{text}");
        }
    }

    #[test]
    fn xdg_config_home_wins_over_home() {
        let got = path(Some(Path::new("/xdg")), Some(Path::new("/home/op")));
        assert_eq!(got, Some(PathBuf::from("/xdg/tekops/config.toml")));
    }

    #[test]
    fn home_is_used_when_xdg_is_unset() {
        let got = path(None, Some(Path::new("/home/op")));
        assert_eq!(
            got,
            Some(PathBuf::from("/home/op/.config/tekops/config.toml"))
        );
    }

    /// An empty `$XDG_CONFIG_HOME` is how scripts and systemd units commonly
    /// "unset" it, and the XDG spec says to treat it as unset. Honouring it
    /// would resolve to a cwd-relative `tekops/config.toml` and silently skip
    /// the operator's real one.
    #[test]
    fn an_empty_xdg_config_home_falls_back_to_home() {
        let got = path(Some(Path::new("")), Some(Path::new("/home/op")));
        assert_eq!(
            got,
            Some(PathBuf::from("/home/op/.config/tekops/config.toml"))
        );
    }

    #[test]
    fn a_relative_xdg_config_home_falls_back_to_home() {
        let got = path(Some(Path::new("relative/dir")), Some(Path::new("/home/op")));
        assert_eq!(
            got,
            Some(PathBuf::from("/home/op/.config/tekops/config.toml"))
        );
    }

    /// A non-absolute XDG value with no HOME to fall back to is no path at all,
    /// never a cwd-relative one.
    #[test]
    fn a_relative_xdg_config_home_with_no_home_is_no_path() {
        assert_eq!(path(Some(Path::new("")), None), None);
        assert_eq!(path(Some(Path::new("relative/dir")), None), None);
    }

    /// No HOME and no XDG is "no config file", never an error. This is why
    /// `config` has its own resolver rather than reusing `completions::Dirs`,
    /// whose `home` is non-optional and whose `NoHome` is fatal.
    #[test]
    fn no_home_and_no_xdg_is_no_path() {
        assert_eq!(path(None, None), None);
    }

    #[test]
    fn no_path_loads_an_empty_config() {
        assert_eq!(load(None).unwrap(), Config::default());
    }

    #[test]
    fn an_absent_file_loads_an_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("tekops/config.toml");
        assert_eq!(load(Some(&missing)).unwrap(), Config::default());
    }

    #[test]
    fn a_present_file_is_read_and_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "stack = \"rocketpool\"\n").unwrap();
        assert_eq!(load(Some(&p)).unwrap().stack, Some(Stack::RocketPool));
    }

    /// A directory where a file is expected is a real operator mistake and
    /// must not be silently swallowed as "absent".
    #[test]
    fn an_unreadable_path_is_an_error_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(Some(dir.path())).unwrap_err();
        assert!(err.to_string().contains(&dir.path().display().to_string()));
    }

    fn read_sample() -> String {
        let sample_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        std::fs::read_to_string(&sample_path)
            .expect("config.example.toml must exist at the repo root")
    }

    /// Reads the sample's commented-out keys back as live TOML.
    ///
    /// The sample distinguishes the two kinds of comment by a single character:
    /// a key is written `#key = value` with no space after the hash, prose is
    /// written `# text` with one. That convention is what makes the file both
    /// safe to copy verbatim (every key inert) and still checkable here.
    ///
    /// The `contains(" = ")` guard means a prose line that happened to omit the
    /// space stays a comment rather than becoming malformed TOML.
    fn uncomment_keys(text: &str) -> String {
        text.lines()
            .map(|line| match line.strip_prefix('#') {
                Some(rest)
                    if rest.starts_with(|c: char| c.is_ascii_lowercase())
                        && rest.contains(" = ") =>
                {
                    rest
                }
                _ => line,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The convention `uncomment_keys` relies on is itself worth pinning: if the
    /// sample ever switches to `# key = value`, the keys stop being read back
    /// and the drift check silently starts comparing against an empty set.
    #[test]
    fn uncomment_keys_reads_keys_back_and_leaves_prose_alone() {
        let got = uncomment_keys("# Prose, with a space.\n#stack = \"eth-docker\"\n# more = prose");
        assert_eq!(
            got,
            "# Prose, with a space.\nstack = \"eth-docker\"\n# more = prose"
        );
    }

    /// The committed sample must stay in step with the struct it documents.
    ///
    /// A sample kept in sync by discipline drifts. This repo already solves that
    /// class of problem by having a test read the other artifact - see
    /// `ci_gates.rs`, which parses both `check.sh` and `ci.yml`, and
    /// `target_matches_the_names_build_release_publishes`, which reads
    /// `build-release.sh`. This is the same trade for `config.example.toml`.
    ///
    /// The field list comes from serializing a fully populated `Config` rather
    /// than from a list written out here, so the struct itself is what the sample
    /// is checked against. A hand-written list would just move the drift problem
    /// into this file.
    ///
    /// The assertion is set *equality*, in both directions: adding a field to
    /// `Config` without documenting it fails here, and so does leaving a removed
    /// one behind in the sample.
    ///
    /// The sample's keys are commented out, so copying the file verbatim is a
    /// no-op rather than a silent commitment to whichever stack the example
    /// happens to show - uncommenting a line is the operator's affirmative act.
    /// That is why `uncomment_keys` exists: a commented key *can* be checked,
    /// it just has to be read back first.
    #[test]
    fn the_sample_documents_exactly_the_configurable_keys() {
        use std::collections::BTreeSet;

        // Every field Some, so none is skipped by serialization.
        let populated = Config {
            stack: Some(Stack::EthDocker),
            api_url: Some("http://x".into()),
            bn_metric_url: Some("http://y".into()),
            vc_metric_url: Some("http://z".into()),
            metric_url: Some("http://w".into()),
            container: Some("c".into()),
            logs_file: Some(PathBuf::from("/a")),
            data_dir: Some(PathBuf::from("/b")),
        };
        let rendered = toml::to_string(&populated).expect("Config must serialize");
        let from_struct: BTreeSet<String> = rendered
            .parse::<toml::Table>()
            .expect("serialized Config must be valid TOML")
            .keys()
            .cloned()
            .collect();

        let from_sample: BTreeSet<String> = uncomment_keys(&read_sample())
            .parse::<toml::Table>()
            .expect("config.example.toml must be valid TOML once its keys are read back")
            .keys()
            .cloned()
            .collect();

        assert_eq!(
            from_sample, from_struct,
            "config.example.toml and Config have drifted apart"
        );
    }

    /// The sample is not merely key-correct: every value in it is of the type the
    /// real parser accepts, checked by reading the commented keys back.
    #[test]
    fn the_samples_values_are_all_of_the_right_type() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let cfg = parse(&uncomment_keys(&read_sample()), &path)
            .expect("every value in the shipped sample must be well typed");
        assert_eq!(cfg.stack, Some(Stack::EthDocker));
        assert!(cfg.api_url.is_some());
        assert!(cfg.data_dir.is_some());
    }

    /// Copying the sample verbatim must configure nothing at all. This is the
    /// property the commenting buys, and it is the whole reason the sync test
    /// goes to the trouble of reading the keys back.
    #[test]
    fn the_sample_as_shipped_configures_nothing() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let cfg = parse(&read_sample(), &path).expect("the shipped sample must parse as-is");
        assert_eq!(cfg, Config::default());
    }
}
