//! `tekops` reads its config file, end to end, through a real invocation.
//!
//! The unit tests cover each rung of each ladder in isolation. This covers the
//! wiring between them: that `$XDG_CONFIG_HOME` is actually consulted, that
//! the file is actually read, and that a value in it actually reaches the
//! request. Every `$TEKOPS_*` variable is cleared so a developer's own shell
//! cannot change the result.

use std::process::Command;

fn tekops(config: Option<&str>, dir: &std::path::Path) -> Command {
    if let Some(text) = config {
        let cfg_dir = dir.join("tekops");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.toml"), text).unwrap();
    }
    let mut c = Command::new(env!("CARGO_BIN_EXE_tekops"));
    c.env("XDG_CONFIG_HOME", dir)
        .env_remove("HOME")
        .env_remove("TEKOPS_STACK")
        .env_remove("TEKOPS_API_URL")
        .env_remove("TEKOPS_METRIC_URL")
        .env_remove("TEKOPS_CONTAINER")
        .env_remove("TEKOPS_LOGS_FILE")
        .env_remove("TEKOPS_DATA_DIR");
    c
}

/// The config's `api_url` reaches the request: pointed at a port nothing
/// listens on, the error names that port rather than the default 5051.
#[test]
fn the_config_api_url_is_what_gets_dialled() {
    let dir = tempfile::tempdir().unwrap();
    let out = tekops(Some("api_url = \"http://127.0.0.1:1/\"\n"), dir.path())
        .arg("health")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a dead port must fail: {stderr}");
    assert!(
        stderr.contains("127.0.0.1:1"),
        "the config's url must be the one dialled, got: {stderr}"
    );
}

/// The environment still beats the file.
#[test]
fn the_env_api_url_beats_the_config_api_url() {
    let dir = tempfile::tempdir().unwrap();
    let out = tekops(Some("api_url = \"http://127.0.0.1:1/\"\n"), dir.path())
        .env("TEKOPS_API_URL", "http://127.0.0.1:2/")
        .arg("health")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("127.0.0.1:2"),
        "the variable must win, got: {stderr}"
    );
}

/// The flag still beats both.
#[test]
fn the_api_url_flag_beats_the_env_and_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let out = tekops(Some("api_url = \"http://127.0.0.1:1/\"\n"), dir.path())
        .env("TEKOPS_API_URL", "http://127.0.0.1:2/")
        .args(["health", "--api-url", "http://127.0.0.1:3/"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("127.0.0.1:3"),
        "the flag must win, got: {stderr}"
    );
}

/// A broken config fails any command, including one that reads none of the
/// six keys, and the message names the file to open.
#[test]
fn a_broken_config_fails_even_a_command_that_reads_none_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let out = tekops(Some("api_ur1 = \"http://x\"\n"), dir.path())
        .arg("about")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "a broken config must fail");
    assert!(
        stderr.contains("config.toml"),
        "must name the file: {stderr}"
    );
    assert!(stderr.contains("api_ur1"), "must name the key: {stderr}");
}

/// No config file at all is the common case and must be silent.
#[test]
fn an_absent_config_file_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = tekops(None, dir.path()).arg("about").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
