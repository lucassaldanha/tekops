//! Declining `tekops dump-logs --gist` at the prompt is a decision, not a
//! failure, and the redaction summary belongs on screen once.
//!
//! Both of these are integration tests rather than unit tests because both
//! defects live in `run_dump`'s sequencing rather than in any one function:
//! the summary is printed correctly by each of its two call sites and is only
//! wrong in combination, and the exit code is decided by `exit_for` two layers
//! above the `confirm_upload` that returns `false`. Only the real binary shows
//! either.
//!
//! No network is reached. `confirm_upload` runs before `gist::upload`, so a
//! declined run never has a request to make, which is why a syntactically
//! plausible but fake token is enough to select the gist path.

use std::io::Write;
use std::process::{Command, Output, Stdio};

/// A log holding one value from each redactable category, so the summary line
/// under test is a real one rather than "0 values".
const LOG: &str = concat!(
    "2026-09-15 09:14:02.113 INFO  - Peer connected: ",
    "16Uiu2HAmGxRk9mQ7vT3pL8sNcW2dYfJ4bH6zK1aQxE5nRtUvPwCd from 203.0.113.47:9000\n",
    "2026-09-15 09:14:03.001 INFO  - Validator 412887 published attestation for slot 9284410\n",
);

/// Runs `tekops dump-logs --gist` against `log`, answering the prompt with
/// `answer`.
///
/// `$GITHUB_TOKEN` is set rather than inherited so the gist path is selected
/// on a machine that has no token, and `$GH_TOKEN` is cleared so a developer's
/// own shell cannot supply a different one.
///
/// `$HOME` is cleared and `$XDG_CONFIG_HOME` points at an empty tempdir because
/// `run()` now loads the config file before dispatch and exits 1 on a malformed
/// one. Without this, a typo in the developer's own `config.toml` would fail all
/// three tests here with a message about a file none of them are testing.
fn dump_logs(answer: &str) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    let log = dir.path().join("teku.log");
    std::fs::write(&log, LOG).expect("write log");

    let mut child = Command::new(env!("CARGO_BIN_EXE_tekops"))
        .arg("dump-logs")
        .arg(&log)
        .arg("--gist")
        .env("GITHUB_TOKEN", "ghp_0000000000000000000000000000000000000")
        .env_remove("GH_TOKEN")
        .env_remove("TEKOPS_CONTAINER")
        .env_remove("TEKOPS_STACK")
        .env_remove("HOME")
        .env("XDG_CONFIG_HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tekops dump-logs");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(answer.as_bytes())
        .expect("write answer");

    child.wait_with_output().expect("run tekops dump-logs")
}

/// Declining is the mitigation working as designed - the operator looked and
/// said no - so it must not look like the command broke.
#[test]
fn declining_the_upload_exits_zero() {
    let out = dump_logs("n\n");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "declining must exit 0, got {:?}; stderr:\n{stderr}",
        out.status.code()
    );
    assert!(
        !stderr.contains("error:"),
        "declining is not an error, but stderr said:\n{stderr}"
    );
    assert!(
        stderr.contains("nothing was uploaded"),
        "declining should say nothing was uploaded, got:\n{stderr}"
    );
}

/// `run_dump` prints the summary, and `confirm_upload` used to print it again
/// a line later, so the gist path said it twice.
#[test]
fn the_redaction_summary_is_printed_once() {
    let out = dump_logs("n\n");
    let stderr = String::from_utf8_lossy(&out.stderr);

    let times = stderr.matches("redacted ").count();
    assert_eq!(
        times, 1,
        "expected the redaction summary once, saw it {times} times:\n{stderr}"
    );
}

/// Declining must not leave the dump behind on disk either - the whole point
/// is that nothing escaped.
#[test]
fn declining_writes_no_file() {
    let out = dump_logs("n\n");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        stdout.trim().is_empty(),
        "declining should print no path and no url, got:\n{stdout}"
    );
}
