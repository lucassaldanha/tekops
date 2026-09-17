use crate::logfmt::format_log_line;
use crate::merge::Source;
use std::io::{self, BufRead, BufReader, LineWriter, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread;

/// How much colorized output one session may buffer before it stops following.
///
/// The buffer is a real file (so `less` can search it - see the note in
/// `run_logs`), and it only ever grows. On a busy node an open-ended
/// session would grow it without bound, which matters because `/tmp` is tmpfs
/// on most systemd distros: that is RAM, on the machine running the validator.
/// Stopping is the only bound that keeps the pager's view coherent, since
/// truncating a file `less` is holding offsets into corrupts what it displays.
pub const MAX_BUFFER_BYTES: u64 = 256 * 1024 * 1024;

pub fn stream_logs<R: BufRead, W: Write>(reader: R, writer: &mut W) -> io::Result<()> {
    stream_logs_capped(reader, writer, MAX_BUFFER_BYTES)
}

/// Formats each line into `writer` until the reader ends or `max_bytes` have
/// been written, whichever comes first. Hitting the cap is a normal end to the
/// stream, not an error: the session keeps working as scrollback, so it says so
/// in-band and returns `Ok`.
pub fn stream_logs_capped<R: BufRead, W: Write>(
    reader: R,
    writer: &mut W,
    max_bytes: u64,
) -> io::Result<()> {
    let mut written: u64 = 0;
    for line in reader.lines() {
        let line = line?;
        let formatted = format_log_line(&line);
        // +1 for the newline `writeln!` adds.
        let next = written.saturating_add(formatted.len() as u64 + 1);
        if next > max_bytes {
            writeln!(writer)?;
            writeln!(
                writer,
                "*** tekops: {} MiB buffer limit reached, stopped following.",
                max_bytes / (1024 * 1024)
            )?;
            writeln!(
                writer,
                "*** Scrollback and search still work. Quit and rerun to resume."
            )?;
            writer.flush()?;
            return Ok(());
        }
        writeln!(writer, "{formatted}")?;
        written = next;
    }
    Ok(())
}

/// The path the old bashrc function tailed.
pub(crate) const DEFAULT_TEKU_LOG: &str = "/var/log/teku/teku.log";

/// Where one `tekops logs` session reads from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogTarget {
    File(PathBuf),
    Container(String),
}

/// What one `tekops logs` session reads, per process.
///
/// `None` in a slot means that process has no log source, which is the normal
/// state of an all-in-one node's `vc` slot - not an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogSources {
    pub bn: Option<LogTarget>,
    pub vc: Option<LogTarget>,
}

