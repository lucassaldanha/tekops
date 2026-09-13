//! Guards that the local pre-push gate and CI check the same things.
//!
//! Lives here rather than in a module's `#[cfg(test)]` because it tests the
//! repository's configuration, not any of its code.

/// The cargo invocations `.github/workflows/ci.yml` runs as gates, in order.
///
/// Only single-line `run: cargo ...` steps count. The toolchain step uses a
/// `run: |` block whose `cargo --version` line is not a gate, and skipping it
/// falls out of the parse rather than needing a special case.
fn ci_gates() -> Vec<String> {
    include_str!("../.github/workflows/ci.yml")
        .lines()
        .filter_map(|line| line.trim().strip_prefix("run: "))
        .filter(|command| command.starts_with("cargo "))
        .map(str::to_string)
        .collect()
}

/// The cargo invocations `scripts/check.sh` runs, in order. They are the only
/// unindented `cargo ` lines in the file.
fn local_gates() -> Vec<String> {
    include_str!("../scripts/check.sh")
        .lines()
        .filter(|line| line.starts_with("cargo "))
        .map(str::to_string)
        .collect()
}

/// The whole point of `scripts/check.sh` is that passing it locally means CI
/// will pass too. Nothing but this test connects the two files - they are in
/// different languages and neither reads the other - so a gate added, removed,
/// or reworded on one side lands here rather than as a surprise red build
/// after a push that was supposed to be pre-verified.
#[test]
fn local_check_runs_the_same_gates_as_ci() {
    let ci = ci_gates();
    let local = local_gates();

    assert!(!ci.is_empty(), "parsed no gates out of ci.yml");
    assert_eq!(
        ci, local,
        "ci.yml and scripts/check.sh disagree about the gates"
    );
}

/// The parse above is only meaningful if it actually finds the real commands;
/// a typo that made both sides parse as empty would otherwise pass. Assert the
/// formatting gate is present and first, which is the order CI documents.
#[test]
fn the_format_gate_is_present_and_runs_first() {
    let ci = ci_gates();
    assert_eq!(
        ci.first().map(String::as_str),
        Some("cargo fmt --all -- --check"),
        "formatting is the cheapest gate to fail and must stay first"
    );
}

/// The pre-push hook has to actually call the script, or the local gate is
/// installed but inert.
#[test]
fn the_pre_push_hook_runs_the_check_script() {
    let hook = include_str!("../.githooks/pre-push");
    assert!(
        hook.contains("scripts/check.sh"),
        "pre-push hook does not run scripts/check.sh"
    );
}
