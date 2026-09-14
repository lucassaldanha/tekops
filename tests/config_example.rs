//! The shipped binary accepts the shipped sample.
//!
//! The key-set and type checks are unit tests in `src/config.rs`, which is the
//! only place that can see `Config`. What is left for an integration test is
//! the thing only a real invocation proves: that a sample copied to the place
//! the docs tell an operator to copy it to is actually accepted.

/// The sample, installed exactly as USAGE.md tells an operator to install it.
#[test]
fn the_sample_is_accepted_by_the_real_binary() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("tekops");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::copy("config.example.toml", cfg_dir.join("config.toml")).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tekops"))
        .arg("about")
        .env("XDG_CONFIG_HOME", dir.path())
        .env_remove("HOME")
        .output()
        .expect("tekops must run");

    assert!(
        out.status.success(),
        "the sample config must be accepted: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
