use clap::ValueEnum;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

/// The shells `tekops autocomplete` can install for.
///
/// Deliberately not `clap_complete::Shell`, which also lists elvish and
/// powershell. Generating for those is free, but *installing* is not: each
/// needs a directory and an rc-equivalent this binary has never been run
/// against. Naming only what is supported lets clap reject the rest at parse
/// time, with the valid values listed, instead of failing at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

/// Picks a shell from the value of `$SHELL`, which is a path like `/bin/zsh`.
///
/// Takes the value as a parameter rather than reading the environment so it
/// stays a pure, race-free unit test - the same reason `logs::resolve_log_target`
/// does. An unrecognized or absent shell yields `None`; the caller turns that
/// into "name one explicitly" rather than guessing.
pub fn detect_shell(shell_env: Option<&str>) -> Option<Shell> {
    match shell_env?.rsplit('/').next()? {
        "bash" => Some(Shell::Bash),
        "zsh" => Some(Shell::Zsh),
        "fish" => Some(Shell::Fish),
        _ => None,
    }
}

/// Tags the block this command appends to an rc file, so a second run can
/// recognize its own work and skip it. Everything installed outside the
/// completion file itself sits between this line and the end of the stanza,
/// which is also what makes uninstalling a hand-editable three-line delete.
pub const MARKER: &str = "# added by tekops";

/// The directories `plan` builds paths from.
///
/// Passed in rather than read from the environment inside `plan`, so the whole
/// path computation is a pure function testable against a tempdir - the same
/// shape `logs::resolve_log_target` uses.
pub struct Dirs {
    pub home: PathBuf,
    pub xdg_data: Option<PathBuf>,
    pub xdg_config: Option<PathBuf>,
}

/// A block to append to a shell's rc file, and where.
pub struct RcEdit {
    pub path: PathBuf,
    pub stanza: String,
}

/// Everything `tekops autocomplete` would write, computed before anything is
/// written so it can be shown to the user and confirmed.
pub struct Plan {
    pub script_path: PathBuf,
    /// `None` for shells that auto-load the completion directory (fish).
    pub rc: Option<RcEdit>,
}

/// Works out where this shell's completion script belongs and what, if
/// anything, has to be added to an rc file for the shell to find it.
pub fn plan(shell: Shell, dirs: &Dirs) -> Plan {
    let home = &dirs.home;
    let xdg_data = dirs
        .xdg_data
        .clone()
        .unwrap_or_else(|| home.join(".local/share"));
    let xdg_config = dirs
        .xdg_config
        .clone()
        .unwrap_or_else(|| home.join(".config"));

    match shell {
        // fish scans its completions directory on every invocation, so the
        // file landing there is the whole install.
        Shell::Fish => Plan {
            script_path: xdg_config.join("fish/completions/tekops.fish"),
            rc: None,
        },
        // bash-completion's own dynamic-loading directory, but the stanza does
        // not rely on that package being installed: the generated script is
        // self-contained (plain COMP_WORDS, and it branches on BASH_VERSINFO
        // for bash 3.2), so sourcing it directly works on stock macOS bash too.
        // Sourcing a file bash-completion also loads is harmless - it just
        // redefines the function and re-runs `complete`.
        Shell::Bash => {
            let script_path = xdg_data.join("bash-completion/completions/tekops");
            let rendered = home_relative(&script_path, home);
            Plan {
                rc: Some(RcEdit {
                    path: home.join(bash_rc_name(cfg!(target_os = "macos"))),
                    stanza: format!("\n{MARKER}\n[ -f \"{rendered}\" ] && . \"{rendered}\"\n"),
                }),
                script_path,
            }
        }
        // zsh has no user completion directory that is on `fpath` by default,
        // so unlike the other two this genuinely needs the rc edit. compinit
        // has to run *after* the directory joins fpath, which for an appended
        // stanza means running it a second time - redundant for framework
        // users, and correct for everyone.
        Shell::Zsh => {
            let dir = home.join(".zfunc");
            let rendered = home_relative(&dir, home);
            Plan {
                script_path: dir.join("_tekops"),
                rc: Some(RcEdit {
                    path: home.join(".zshrc"),
                    stanza: format!(
                        "\n{MARKER}\nfpath=(\"{rendered}\" $fpath)\nautoload -Uz compinit && compinit\n"
                    ),
                }),
            }
        }
    }
}