// `run_logs` only reads `.bn` until the task that wires `merge::Merger` into
// it drives both slots through `active()`; until then `active` and
// `is_empty` are unreached from `main` and `-D warnings` would fail the
// build on them, same as the `#[allow(dead_code)]` block in merge.rs.
#[allow(dead_code)]
impl LogSources {
    /// The sources that resolved, in a stable order. Feeds `Merger::new`, so
    /// the order here is also the tie-break order for equal timestamps.
    pub fn active(&self) -> Vec<Source> {
        let mut out = Vec::new();
        if self.bn.is_some() {
            out.push(Source::Bn);
        }
        if self.vc.is_some() {
            out.push(Source::Vc);
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.bn.is_none() && self.vc.is_none()
    }
}

/// Everything that can say where the logs are.
///
/// A struct rather than fifteen positional parameters, and every field
/// arrives as a value rather than being read here, so the whole precedence
/// ladder is testable with no environment races and no Docker installed.
pub struct SourceInputs {
    pub path: Option<PathBuf>,
    pub container_flag: Option<String>,
    pub vc_container_flag: Option<String>,
    pub vc_logs_file_flag: Option<PathBuf>,
    pub container_env: Option<String>,
    pub logs_file_env: Option<String>,
    pub vc_container_env: Option<String>,
    pub vc_logs_file_env: Option<String>,
    pub container_cfg: Option<String>,
    pub logs_file_cfg: Option<PathBuf>,
    pub vc_container_cfg: Option<String>,
    pub vc_logs_file_cfg: Option<PathBuf>,
    pub detected_bn: Option<String>,
    pub detected_vc: Option<String>,
    /// `--bn` / `--vc`: filters what resolved, rather than naming anything.
    pub select: Option<Source>,
}

/// Resolves both processes' log sources.
///
/// Each slot runs the same ladder the single source used to:
/// flag > environment > config file > detection. Within a tier, naming a
/// container beats naming a file, because it is the more specific statement.
///
/// Three rules sit on top, and each exists to stop a specific wrong answer:
///
/// 1. **Stating one side by flag or environment variable yields that side
///    only.** The config file does not count, and neither does detection -
///    only the flag and environment tiers gate this. A flag or a variable is
///    typed for this invocation; config is ambient, describing the node
///    rather than expressing an intent for this run, so it free-mixes with
///    the other side exactly as detection does. This is what keeps `tekops
///    logs --container rocketpool_validator` printing exactly the one stream
///    it prints today, on a host where detection also finds a consensus
///    container - an invocation that works today must keep working
///    unchanged. It also means a *configured* `container` alongside a
///    *detected* validator now yields both streams: that is the separated
///    deployment this feature exists for, and the new behaviour is
///    intentional, not a regression of rule 1.
/// 2. **The hardcoded default is a whole-command last resort**, not a
///    per-slot one, so a pure Rocket Pool node is not handed a "file not
///    found" for `/var/log/teku/teku.log`, a path it never had.
/// 3. **`select` filters afterwards.** It names nothing, so it cannot
///    interact with rule 1.
///
/// `cli.rs::needs_detection` is this function's precedence list negated by
/// hand, and since R12 the negation is per-slot: detection is skippable only
/// when *both* slots are already answered. The bn rungs alone no longer
/// suppress the spawn, because `docker ps` now also answers `detected_vc` -
/// a host with `logs_file` configured and a Rocket Pool validator container
/// running needs that spawn to find its second stream at all. So a rung
/// added above `detected_bn`/`detected_vc` means a clause added there, and a
/// rung that only answers one slot must not gate the whole condition. Get
/// the first wrong and a configured operator pays for a `docker ps` spawn
/// whose answer cannot be used; get the second wrong and a separated
/// deployment silently keeps printing one stream.
pub fn resolve_log_sources(inputs: SourceInputs) -> LogSources {
    let bn_flags = [
        inputs.container_flag.map(LogTarget::Container),
        inputs.path.map(LogTarget::File),
    ];
    let bn_env = [
        inputs.container_env.map(LogTarget::Container),
        inputs
            .logs_file_env
            .map(|p| LogTarget::File(PathBuf::from(p))),
    ];
    let bn_cfg = [
        inputs.container_cfg.map(LogTarget::Container),
        inputs.logs_file_cfg.map(LogTarget::File),
    ];
    // Rule 1's gate: flag or environment variable only, never config.
    let bn_explicit = first_target(bn_flags.clone(), bn_env.clone(), [None, None]);
    let bn_stated = first_target(bn_flags, bn_env, bn_cfg);

    let vc_flags = [
        inputs.vc_container_flag.map(LogTarget::Container),
        inputs.vc_logs_file_flag.map(LogTarget::File),
    ];
    let vc_env = [
        inputs.vc_container_env.map(LogTarget::Container),
        inputs
            .vc_logs_file_env
            .map(|p| LogTarget::File(PathBuf::from(p))),
    ];
    let vc_cfg = [
        inputs.vc_container_cfg.map(LogTarget::Container),
        inputs.vc_logs_file_cfg.map(LogTarget::File),
    ];
    let vc_explicit = first_target(vc_flags.clone(), vc_env.clone(), [None, None]);
    let vc_stated = first_target(vc_flags, vc_env, vc_cfg);

    // Rule 1: an unstated side stays silent when the other was stated by
    // flag or environment variable. Config and detection free-mix on both
    // sides otherwise, which is why the fallthrough arm below still resolves
    // both independently through the full ladder.
    let (mut bn, mut vc) = match (bn_explicit.is_some(), vc_explicit.is_some()) {
        (true, false) => (bn_stated, None),
        (false, true) => (None, vc_stated),
        _ => (
            bn_stated.or_else(|| inputs.detected_bn.map(LogTarget::Container)),
            vc_stated.or_else(|| inputs.detected_vc.map(LogTarget::Container)),
        ),
    };

    // Rule 2: the hardcoded path answers for the whole command or not at all.
    if bn.is_none() && vc.is_none() {
        bn = Some(LogTarget::File(PathBuf::from(DEFAULT_TEKU_LOG)));
    }

    // Rule 3.
    match inputs.select {
        Some(Source::Bn) => vc = None,
        Some(Source::Vc) => bn = None,
        None => {}
    }

    LogSources { bn, vc }
}

/// The first target present, tier by tier, container before file within a
/// tier.
fn first_target(
    flags: [Option<LogTarget>; 2],
    env: [Option<LogTarget>; 2],
    cfg: [Option<LogTarget>; 2],
) -> Option<LogTarget> {
    flags.into_iter().chain(env).chain(cfg).flatten().next()
}

/// Whether a producer follows the log forever or stops at the end of it.
///
/// `Follow` is what `tekops logs` needs, and `run_logs`'s whole
/// process-lifetime design rests on it: the producer never reaches EOF, so the
/// streaming loop cannot be what ends the session, which is why the main
/// thread blocks on the pager instead. `Once` is what `tekops dump-logs`
/// needs and has the opposite property - the process ends by itself, so none
/// of the pager, signal-handler or temp-file machinery applies to it at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Follow,
    Once,
}

