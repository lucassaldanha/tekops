//! `tekops doctor` must not mention Docker when the report it prints is a
//! bare-metal one (issue #16).
//!
//! This is an integration test rather than a unit test on purpose. The defect
//! was never inside a single function: `docker_ps_names` reported a failed
//! `docker ps` at spawn time, which is before doctor's stack ladder has
//! resolved, so no pure function had both facts in hand to be wrong about.
//! Only the wiring between the two can be, which means only the real binary
//! can demonstrate it.
//!
//! The `docker` on `$PATH` here fails the way a stopped daemon does - present,
//! runnable, and refusing - since Docker being absent entirely was always
//! silent and is not what the issue is about.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

/// A directory containing a `docker` that behaves like a stopped daemon.
fn stopped_daemon() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("docker");
    fs::write(
        &bin,
        "#!/bin/sh\n\
         echo 'Cannot connect to the Docker daemon at unix:///var/run/docker.sock.' >&2\n\
         exit 1\n",
    )
    .expect("write docker stub");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod docker stub");
    dir
}

/// Runs `tekops doctor` with `dir` prepended to `$PATH`.
///
/// Both URLs point at port 1, which is refused instantly on every platform
/// this builds for: the endpoint checks are noise for this test, and a port
/// that merely happens to be closed could cost the 10s request timeout twice.
/// Every `$TEKOPS_*` variable is cleared so a developer's own shell cannot
/// answer a rung of the ladder under test. `$HOME` is cleared and
/// `$XDG_CONFIG_HOME` is pointed at `dir` (which holds the `docker` stub and no
/// `tekops/` subdirectory) for the same reason: the config file is a rung of
/// that same ladder, so a real `~/.config/tekops/config.toml` on the developer's
/// machine would otherwise decide the stack this test is asserting about.
fn doctor(dir: &Path, args: &[&str]) -> Output {
    let path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(env!("CARGO_BIN_EXE_tekops"))
        .arg("doctor")
        .args(["--api-url", "http://127.0.0.1:1"])
        .args(["--metric-url", "http://127.0.0.1:1"])
        .args(args)
        .env("PATH", path)
        .env_remove("TEKOPS_STACK")
        .env_remove("TEKOPS_API_URL")
        .env_remove("TEKOPS_METRIC_URL")
        .env_remove("TEKOPS_CONTAINER")
        .env_remove("TEKOPS_DATA_DIR")
        .env_remove("HOME")
        .env("XDG_CONFIG_HOME", dir)
        .output()
        .expect("run tekops doctor")
}

/// The reported bug: no `--stack`, so detection runs, fails, and the ladder
/// falls back to bare-metal - a report about a node that has no containers,
/// which must not carry a note about Docker.
#[test]
fn doctor_says_nothing_about_docker_when_the_report_is_bare_metal() {
    let dir = stopped_daemon();
    let out = doctor(dir.path(), &[]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("bare-metal"),
        "expected a bare-metal report, got:\n{stdout}"
    );
    assert!(
        !stderr.to_lowercase().contains("docker"),
        "bare-metal report must not mention docker, but stderr said:\n{stderr}"
    );
}

/// Declaring bare-metal was already silent, and has to stay that way: it is
/// the case that proved the gate belonged at the consumer rather than at the
/// `docker ps` spawn.
#[test]
fn doctor_says_nothing_about_docker_on_a_declared_bare_metal_stack() {
    let dir = stopped_daemon();
    let out = doctor(dir.path(), &["--stack", "bare-metal"]);

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.to_lowercase().contains("docker"),
        "bare-metal report must not mention docker, but stderr said:\n{stderr}"
    );
}

/// The other half of the fix: silence is scoped to the stacks that have no
/// containers. On one that does, doctor wanted an answer from Docker and did
/// not get one, so the container check below goes unexplained without it.
#[test]
fn doctor_reports_an_unaskable_docker_on_a_stack_that_has_containers() {
    for stack in ["eth-docker", "rocketpool"] {
        let dir = stopped_daemon();
        let out = doctor(dir.path(), &["--stack", stack]);

        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("`docker ps` failed"),
            "{stack} needs docker, so a failed `docker ps` must be reported; stderr said:\n{stderr}"
        );
    }
}