/// Rewrites every completion script that is already installed, using `binary`
/// to render them.
///
/// This is what keeps an installed script honest across a `tekops update`: the
/// file on disk is a snapshot of the CLI as it was, so a release that adds a
/// command leaves it stale. It must run the *newly installed* binary rather
/// than generating in-process, because the running process is the old build and
/// would faithfully write the old command set.
///
/// Only files that already exist are touched. Their presence is the user's
/// opt-in; this never installs completions for a shell they did not ask about,
/// and never edits an rc file.
pub fn refresh_installed(binary: &Path, dirs: &Dirs) -> Result<Vec<PathBuf>, CompletionError> {
    let mut refreshed = Vec::new();
    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
        let plan = plan(shell, dirs);
        if !plan.script_path.exists() {
            continue;
        }
        let script = render_with(binary, shell, &plan.script_path)?;
        fs::write(&plan.script_path, script).map_err(|e| CompletionError::Io {
            path: plan.script_path.clone(),
            message: e.to_string(),
        })?;
        refreshed.push(plan.script_path);
    }
    Ok(refreshed)
}

/// Runs `binary autocomplete <shell> --print` and returns what it wrote.
///
/// `script_path` is only used to name the file in an error, since that is the
/// thing the user cares about when a refresh fails. The whole output is
/// buffered before anything is written, so a binary that fails partway through
/// cannot leave a truncated completion script on disk.
fn render_with(
    binary: &Path,
    shell: Shell,
    script_path: &Path,
) -> Result<Vec<u8>, CompletionError> {
    // Taken from clap rather than a second hand-written mapping, so the name
    // passed here is by construction the one the CLI accepts.
    let name = shell
        .to_possible_value()
        .expect("every Shell variant is a clap value")
        .get_name()
        .to_string();

    let output = process::Command::new(binary)
        .args(["autocomplete", &name, "--print"])
        .output()
        .map_err(|e| CompletionError::Io {
            path: script_path.to_path_buf(),
            message: format!("could not run {}: {e}", binary.display()),
        })?;

    if !output.status.success() {
        return Err(CompletionError::Io {
            path: script_path.to_path_buf(),
            message: format!(
                "{} could not render {name} completions: {}",
                binary.display(),
                // Untrusted only in the sense that it is another process's
                // output landing on this terminal; sanitized like every other
                // such channel in this binary.
                crate::term::sanitize(&String::from_utf8_lossy(&output.stderr)).trim()
            ),
        });
    }
    Ok(output.stdout)
}

/// What happened to the rc file, which is the only part of an install that is
/// not simply "a file was written".
#[derive(Debug, PartialEq, Eq)]
pub enum RcOutcome {
    /// The shell auto-loads its completion directory, so there was nothing to add.
    NotNeeded,
    /// A previous run's stanza is already there; appending again would
    /// duplicate it, so it was left alone.
    AlreadyPresent(PathBuf),
    Appended(PathBuf),
}

#[derive(Debug)]
pub struct Applied {
    pub script_path: PathBuf,
    pub rc: RcOutcome,
}

#[derive(Debug)]
pub enum CompletionError {
    Io { path: PathBuf, message: String },
    NoHome,
    UndetectedShell,
}

impl fmt::Display for CompletionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompletionError::Io { path, message } => {
                write!(f, "could not write {}: {message}", path.display())
            }
            CompletionError::NoHome => {
                write!(f, "could not determine your home directory ($HOME is not set)")
            }
            CompletionError::UndetectedShell => write!(
                f,
                "could not detect your shell from $SHELL; name one: tekops autocomplete <bash|zsh|fish>"
            ),
        }
    }
}

/// Writes the completion script, and appends the rc stanza if it is both needed
/// and not already there.
///
/// The script file is always overwritten - that is the refresh path after the
/// CLI gains a command. The rc stanza is append-once, guarded by [`MARKER`], so
/// re-running never duplicates it.
pub fn apply(plan: &Plan, script: &str) -> Result<Applied, CompletionError> {
    if let Some(parent) = plan.script_path.parent() {
        fs::create_dir_all(parent).map_err(|e| CompletionError::Io {
            path: parent.to_path_buf(),
            message: e.to_string(),
        })?;
    }
    fs::write(&plan.script_path, script).map_err(|e| CompletionError::Io {
        path: plan.script_path.clone(),
        message: e.to_string(),
    })?;

    let rc = match &plan.rc {
        None => RcOutcome::NotNeeded,
        Some(edit) => {
            // A missing rc file is normal, not an error: a fresh account may
            // have no .zshrc at all, and the stanza is what creates it.
            let existing = match fs::read_to_string(&edit.path) {
                Ok(contents) => contents,
                Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
                Err(e) => {
                    return Err(CompletionError::Io {
                        path: edit.path.clone(),
                        message: e.to_string(),
                    })
                }
            };
            if existing.contains(MARKER) {
                RcOutcome::AlreadyPresent(edit.path.clone())
            } else {
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&edit.path)
                    .map_err(|e| CompletionError::Io {
                        path: edit.path.clone(),
                        message: e.to_string(),
                    })?;
                file.write_all(edit.stanza.as_bytes())
                    .map_err(|e| CompletionError::Io {
                        path: edit.path.clone(),
                        message: e.to_string(),
                    })?;
                RcOutcome::Appended(edit.path.clone())
            }
        }
    };

    Ok(Applied {
        script_path: plan.script_path.clone(),
        rc,
    })
}

