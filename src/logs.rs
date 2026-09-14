use crate::logfmt::format_log_line;
use std::env;
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
const DEFAULT_TEKU_LOG: &str = "/var/log/teku/teku.log";

/// Where one `tekops logs` session reads from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogTarget {
    File(PathBuf),
    Container(String),
}

/// Resolves what `tekops logs` should read, given everything that can say so.
///
/// Precedence, highest first: the `--container` flag or positional path (clap
/// keeps those mutually exclusive, so there is no ordering question between
/// them), then `$TEKOPS_CONTAINER`, then `$TEKOPS_LOGS_FILE`, then `docker ps`
/// detection, then the hardcoded default.
///
/// The principle is: flags beat environment beats detection beats hardcoded
/// default. Detection sits below everything the operator stated and above the
/// hardcoded path, which is the only placement that behaves. Above the
/// positional path, `tekops logs /var/log/teku/teku.log` would tail a container
/// on any host that also runs Docker; below the hardcoded default, a Docker
/// host would always fail on a path that does not exist there.
///
/// Every input arrives as a parameter rather than being read here, so the whole
/// ladder is testable with no environment races and no Docker installed.
pub fn resolve_log_target(
    path: Option<PathBuf>,
    container_flag: Option<String>,
    container_env: Option<String>,
    teku_logs_file_env: Option<String>,
    detected: Option<String>,
) -> LogTarget {
    if let Some(c) = container_flag {
        return LogTarget::Container(c);
    }
    if let Some(p) = path {
        return LogTarget::File(p);
    }
    if let Some(c) = container_env {
        return LogTarget::Container(c);
    }
    if let Some(p) = teku_logs_file_env {
        return LogTarget::File(PathBuf::from(p));
    }
    if let Some(c) = detected {
        return LogTarget::Container(c);
    }
    LogTarget::File(PathBuf::from(DEFAULT_TEKU_LOG))
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

pub fn run_logs(
    path: Option<PathBuf>,
    lines: u32,
    container: Option<String>,
    detected: Option<String>,
) -> ExitCode {
    let target = resolve_log_target(
        path,
        container,
        env::var("TEKOPS_CONTAINER").ok(),
        env::var("TEKOPS_LOGS_FILE").ok(),
        detected,
    );

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
        let code = run_logs(Some(missing), 500, None, None);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::FAILURE));
    }

    fn target(
        path: Option<&str>,
        container_flag: Option<&str>,
        container_env: Option<&str>,
        logs_file_env: Option<&str>,
        detected: Option<&str>,
    ) -> LogTarget {
        resolve_log_target(
            path.map(PathBuf::from),
            container_flag.map(String::from),
            container_env.map(String::from),
            logs_file_env.map(String::from),
            detected.map(String::from),
        )
    }

    #[test]
    fn container_flag_wins_over_everything_below_it() {
        let t = target(None, Some("mine"), Some("env"), Some("/x.log"), Some("det"));
        assert_eq!(t, LogTarget::Container("mine".into()));
    }

    /// The case that pins the ordering: naming a file on a host that also runs
    /// Docker must read that file. An earlier draft put detection above the
    /// positional path and would have tailed a container instead.
    #[test]
    fn explicit_path_beats_a_successful_detection() {
        let t = target(
            Some("/var/log/teku/teku.log"),
            None,
            None,
            None,
            Some("det"),
        );
        assert_eq!(t, LogTarget::File(PathBuf::from("/var/log/teku/teku.log")));
    }

    #[test]
    fn container_env_beats_logs_file_env_and_detection() {
        let t = target(None, None, Some("env"), Some("/x.log"), Some("det"));
        assert_eq!(t, LogTarget::Container("env".into()));
    }

    #[test]
    fn logs_file_env_beats_detection() {
        let t = target(None, None, None, Some("/x.log"), Some("det"));
        assert_eq!(t, LogTarget::File(PathBuf::from("/x.log")));
    }

    /// Detection sits above the hardcoded default because on a Docker host that
    /// default path does not exist, and a detected container is a far better
    /// answer than a guaranteed "file not found".
    #[test]
    fn detection_beats_the_hardcoded_default_path() {
        let t = target(None, None, None, None, Some("rocketpool_eth2"));
        assert_eq!(t, LogTarget::Container("rocketpool_eth2".into()));
    }

    #[test]
    fn falls_back_to_the_default_path_when_nothing_is_known() {
        assert_eq!(
            target(None, None, None, None, None),
            LogTarget::File(PathBuf::from(DEFAULT_TEKU_LOG))
        );
    }

    /// `$TEKOPS_LOGS_FILE` now applies unconditionally - there is no source to
    /// gate it on any more.
    #[test]
    fn teku_logs_file_env_is_honoured() {
        let t = target(None, None, None, Some("/x.log"), None);
        assert_eq!(t, LogTarget::File(PathBuf::from("/x.log")));
    }

    /// A custom stack tekops does not recognize is a supported deployment: the
    /// container override must work with detection having found nothing.
    #[test]
    fn custom_container_works_with_no_detection_at_all() {
        let t = target(None, Some("mynode-teku"), None, None, None);
        assert_eq!(t, LogTarget::Container("mynode-teku".into()));
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