/// The command that produces raw log lines for one session.
///
/// Returned as data rather than a built `Command` so the argv shape is
/// testable, which matters because of the shell on the container path.
///
/// In `Mode::Follow`, both producers share the property the whole
/// process-lifetime design in `run_logs` depends on: neither ever reaches EOF
/// on its own, so the streaming loop cannot be what ends the session. In
/// `Mode::Once` that is deliberately inverted - both end at EOF, which is what
/// lets `dump.rs` read them to completion with no pager to supervise.
///
/// The container path goes through `sh` for exactly one reason: `docker logs`
/// writes the container's stderr to *its* stderr, and an uncaptured stderr
/// would paint over the live `less`. Merging with `2>&1` in the shell keeps
/// `stream_logs` as it is - one reader, one writer, one byte cap - where
/// capturing both streams in Rust would need either two threads sharing the
/// writer behind a mutex, which splits the cap across them, or a `pre_exec`
/// `dup2`.
///
/// The line count and container name are passed as arguments *after* the `sh`
/// argv[0] placeholder and referenced positionally as `"$1"` and `"$2"`. They
/// are never interpolated into the script text: a container name is
/// autodetected or operator-supplied, and interpolating it would turn it into
/// shell code. `exec` keeps the process count the same as the `tail` path, so
/// the kill in `supervise_pager` still reaches the process that matters.
///
/// `mode` decides only whether the follow flag is present. Everything else,
/// and in particular the container arm's positional-argument shape, is
/// identical between the two - which is the reason this is one function with a
/// mode rather than two functions.
pub fn producer_argv(target: &LogTarget, lines: u32, mode: Mode) -> (String, Vec<String>) {
    match target {
        LogTarget::File(path) => {
            let mut argv = Vec::new();
            if mode == Mode::Follow {
                argv.push("-F".to_string());
            }
            argv.push("-n".to_string());
            argv.push(lines.to_string());
            argv.push(path.display().to_string());
            ("tail".to_string(), argv)
        }
        LogTarget::Container(name) => {
            let script = match mode {
                Mode::Follow => "exec docker logs -f --tail \"$1\" \"$2\" 2>&1",
                Mode::Once => "exec docker logs --tail \"$1\" \"$2\" 2>&1",
            };
            (
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    script.to_string(),
                    "sh".to_string(),
                    lines.to_string(),
                    name.clone(),
                ],
            )
        }
    }
}