/// Renders this shell's completion script from the live `clap::Command`.
///
/// This is what makes the installed script impossible to let drift: there is no
/// checked-in copy to regenerate and no build step to forget. Whatever
/// subcommands and flags `cli` defines at the moment of the call are what comes
/// out.
pub fn generate(shell: Shell, cmd: &mut clap::Command) -> String {
    let mut out = Vec::new();
    match shell {
        Shell::Bash => {
            clap_complete::generate(clap_complete::shells::Bash, cmd, "tekops", &mut out)
        }
        Shell::Zsh => clap_complete::generate(clap_complete::shells::Zsh, cmd, "tekops", &mut out),
        Shell::Fish => {
            clap_complete::generate(clap_complete::shells::Fish, cmd, "tekops", &mut out)
        }
    }
    // The generators write only what they rendered from the command
    // definition, which is ASCII shell source by construction.
    String::from_utf8(out).expect("clap_complete emits UTF-8")
}

/// Which rc file bash reads on this platform.
///
/// macOS Terminal starts bash as a *login* shell, and a login bash reads
/// `~/.bash_profile` and never `~/.bashrc`. Writing the stanza to `.bashrc`
/// there installs a completion that silently never loads. Takes the platform
/// as a parameter so both branches are testable from either machine.
fn bash_rc_name(is_macos: bool) -> &'static str {
    if is_macos {
        ".bash_profile"
    } else {
        ".bashrc"
    }
}

/// Renders a path for embedding in an rc file, preferring `$HOME/...` over a
/// hardcoded home directory so a synced dotfile still works on another machine.
fn home_relative(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("$HOME/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs() -> Dirs {
        Dirs {
            home: PathBuf::from("/home/lucas"),
            xdg_data: Some(PathBuf::from("/home/lucas/xdg-data")),
            xdg_config: Some(PathBuf::from("/home/lucas/xdg-config")),
        }
    }

    #[test]
    fn fish_installs_into_its_auto_loaded_directory_and_needs_no_rc_edit() {
        let plan = plan(Shell::Fish, &dirs());

        assert_eq!(
            plan.script_path,
            PathBuf::from("/home/lucas/xdg-config/fish/completions/tekops.fish")
        );
        assert!(plan.rc.is_none());
    }

    #[test]
    fn zsh_installs_into_zfunc_and_puts_it_on_fpath_before_compinit() {
        let plan = plan(Shell::Zsh, &dirs());

        assert_eq!(
            plan.script_path,
            PathBuf::from("/home/lucas/.zfunc/_tekops")
        );
        let rc = plan.rc.expect("zsh needs an rc edit");
        assert_eq!(rc.path, PathBuf::from("/home/lucas/.zshrc"));
        assert!(rc.stanza.contains(MARKER));
        assert!(rc.stanza.contains("fpath=(\"$HOME/.zfunc\" $fpath)"));
        // Must follow the fpath line: a framework's own compinit has already
        // run by the time an appended stanza is reached, so the directory is
        // only searched if compinit runs again after it joins fpath.
        let fpath_at = rc.stanza.find("fpath=").expect("fpath line");
        let compinit_at = rc.stanza.find("compinit").expect("compinit line");
        assert!(fpath_at < compinit_at);
    }

    #[test]
    fn bash_installs_into_the_bash_completion_directory_and_sources_it_directly() {
        let plan = plan(Shell::Bash, &dirs());

        assert_eq!(
            plan.script_path,
            PathBuf::from("/home/lucas/xdg-data/bash-completion/completions/tekops")
        );
        // The generated bash script is self-contained, so sourcing it works
        // with or without the bash-completion package installed - which stock
        // macOS does not have. The guard keeps the rc file valid if the
        // completion file is later deleted.
        let rc = plan.rc.expect("bash needs an rc edit");
        assert!(rc.stanza.contains(MARKER));
        assert!(rc
            .stanza
            .contains("[ -f \"$HOME/xdg-data/bash-completion/completions/tekops\" ]"));
        assert!(rc
            .stanza
            .contains(". \"$HOME/xdg-data/bash-completion/completions/tekops\""));
    }

    #[test]
    fn xdg_directories_fall_back_to_their_specified_defaults() {
        let bare = Dirs {
            home: PathBuf::from("/home/lucas"),
            xdg_data: None,
            xdg_config: None,
        };

        assert_eq!(
            plan(Shell::Bash, &bare).script_path,
            PathBuf::from("/home/lucas/.local/share/bash-completion/completions/tekops")
        );
        assert_eq!(
            plan(Shell::Fish, &bare).script_path,
            PathBuf::from("/home/lucas/.config/fish/completions/tekops.fish")
        );
    }

    /// A `Dirs` rooted in a tempdir, so `apply` writes somewhere real and
    /// throwaway rather than into the developer's actual shell config.
    fn sandbox(dir: &tempfile::TempDir) -> Dirs {
        Dirs {
            home: dir.path().to_path_buf(),
            xdg_data: None,
            xdg_config: None,
        }
    }

    #[test]
    fn installing_creates_the_completion_file_and_the_directories_above_it() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(Shell::Fish, &sandbox(&dir));

        let applied = apply(&plan, "# script").unwrap();

        assert_eq!(
            std::fs::read_to_string(&plan.script_path).unwrap(),
            "# script"
        );
        assert_eq!(applied.rc, RcOutcome::NotNeeded);
    }

    #[test]
    fn installing_appends_the_stanza_to_an_rc_file_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(Shell::Zsh, &sandbox(&dir));
        let rc = plan.rc.as_ref().unwrap().path.clone();

        let applied = apply(&plan, "#compdef tekops").unwrap();

        assert_eq!(applied.rc, RcOutcome::Appended(rc.clone()));
        assert!(std::fs::read_to_string(&rc).unwrap().contains(MARKER));
    }

    #[test]
    fn installing_preserves_what_the_rc_file_already_contained() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(Shell::Zsh, &sandbox(&dir));
        let rc = plan.rc.as_ref().unwrap().path.clone();
        std::fs::write(&rc, "export EDITOR=vim\n").unwrap();

        apply(&plan, "#compdef tekops").unwrap();

        let contents = std::fs::read_to_string(&rc).unwrap();
        assert!(contents.starts_with("export EDITOR=vim\n"));
        assert!(contents.contains(MARKER));
    }

    #[test]
    fn reinstalling_refreshes_the_script_without_appending_the_stanza_twice() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(Shell::Zsh, &sandbox(&dir));
        let rc = plan.rc.as_ref().unwrap().path.clone();
        apply(&plan, "old script").unwrap();

        let applied = apply(&plan, "new script").unwrap();

        // The script is a snapshot of the CLI, so a second run must replace it.
        assert_eq!(
            std::fs::read_to_string(&plan.script_path).unwrap(),
            "new script"
        );
        assert_eq!(applied.rc, RcOutcome::AlreadyPresent(rc.clone()));
        let contents = std::fs::read_to_string(&rc).unwrap();
        assert_eq!(contents.matches(MARKER).count(), 1);
    }

    #[test]
    fn a_failed_write_names_the_path_it_could_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(Shell::Fish, &sandbox(&dir));
        // A file where the completions directory needs to be, so creating the
        // parent directory fails for a reason the message has to explain.
        std::fs::create_dir_all(dir.path().join(".config")).unwrap();
        std::fs::write(dir.path().join(".config/fish"), "not a directory").unwrap();

        let err = apply(&plan, "# script").unwrap_err();

        assert!(matches!(err, CompletionError::Io { .. }));
        assert!(err.to_string().contains("fish"));
    }

    /// Stands in for a freshly installed tekops: echoes which shell it was
    /// asked for, so a test can tell the refreshed content came from running
    /// this binary rather than from the in-process generator.
    fn fake_binary(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join("tekops-stand-in");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn refreshing_rewrites_only_the_completions_that_are_already_installed() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = sandbox(&dir);
        let zsh = plan(Shell::Zsh, &dirs);
        apply(&zsh, "stale script").unwrap();
        let fish = plan(Shell::Fish, &dirs);
        let binary = fake_binary(&dir, "#!/bin/sh\necho \"fresh $2\"\n");

        let refreshed = refresh_installed(&binary, &dirs).unwrap();

        assert_eq!(refreshed, vec![zsh.script_path.clone()]);
        assert_eq!(
            std::fs::read_to_string(&zsh.script_path).unwrap(),
            "fresh zsh\n"
        );
        // Never installs for a shell the user did not already opt into.
        assert!(!fish.script_path.exists());
    }

    #[test]
    fn refreshing_leaves_the_rc_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = sandbox(&dir);
        let zsh = plan(Shell::Zsh, &dirs);
        apply(&zsh, "stale script").unwrap();
        let rc = zsh.rc.as_ref().unwrap().path.clone();
        let before = std::fs::read_to_string(&rc).unwrap();

        refresh_installed(&fake_binary(&dir, "#!/bin/sh\necho fresh\n"), &dirs).unwrap();

        assert_eq!(std::fs::read_to_string(&rc).unwrap(), before);
    }

    #[test]
    fn refreshing_fails_loudly_when_the_binary_cannot_render_a_script() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = sandbox(&dir);
        let zsh = plan(Shell::Zsh, &dirs);
        apply(&zsh, "stale script").unwrap();
        let binary = fake_binary(&dir, "#!/bin/sh\nexit 3\n");

        let err = refresh_installed(&binary, &dirs).unwrap_err();

        assert!(err.to_string().contains("_tekops"));
        // A failed refresh must not leave a truncated script behind.
        assert_eq!(
            std::fs::read_to_string(&zsh.script_path).unwrap(),
            "stale script"
        );
    }

    #[test]
    fn the_generated_script_covers_the_commands_the_cli_actually_defines() {
        let script = generate(Shell::Zsh, &mut crate::cli::command());

        // Named commands rather than a non-empty check, so a generator that
        // emitted only the bare skeleton would still fail. `log-level` is here
        // because its kebab-case rename is the kind of thing a hand-maintained
        // script gets wrong.
        assert!(script.contains("beacon"));
        assert!(script.contains("log-level"));
        assert!(script.contains("autocomplete"));
    }

    /// Runs each installed shell's own parser over the script we would write.
    ///
    /// The point is to catch a generator emitting something the shell cannot
    /// parse, which no assertion about the string's contents can. Shells that
    /// are not installed are skipped - CI's Linux runner has no zsh or fish -
    /// so the test asserts it checked at least one, rather than passing
    /// vacuously on a host with none.
    #[test]
    fn every_generated_script_parses_under_its_own_shell() {
        use std::process::Command;

        let mut checked = 0;
        for (shell, bin, flag) in [
            (Shell::Bash, "bash", "-n"),
            (Shell::Zsh, "zsh", "-n"),
            (Shell::Fish, "fish", "--no-execute"),
        ] {
            if Command::new(bin).arg("--version").output().is_err() {
                continue;
            }
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("script");
            std::fs::write(&path, generate(shell, &mut crate::cli::command())).unwrap();

            let out = Command::new(bin).arg(flag).arg(&path).output().unwrap();
            assert!(
                out.status.success(),
                "{bin} rejected the generated script: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            checked += 1;
        }
        assert!(
            checked > 0,
            "no supported shell was available to parse-check"
        );
    }

    #[test]
    fn bash_reads_bash_profile_on_macos_and_bashrc_elsewhere() {
        assert_eq!(bash_rc_name(true), ".bash_profile");
        assert_eq!(bash_rc_name(false), ".bashrc");
    }

    #[test]
    fn paths_under_home_are_rendered_relative_to_home() {
        let home = Path::new("/Users/lucas");
        assert_eq!(
            home_relative(Path::new("/Users/lucas/.zfunc"), home),
            "$HOME/.zfunc"
        );
    }

    #[test]
    fn paths_outside_home_are_rendered_absolute() {
        let home = Path::new("/Users/lucas");
        assert_eq!(
            home_relative(Path::new("/opt/share/tekops"), home),
            "/opt/share/tekops"
        );
    }

    #[test]
    fn detects_each_supported_shell_from_a_shell_path() {
        assert_eq!(detect_shell(Some("/bin/bash")), Some(Shell::Bash));
        assert_eq!(detect_shell(Some("/bin/zsh")), Some(Shell::Zsh));
        assert_eq!(detect_shell(Some("/usr/local/bin/fish")), Some(Shell::Fish));
    }

    #[test]
    fn detects_nothing_from_an_unset_or_unsupported_shell() {
        assert_eq!(detect_shell(None), None);
        assert_eq!(detect_shell(Some("/usr/bin/elvish")), None);
        assert_eq!(detect_shell(Some("")), None);
    }
}