/// Interim: Task 6 rewrites this to drive both slots through `merge::Merger`.
/// Until then it keeps the tree compiling and green by taking only the `bn`
/// slot from an already-resolved `LogSources` - the `unwrap_or_else` fallback
/// exists because an empty `bn` cannot happen in practice for a caller going
/// through `resolve_log_sources` (with no vc inputs supplied, rule 2's default
/// always fills it), so there is no panic path standing in for a case that can
/// come up here.
pub fn run_logs(sources: LogSources, lines: u32) -> ExitCode {
    let target = sources
        .bn
        .unwrap_or_else(|| LogTarget::File(PathBuf::from(DEFAULT_TEKU_LOG)));

    // Only a file can be checked for existence up front. A container's absence
    // surfaces as `docker logs` exiting non-zero, which reaches the operator
    // through the pager's own teardown.
    if let LogTarget::File(ref p) = target {
        if !p.exists() {
            eprintln!("error: log file not found: {}", p.display());
            return ExitCode::FAILURE;
        }
    }

    // The terminal delivers Ctrl+C to the whole foreground process group. `less`
    // is designed to catch it and drop out of follow mode, but tekops itself has
    // no handler and would otherwise die right along with it, tearing down the
    // pager it's supervising. Ignoring it here lets tekops outlive the keypress;
    // The producer is put in its own process group so the same Ctrl+C doesn't
    // kill it too, which would otherwise permanently break `less`'s `F` (resume
    // follow).
    //
    // The `termination` feature extends this same ignore to SIGTERM/SIGHUP (e.g.
    // a dropped SSH session). Unlike SIGINT, `less` has no special handling for
    // those and just dies normally, so `pager.wait()` below still returns and
    // the existing cleanup (kill the producer, let the temp file's Drop run)
    // still executes. Without this, tekops and `less` would die immediately
    // alongside the signal, skipping that cleanup entirely: the producer
    // (isolated into its own process group above, specifically so Ctrl+C can't
    // reach it) would be orphaned and keep running forever, and the temp file
    // backing `less` would never be removed - a real leak, one per dropped
    // session, not hypothetical. Failing to install this is not cosmetic: it
    // silently reverts the process to the exact behaviour the handler exists to
    // prevent - Ctrl+C kills tekops mid-session, leaving the producer
    // (deliberately in its own process group, out of the signal's reach)
    // orphaned forever and the temp file below undeleted. Swallowing the error
    // hides a guaranteed leak, so say so and bail rather than starting a session
    // that can't clean up after itself.
    if let Err(e) = ctrlc::set_handler(|| {}) {
        eprintln!("error: could not install signal handler: {e}");
        eprintln!("refusing to start: Ctrl+C or a dropped session would orphan the log producer");
        return ExitCode::FAILURE;
    }

    let (prog, args) = producer_argv(&target, lines, Mode::Follow);
    let mut producer = match Command::new(&prog)
        .args(&args)
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("error: failed to spawn {prog}: {e}");
            if let LogTarget::Container(_) = target {
                eprintln!("reading container logs needs the docker CLI on $PATH");
            }
            return ExitCode::FAILURE;
        }
    };

    // `less` is fed through a real temp file rather than piped directly into its
    // stdin. A pipe has no knowable end short of reading more of it, so a search
    // for text that isn't in the buffer yet leaves `less` unable to tell "not
    // found" from "not yet written" - it blocks waiting for more input rather
    // than reporting no match, indistinguishable from a hang. A real file has a
    // stat()-able size, so `less` can tell those two cases apart and searches
    // that miss return immediately instead of freezing the pager.
    let sink = match tempfile::Builder::new().prefix("tekops-logs-").tempfile() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: failed to create temp file for log output: {e}");
            let _ = producer.kill();
            return ExitCode::FAILURE;
        }
    };

    let mut pager = match Command::new("less")
        .args(["-R", "+F"])
        .arg(sink.path())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("error: failed to spawn less: {e}");
            let _ = producer.kill();
            return ExitCode::FAILURE;
        }
    };

    // Both of these can fail for real (fd exhaustion, /tmp remounted read-only)
    // and both happen with the pager already on screen, so a panic here would
    // dump a Rust backtrace over a live `less` and skip the cleanup below.
    //
    // On the container path, a container that isn't running makes `docker logs`
    // write an error to stderr and exit non-zero. Because `producer_argv` merged
    // that stderr into this same stdout stream with `2>&1`, it arrives here as
    // just another line, flows through `stream_logs` and `format_log_line` like
    // any other, and is sanitized by the passthrough branch rather than reaching
    // the terminal raw. That is the whole error-reporting path for a missing
    // container - there is no separate one, and there should not be one added.
    let Some(stdout) = producer.stdout.take() else {
        eprintln!("error: log producer stdout was not piped");
        let _ = pager.kill();
        let _ = producer.kill();
        return ExitCode::FAILURE;
    };
    let writer = match sink.reopen() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: failed to open temp file for writing: {e}");
            let _ = pager.kill();
            let _ = producer.kill();
            return ExitCode::FAILURE;
        }
    };
    match supervise_pager(
        pager,
        producer,
        BufReader::new(stdout),
        LineWriter::new(writer),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error while streaming logs: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the streaming loop against `reader` while the pager owns process
/// lifetime, then tears the producer down.
///
/// The ordering here is the whole point, and it has regressed before. Neither
/// `tail -F` nor `docker logs -f` reaches EOF on its own, so the streaming loop
/// cannot be what ends the process - blocking the main thread on it hangs until
/// the log happens to move, which on a quiet node (or a quiet container) is
/// forever. So: stream on a worker thread, block the main thread on the pager,
/// and only once the pager has exited kill the producer. Killing it is what
/// closes the pipe and gives the worker its EOF; joining before the kill
/// deadlocks instead.
fn supervise_pager(
    mut pager: Child,
    mut tail: Child,
    reader: impl BufRead + Send + 'static,
    mut writer: impl Write + Send + 'static,
) -> io::Result<()> {
    let streaming = thread::spawn(move || stream_logs(reader, &mut writer));

    let _ = pager.wait();
    let _ = tail.kill();
    let _ = tail.wait();

    match streaming.join() {
        Ok(result) => result,
        // The pager has already exited by this point, so the terminal is the
        // user's again and a plain error beats a propagated panic.
        Err(_) => Err(io::Error::other("log streaming thread panicked")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn streams_and_formats_each_line() {
        let input = "{\"@timestamp\":\"t\",\"level\":\"INFO\",\"thread\":\"t1\",\"class\":\"C\",\"message\":\"one\"}\n\
                      {\"@timestamp\":\"t\",\"level\":\"ERROR\",\"thread\":\"t1\",\"class\":\"C\",\"message\":\"two\"}\n";
        let reader = Cursor::new(input);
        let mut output = Vec::new();

        stream_logs(reader, &mut output).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("INFO [t1] C - one"));
        assert!(output.contains("ERROR [t1] C - two"));
        assert_eq!(output.lines().count(), 2);
    }

    #[test]
    fn passes_through_malformed_lines() {
        let reader = Cursor::new("garbage\n");
        let mut output = Vec::new();

        stream_logs(reader, &mut output).unwrap();

        assert_eq!(String::from_utf8(output).unwrap().trim_end(), "garbage");
    }

    fn log_line(message: &str) -> String {
        format!(
            "{{\"@timestamp\":\"t\",\"level\":\"INFO\",\"thread\":\"t1\",\"class\":\"C\",\"message\":\"{message}\"}}\n"
        )
    }

    #[test]
    fn stops_following_once_the_buffer_cap_is_reached() {
        let input: String = (0..500).map(|i| log_line(&format!("line{i}"))).collect();
        let mut output = Vec::new();

        stream_logs_capped(Cursor::new(input), &mut output, 200).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("buffer limit reached"),
            "no notice emitted: {output:?}"
        );
        assert!(output.contains("Quit and rerun to resume"));
        assert!(
            output.contains("line0"),
            "content before the cap should survive"
        );
        assert!(
            !output.contains("line499"),
            "content past the cap should be dropped"
        );
    }

    #[test]
    fn buffer_cap_bounds_what_is_written() {
        let input: String = (0..500).map(|i| log_line(&format!("line{i}"))).collect();
        let mut output = Vec::new();
        let cap = 1024;

        stream_logs_capped(Cursor::new(input), &mut output, cap).unwrap();

        // The log content itself stays under the cap; only the fixed-size
        // notice is allowed past it, so the bound stays meaningful.
        assert!(
            (output.len() as u64) < cap + 200,
            "wrote {} bytes for a {cap}-byte cap",
            output.len()
        );
    }

    #[test]
    fn a_stream_under_the_cap_is_untouched_and_has_no_notice() {
        let input = log_line("only line");
        let mut output = Vec::new();

        stream_logs_capped(Cursor::new(input), &mut output, MAX_BUFFER_BYTES).unwrap();

        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("only line"));
        assert!(
            !output.contains("buffer limit"),
            "notice on an under-cap stream: {output:?}"
        );
        assert_eq!(output.lines().count(), 1);
    }

    #[test]
    fn hitting_the_cap_is_not_an_error() {
        let input: String = (0..100).map(|i| log_line(&format!("line{i}"))).collect();
        let mut output = Vec::new();
        assert!(stream_logs_capped(Cursor::new(input), &mut output, 50).is_ok());
    }

    #[test]
    fn default_path_matches_existing_bashrc_function() {
        assert_eq!(DEFAULT_TEKU_LOG, "/var/log/teku/teku.log");
    }

    /// Stands in for `tail -F`: a child that never exits on its own and whose
    /// stdout pipe therefore never reaches EOF. Reading it blocks forever,
    /// which is precisely the condition that makes the ordering in
    /// `supervise_pager` load-bearing.
    fn never_ending_child() -> Child {
        Command::new("sleep")
            .arg("300")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sleep")
    }

    /// The documented regression: with `tail -F` never reaching EOF, quitting
    /// the pager on a quiet log must still end the session. If the wait/kill
    /// ordering is inverted this test hangs rather than fails, so it runs on a
    /// worker thread with a hard deadline.
    #[test]
    fn supervise_pager_returns_once_the_pager_exits_even_if_the_log_is_silent() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut tail = never_ending_child();
            let stdout = tail.stdout.take().expect("piped");
            // A pager that exits promptly, as if the user pressed `q`.
            let pager = Command::new("sleep")
                .arg("0.2")
                .spawn()
                .expect("spawn pager stand-in");
            let result = supervise_pager(pager, tail, BufReader::new(stdout), io::sink());
            let _ = done_tx.send(result.is_ok());
        });

        match done_rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(ok) => assert!(ok, "supervise_pager returned an error"),
            Err(_) => panic!(
                "supervise_pager did not return after the pager exited - \
                 the wait/kill/join ordering has regressed into a hang"
            ),
        }
    }

    /// The tailer must not outlive the session; leaking it was the original
    /// bug behind the process-group work.
    #[test]
    fn supervise_pager_reaps_the_tailer() {
        let mut tail = never_ending_child();
        let pid = tail.id();
        let stdout = tail.stdout.take().expect("piped");
        let pager = Command::new("sleep")
            .arg("0.2")
            .spawn()
            .expect("spawn pager stand-in");

        supervise_pager(pager, tail, BufReader::new(stdout), io::sink()).unwrap();

        // The child was killed and waited on, so it is fully reaped rather than
        // left as a zombie or still running.
        let still_alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .expect("kill -0")
            .success();
        assert!(!still_alive, "tailer pid {pid} survived the session");
    }

    /// The colorized bytes must actually reach the sink the pager reads, not
    /// just be produced and dropped.
    #[test]
    fn supervise_pager_streams_log_lines_into_the_sink() {
        let mut source = Command::new("printf")
            .arg(
                r#"{"@timestamp":"t","level":"INFO","thread":"m","class":"C","message":"hello"}\n"#,
            )
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn printf");
        let stdout = source.stdout.take().expect("piped");
        let pager = Command::new("sleep")
            .arg("0.3")
            .spawn()
            .expect("spawn pager stand-in");

        let sink = tempfile::Builder::new()
            .prefix("tekops-test-")
            .tempfile()
            .unwrap();
        let writer = LineWriter::new(sink.reopen().unwrap());
        supervise_pager(pager, source, BufReader::new(stdout), writer).unwrap();

        let written = std::fs::read_to_string(sink.path()).unwrap();
        assert!(written.contains("INFO [m] C - hello"), "got {written:?}");
    }

    #[test]
    fn run_logs_reports_a_missing_log_file_instead_of_spawning_anything() {
        let missing = std::env::temp_dir().join("tekops-definitely-not-here.log");
        assert!(!missing.exists(), "test precondition");
        let sources = LogSources {
            bn: Some(LogTarget::File(missing)),
            vc: None,
        };
        let code = run_logs(sources, 500);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::FAILURE));
    }

    fn inputs() -> SourceInputs {
        SourceInputs {
            path: None,
            container_flag: None,
            vc_container_flag: None,
            vc_logs_file_flag: None,
            container_env: None,
            logs_file_env: None,
            vc_container_env: None,
            vc_logs_file_env: None,
            container_cfg: None,
            logs_file_cfg: None,
            vc_container_cfg: None,
            vc_logs_file_cfg: None,
            detected_bn: None,
            detected_vc: None,
            select: None,
        }
    }

    /// The reported deployment: a Rocket Pool validator container detected
    /// alongside a beacon node that is not under Docker. Both sources, no flags.
    #[test]
    fn detection_alone_resolves_both_sources() {
        let got = resolve_log_sources(SourceInputs {
            detected_vc: Some("rocketpool_validator".into()),
            logs_file_cfg: Some(PathBuf::from("/var/log/teku/teku.log")),
            ..inputs()
        });
        assert_eq!(
            got.bn,
            Some(LogTarget::File(PathBuf::from("/var/log/teku/teku.log")))
        );
        assert_eq!(
            got.vc,
            Some(LogTarget::Container("rocketpool_validator".into()))
        );
    }

    /// The back-compatibility rule, and the most important test in this task.
    /// Naming one side and not the other means that one stream only - so every
    /// invocation that works today prints exactly what it prints today, even on a
    /// host where a validator container is sitting there detectable.
    #[test]
    fn naming_only_the_beacon_node_suppresses_a_detected_validator() {
        let got = resolve_log_sources(SourceInputs {
            container_flag: Some("mine".into()),
            detected_vc: Some("rocketpool_validator".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("mine".into())));
        assert_eq!(got.vc, None, "an unstated side stays silent");
    }

    #[test]
    fn naming_only_the_validator_suppresses_a_detected_beacon_node() {
        let got = resolve_log_sources(SourceInputs {
            vc_container_flag: Some("rocketpool_validator".into()),
            detected_bn: Some("eth-docker-consensus-1".into()),
            ..inputs()
        });
        assert_eq!(got.bn, None);
        assert_eq!(
            got.vc,
            Some(LogTarget::Container("rocketpool_validator".into()))
        );
    }

    /// The asymmetric half of R12's boundary: a flag on one side silences a
    /// *configured* source on the other, not just a detected one. This is
    /// deliberate, not an oversight - rule 1's gate is `bn_explicit`/
    /// `vc_explicit` (flag/env only), and `vc_container_cfg` here never
    /// reaches that gate, so `vc_stated` (which does see it) is discarded
    /// along with it. Do not "fix" this by widening the gate to config; that
    /// is exactly the case `detection_alone_resolves_both_sources` above
    /// pins the other way.
    #[test]
    fn a_flag_on_one_side_suppresses_a_configured_other_side() {
        let got = resolve_log_sources(SourceInputs {
            container_flag: Some("X".into()),
            vc_container_cfg: Some("Y".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("X".into())));
        assert_eq!(got.vc, None);
    }

    #[test]
    fn naming_both_sides_gives_both() {
        let got = resolve_log_sources(SourceInputs {
            container_flag: Some("bn-c".into()),
            vc_container_flag: Some("vc-c".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("bn-c".into())));
        assert_eq!(got.vc, Some(LogTarget::Container("vc-c".into())));
    }

    /// The hardcoded default is a last resort for the whole command, not for the
    /// beacon node's slot. A pure Rocket Pool node must not be handed a "file not
    /// found" for a path it never had.
    #[test]
    fn the_default_path_applies_only_when_neither_slot_resolved() {
        let nothing = resolve_log_sources(inputs());
        assert_eq!(
            nothing.bn,
            Some(LogTarget::File(PathBuf::from(DEFAULT_TEKU_LOG)))
        );
        assert_eq!(nothing.vc, None);

        let vc_only = resolve_log_sources(SourceInputs {
            detected_vc: Some("rocketpool_validator".into()),
            ..inputs()
        });
        assert_eq!(
            vc_only.bn, None,
            "no spurious default on a validator-only host"
        );
    }

    /// The VC ladder has the same shape as the BN's: flag > env > config, and
    /// naming a container beats naming a file within a tier.
    #[test]
    fn the_validator_ladder_matches_the_beacon_nodes_shape() {
        let flag_wins = resolve_log_sources(SourceInputs {
            vc_container_flag: Some("flag".into()),
            vc_container_env: Some("env".into()),
            vc_container_cfg: Some("cfg".into()),
            ..inputs()
        });
        assert_eq!(flag_wins.vc, Some(LogTarget::Container("flag".into())));

        let env_wins = resolve_log_sources(SourceInputs {
            vc_container_env: Some("env".into()),
            vc_container_cfg: Some("cfg".into()),
            ..inputs()
        });
        assert_eq!(env_wins.vc, Some(LogTarget::Container("env".into())));

        let container_beats_file = resolve_log_sources(SourceInputs {
            vc_container_cfg: Some("cfg".into()),
            vc_logs_file_cfg: Some(PathBuf::from("/cfg/validator.log")),
            ..inputs()
        });
        assert_eq!(
            container_beats_file.vc,
            Some(LogTarget::Container("cfg".into()))
        );
    }

    /// `--vc` filters what was resolved; it does not name anything.
    #[test]
    fn selecting_one_slot_drops_the_other() {
        let got = resolve_log_sources(SourceInputs {
            detected_bn: Some("eth-docker-consensus-1".into()),
            detected_vc: Some("eth-docker-validator-1".into()),
            select: Some(Source::Vc),
            ..inputs()
        });
        assert_eq!(got.bn, None);
        assert_eq!(
            got.vc,
            Some(LogTarget::Container("eth-docker-validator-1".into()))
        );
    }

    #[test]
    fn active_lists_the_sources_that_resolved() {
        let both = resolve_log_sources(SourceInputs {
            detected_bn: Some("c".into()),
            detected_vc: Some("v".into()),
            ..inputs()
        });
        assert_eq!(both.active(), vec![Source::Bn, Source::Vc]);

        let one = resolve_log_sources(SourceInputs {
            container_flag: Some("c".into()),
            ..inputs()
        });
        assert_eq!(one.active(), vec![Source::Bn]);
    }

    /// The two config rungs sit below both environment variables.
    #[test]
    fn the_logs_file_env_beats_the_config_container() {
        let got = resolve_log_sources(SourceInputs {
            logs_file_env: Some("/var/log/teku/teku.log".into()),
            container_cfg: Some("cfg-container".into()),
            logs_file_cfg: Some(PathBuf::from("/cfg/teku.log")),
            ..inputs()
        });
        assert_eq!(
            got.bn,
            Some(LogTarget::File(PathBuf::from("/var/log/teku/teku.log")))
        );
    }

    #[test]
    fn the_container_env_beats_both_config_rungs() {
        let got = resolve_log_sources(SourceInputs {
            container_env: Some("env-container".into()),
            container_cfg: Some("cfg-container".into()),
            logs_file_cfg: Some(PathBuf::from("/cfg/teku.log")),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("env-container".into())));
    }

    /// Within the config tier, naming a container is the more specific statement,
    /// mirroring why $TEKOPS_CONTAINER beats $TEKOPS_LOGS_FILE.
    #[test]
    fn the_config_container_beats_the_config_logs_file() {
        let got = resolve_log_sources(SourceInputs {
            container_cfg: Some("cfg-container".into()),
            logs_file_cfg: Some(PathBuf::from("/cfg/teku.log")),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("cfg-container".into())));
    }

    /// Config beats detection: a value the operator wrote down beats one tekops
    /// guessed.
    #[test]
    fn the_config_logs_file_beats_detection() {
        let got = resolve_log_sources(SourceInputs {
            logs_file_cfg: Some(PathBuf::from("/cfg/teku.log")),
            detected_bn: Some("detected-container".into()),
            ..inputs()
        });
        assert_eq!(
            got.bn,
            Some(LogTarget::File(PathBuf::from("/cfg/teku.log")))
        );
    }

    #[test]
    fn the_config_container_beats_detection() {
        let got = resolve_log_sources(SourceInputs {
            container_cfg: Some("cfg-container".into()),
            detected_bn: Some("detected-container".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("cfg-container".into())));
    }

    /// Detection still beats the hardcoded default when the config says nothing.
    #[test]
    fn detection_still_beats_the_default_with_an_empty_config() {
        let got = resolve_log_sources(SourceInputs {
            detected_bn: Some("d".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("d".into())));
    }

    /// The positional path outranks everything in the config, so naming a file on
    /// a configured host still reads that file.
    #[test]
    fn the_positional_path_beats_both_config_rungs() {
        let got = resolve_log_sources(SourceInputs {
            path: Some(PathBuf::from("/tmp/x.log")),
            container_cfg: Some("cfg-container".into()),
            logs_file_cfg: Some(PathBuf::from("/cfg/teku.log")),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::File(PathBuf::from("/tmp/x.log"))));
    }

    #[test]
    fn container_flag_wins_over_everything_below_it() {
        let got = resolve_log_sources(SourceInputs {
            container_flag: Some("mine".into()),
            container_env: Some("env".into()),
            logs_file_env: Some("/x.log".into()),
            detected_bn: Some("det".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("mine".into())));
    }

    /// Called directly with the config rungs populated, so a future edit that
    /// moved the config rungs above the flag check would fail this test.
    #[test]
    fn container_flag_beats_both_config_rungs() {
        let got = resolve_log_sources(SourceInputs {
            container_flag: Some("mine".into()),
            container_cfg: Some("cfg-container".into()),
            logs_file_cfg: Some(PathBuf::from("/cfg/teku.log")),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("mine".into())));
    }

    /// The case that pins the ordering: naming a file on a host that also runs
    /// Docker must read that file. An earlier draft put detection above the
    /// positional path and would have tailed a container instead.
    #[test]
    fn explicit_path_beats_a_successful_detection() {
        let got = resolve_log_sources(SourceInputs {
            path: Some(PathBuf::from("/var/log/teku/teku.log")),
            detected_bn: Some("det".into()),
            ..inputs()
        });
        assert_eq!(
            got.bn,
            Some(LogTarget::File(PathBuf::from("/var/log/teku/teku.log")))
        );
    }

    #[test]
    fn container_env_beats_logs_file_env_and_detection() {
        let got = resolve_log_sources(SourceInputs {
            container_env: Some("env".into()),
            logs_file_env: Some("/x.log".into()),
            detected_bn: Some("det".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("env".into())));
    }

    #[test]
    fn logs_file_env_beats_detection() {
        let got = resolve_log_sources(SourceInputs {
            logs_file_env: Some("/x.log".into()),
            detected_bn: Some("det".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::File(PathBuf::from("/x.log"))));
    }

    /// Detection sits above the hardcoded default because on a Docker host that
    /// default path does not exist, and a detected container is a far better
    /// answer than a guaranteed "file not found".
    #[test]
    fn detection_beats_the_hardcoded_default_path() {
        let got = resolve_log_sources(SourceInputs {
            detected_bn: Some("rocketpool_eth2".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("rocketpool_eth2".into())));
    }

    #[test]
    fn falls_back_to_the_default_path_when_nothing_is_known() {
        let got = resolve_log_sources(inputs());
        assert_eq!(
            got.bn,
            Some(LogTarget::File(PathBuf::from(DEFAULT_TEKU_LOG)))
        );
    }

    /// `$TEKOPS_LOGS_FILE` now applies unconditionally - there is no source to
    /// gate it on any more.
    #[test]
    fn teku_logs_file_env_is_honoured() {
        let got = resolve_log_sources(SourceInputs {
            logs_file_env: Some("/x.log".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::File(PathBuf::from("/x.log"))));
    }

    /// A custom stack tekops does not recognize is a supported deployment: the
    /// container override must work with detection having found nothing.
    #[test]
    fn custom_container_works_with_no_detection_at_all() {
        let got = resolve_log_sources(SourceInputs {
            container_flag: Some("mynode-teku".into()),
            ..inputs()
        });
        assert_eq!(got.bn, Some(LogTarget::Container("mynode-teku".into())));
    }

    #[test]
    fn file_targets_still_spawn_tail_exactly_as_before() {
        let (prog, args) =
            producer_argv(&LogTarget::File(PathBuf::from("/a.log")), 500, Mode::Follow);
        assert_eq!(prog, "tail");
        assert_eq!(args, vec!["-F", "-n", "500", "/a.log"]);
    }

    #[test]
    fn container_targets_spawn_docker_logs_with_stderr_merged() {
        let (prog, args) = producer_argv(
            &LogTarget::Container("rocketpool_eth2".into()),
            500,
            Mode::Follow,
        );
        assert_eq!(prog, "sh");
        assert_eq!(args[0], "-c");
        assert!(
            args[1].contains("docker logs -f --tail"),
            "got: {}",
            args[1]
        );
        assert!(
            args[1].contains("2>&1"),
            "stderr must be merged: {}",
            args[1]
        );
        assert!(args[1].starts_with("exec "), "got: {}", args[1]);
        assert_eq!(args[2], "sh");
        assert_eq!(args[3], "500");
        assert_eq!(args[4], "rocketpool_eth2");
    }

    /// The injection guard. A container name is autodetected or operator
    /// supplied, and interpolating it into the script text would make it shell
    /// code. It must arrive as its own argv element, with the script referring
    /// to it only positionally.
    #[test]
    fn a_hostile_container_name_stays_data_not_code() {
        let hostile = "x\"; touch /tmp/pwned; echo \"";
        let (_, args) = producer_argv(&LogTarget::Container(hostile.into()), 500, Mode::Follow);
        assert!(
            !args[1].contains("pwned"),
            "container name leaked into the script: {}",
            args[1]
        );
        assert_eq!(
            args[4], hostile,
            "name should arrive verbatim as its own arg"
        );
    }

    /// `-n 0` means "follow only new output" on the file path, and `--tail 0`
    /// is docker's spelling of the same thing. The two must agree.
    #[test]
    fn zero_lines_is_passed_through_on_both_paths() {
        let (_, file) = producer_argv(&LogTarget::File(PathBuf::from("/a.log")), 0, Mode::Follow);
        assert_eq!(file[2], "0");
        let (_, container) = producer_argv(&LogTarget::Container("c".into()), 0, Mode::Follow);
        assert_eq!(container[3], "0");
    }

    /// `dump-logs` needs a producer that ends on its own. Everything else
    /// about the argv must be identical to the follow case.
    #[test]
    fn once_mode_drops_the_follow_flag_for_a_file() {
        let (prog, args) =
            producer_argv(&LogTarget::File(PathBuf::from("/a.log")), 1000, Mode::Once);
        assert_eq!(prog, "tail");
        assert!(!args.contains(&"-F".to_string()));
        assert_eq!(args, vec!["-n", "1000", "/a.log"]);
    }

    /// The container arm's positional-argument shape is the injection guard,
    /// so it has to survive the mode change intact.
    #[test]
    fn once_mode_drops_the_follow_flag_for_a_container() {
        let (prog, args) = producer_argv(
            &LogTarget::Container("eth-docker-consensus-1".into()),
            1000,
            Mode::Once,
        );
        assert_eq!(prog, "sh");
        assert_eq!(args[1], "exec docker logs --tail \"$1\" \"$2\" 2>&1");
        assert!(!args[1].contains("-f"));
        assert!(args[1].contains("2>&1"));
        assert_eq!(args[2], "sh");
        assert_eq!(args[3], "1000");
        assert_eq!(args[4], "eth-docker-consensus-1");
    }
}
