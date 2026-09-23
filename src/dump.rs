//! `tekops dump-logs`: a shareable, anonymised snapshot of the node's log.
//!
//! Split the way `logfmt.rs` and `host.rs` are: `render_dump` and the date
//! helpers are pure and hold every formatting decision, and `run_dump` holds
//! all the I/O.

use crate::logs::{producer_argv, slot_target, wall_clock_ms, Lines, LogSources, LogTarget, Mode};
use crate::merge::{Merger, Source, MERGE_WINDOW};
use crate::redact::Redactor;
use crate::stack::Stack;
use crate::term::sanitize;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

/// A ceiling on how much a single dump will hold in memory.
///
/// With a line count, `tail -n` and `docker logs --tail` already bound the
/// read, so this only matters for a log whose individual lines are enormous - a
/// stack-trace storm, or a line carrying an embedded payload. With `-n all`
/// nothing else bounds it, and this is what stops a months-old log file being
/// read whole into memory. Sixteen mebibytes is far above any honest 1000-line
/// dump and far below anything that would trouble the node.
pub const MAX_DUMP_BYTES: usize = 16 * 1024 * 1024;

/// GitHub truncates large gist files in the web view, and a silently
/// truncated dump is worse than a refusal.
pub const MAX_GIST_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub enum DumpError {
    LogFileMissing(PathBuf),
    ProducerMissing(String),
    ProducerFailed { status: String, stderr: String },
    Gist(crate::gist::GistError),
    TooLarge { bytes: usize, limit: usize },
    Io(String),
}

impl fmt::Display for DumpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DumpError::LogFileMissing(p) => write!(f, "log file not found: {}", p.display()),
            DumpError::ProducerMissing(p) => write!(f, "could not run `{p}`: not found"),
            DumpError::ProducerFailed { status, stderr } => {
                write!(f, "reading the log failed ({status}): {stderr}")
            }
            DumpError::Gist(e) => write!(f, "{e}"),
            DumpError::TooLarge { bytes, limit } => write!(
                f,
                "the dump is {} KB, over the {} KB gist limit\n\
                 hint: lower -n, or drop --gist and use -o to write a local file",
                bytes / 1024,
                limit / 1024
            ),
            DumpError::Io(m) => write!(f, "{m}"),
        }
    }
}

impl From<crate::gist::GistError> for DumpError {
    fn from(e: crate::gist::GistError) -> Self {
        DumpError::Gist(e)
    }
}

/// The provenance block, rendered above the logs when `--header` is passed.
///
/// `redacted` arrives as already-formatted text rather than a count, because
/// the header is rendered *last* - it reports the run's final total, which is
/// not known until every line has been through the redactor.
pub struct Header {
    pub version: String,
    pub generated: String,
    pub stack: String,
    pub source: String,
    pub lines: Lines,
    pub redacted: String,
}

/// Civil date from a count of days since the Unix epoch.
///
/// Howard Hinnant's `civil_from_days`, the standard algorithm, exact for every
/// date this will ever be handed. `doctor.rs` declined to do calendar maths for
/// a container uptime because `restart_count` already carried the signal it
/// needed; here a human-readable stamp *is* the field, and twenty lines is
/// still a better trade than a `chrono` dependency in a binary that fought to
/// halve itself.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The inverse of `civil_from_days`: a civil date to days since the Unix
/// epoch. Howard Hinnant's algorithm, the same one `civil_from_days` above
/// is taken from, so the two round-trip by construction.
///
/// Lives here rather than in `logfmt.rs`, which is its only caller, so the
/// pair stays together - splitting them is how one of them gets "fixed"
/// without the other.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn split_utc(secs: u64) -> (i64, u32, u32, u64, u64, u64) {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// RFC3339 in UTC, for the header's `generated` field.
pub fn format_utc(secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = split_utc(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// The default output filename: sortable, and with no colons, which are
/// awkward in filenames on more than one platform.
pub fn default_filename(secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = split_utc(secs);
    format!("tekops-dump-{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z.txt")
}

/// Assembles the artifact.
///
/// The `--- logs ---` marker appears only when something is rendered above it,
/// so the no-flag output stays byte-identical to the raw log and is still
/// pipeable and diffable. That is the whole reason `--header` is opt-in.
pub fn render_dump(header: Option<&Header>, doctor: Option<&str>, lines: &[String]) -> String {
    let mut out = String::new();
    if let Some(h) = header {
        out.push_str("=== tekops dump-logs ===\n");
        out.push_str(&format!("tekops:    {}\n", h.version));
        out.push_str(&format!("generated: {}\n", h.generated));
        out.push_str(&format!("stack:     {}\n", h.stack));
        out.push_str(&format!("source:    {}\n", h.source));
        out.push_str(&format!("lines:     {}\n", h.lines));
        out.push_str(&format!("redacted:  {}\n", h.redacted));
        out.push('\n');
    }
    if let Some(d) = doctor {
        out.push_str("--- doctor ---\n");
        out.push_str(d);
        if !d.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    if header.is_some() || doctor.is_some() {
        out.push_str("--- logs ---\n");
    }
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// One raw line to one dump line: sanitize, then redact.
///
/// The order is not interchangeable. Sanitizing first means a control byte
/// cannot sit inside a token and stop the scanner recognising it; redacting
/// first would leave the scanner classifying text that the terminal would
/// later render differently from what was matched.
fn redact_line(r: &mut Redactor, raw: &str) -> String {
    r.redact(&sanitize(raw))
}

/// Reads one source to completion, grouping its lines into merge records.
/// Returns whether it stopped early at `MAX_DUMP_BYTES`.
///
/// `Mode::Once` producers end by themselves, so this drains to EOF and needs
/// none of the pager, thread or signal machinery `run_logs` has. Reading the
/// sources one after another cannot deadlock for the same reason: a producer
/// whose pipe fills simply blocks until its turn comes.
fn read_source(
    source: Source,
    target: &LogTarget,
    lines: Lines,
    merger: &mut Merger,
) -> Result<bool, DumpError> {
    // Only a file can be checked up front; a container's absence surfaces as a
    // non-zero exit from `docker logs`. Same split as `run_logs`.
    if let LogTarget::File(p) = target {
        if !p.exists() {
            return Err(DumpError::LogFileMissing(p.clone()));
        }
    }

    let (prog, argv) = producer_argv(target, lines, Mode::Once);
    let mut child = Command::new(&prog)
        .args(&argv)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => DumpError::ProducerMissing(prog.clone()),
            _ => DumpError::Io(e.to_string()),
        })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| DumpError::Io("the producer had no stdout".to_string()))?;

    // The merger owns this source's record builder. `Mode::Once` reaches a
    // real EOF, so `eof` is what finishes the last record here - `flush_stale`
    // is for the follow-mode producers that never get one.
    let capped = read_capped(BufReader::new(stdout), MAX_DUMP_BYTES, |line| {
        merger.push_line(source, line, Instant::now(), wall_clock_ms());
    })?;
    merger.eof(source);

    // Stopping early leaves the producer blocked on a full pipe, and the kill
    // is what makes its non-zero exit expected rather than a failure.
    if capped {
        let _ = child.kill();
        let _ = child.wait();
        return Ok(true);
    }

    let status = child.wait().map_err(|e| DumpError::Io(e.to_string()))?;
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut stderr);
        }
        return Err(DumpError::ProducerFailed {
            status: status.to_string(),
            stderr: sanitize(stderr.trim()),
        });
    }
    Ok(false)
}

/// Hands `reader`'s lines to `push` until EOF, or until more than `cap` bytes
/// have gone by. Returns whether it stopped at the cap.
///
/// The output cap in `read_sources` bounds what the artifact holds, but only
/// after every record is already in memory. With `-n all` the producer is
/// bounded by nothing but the file's size, so the read itself has to stop.
fn read_capped(
    reader: impl BufRead,
    cap: usize,
    mut push: impl FnMut(&str),
) -> Result<bool, DumpError> {
    let mut bytes: usize = 0;
    for line in reader.lines() {
        let line = line.map_err(|e| DumpError::Io(e.to_string()))?;
        bytes = bytes.saturating_add(line.len() + 1);
        if bytes > cap {
            return Ok(true);
        }
        push(&line);
    }
    Ok(false)
}

/// Reads every resolved source and returns one merged, tagged, redacted body.
///
/// A source that cannot be read does not abort the dump when the other one
/// works: the reason goes into the artifact as a visible note instead. A dump
/// exists to be handed to someone else, so silently omitting half of a
/// separated node would be worse than saying so. When nothing can be read at
/// all, the first failure is returned.
fn read_sources(
    sources: &LogSources,
    lines: Lines,
    r: &mut Redactor,
) -> Result<Vec<String>, DumpError> {
    let active = sources.active();
    // `drain_all` ignores the window and the EOF flags, so a `Mode::Once` read
    // needs neither; the instant is only there to satisfy the constructor. The
    // 0 puts an undated line at the top of the artifact rather than beside the
    // live ones, which is what `logs` wants and a dump does not.
    let mut merger = Merger::new(active.clone(), MERGE_WINDOW, Instant::now(), 0);

    let mut notes: Vec<String> = Vec::new();
    let mut read_any = false;
    let mut first_failure: Option<DumpError> = None;
    for source in &active {
        let Some(target) = slot_target(sources, *source) else {
            continue;
        };
        match read_source(*source, target, lines, &mut merger) {
            Ok(capped) => {
                read_any = true;
                if capped {
                    notes.push(format!(
                        "*** tekops: stopped reading the {} log at the {} MiB limit; \
                         the rest of it is not in this dump.",
                        source.tag(),
                        MAX_DUMP_BYTES / (1024 * 1024)
                    ));
                }
            }
            Err(e) => {
                notes.push(format!(
                    "*** tekops: could not read the {} log: {e}",
                    source.tag()
                ));
                if first_failure.is_none() {
                    first_failure = Some(e);
                }
            }
        }
    }
    if !read_any {
        return Err(first_failure.unwrap_or(DumpError::Io("no log source to read".to_string())));
    }

    // A dump is read by someone who was not there, so this matters more here
    // than on screen: an artifact whose two halves are stamped hours apart
    // looks like evidence about the node, and without the note there is
    // nothing to say the times are simply not comparable.
    if let Some(skew) = merger.take_skew_note() {
        notes.push(format!("*** tekops: {skew}"));
    }

    // The tag is prepended *after* redaction, which is what makes it safe by
    // construction rather than by luck: the redactor only ever sees the node's
    // own bytes, so it cannot rewrite the one column saying which process a
    // line came from.
    let tagged = active.len() > 1;
    let mut out = notes;
    let mut bytes: usize = out.iter().map(|n| n.len() + 1).sum();
    for record in merger.drain_all() {
        for line in &record.lines {
            let redacted = redact_line(r, line);
            let text = if tagged {
                format!("[{}] {redacted}", record.source.tag())
            } else {
                redacted
            };
            bytes = bytes.saturating_add(text.len() + 1);
            if bytes > MAX_DUMP_BYTES {
                out.push(format!(
                    "*** tekops: {} MiB limit reached, dump truncated here.",
                    MAX_DUMP_BYTES / (1024 * 1024)
                ));
                return Ok(out);
            }
            out.push(text);
        }
    }
    Ok(out)
}

/// A human label for the stack, for the header.
fn stack_label(stack: Option<Stack>) -> String {
    use clap::ValueEnum;
    stack
        .and_then(|s| s.to_possible_value())
        .map(|v| v.get_name().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// A human label for everything that was read, for the header.
///
/// One source keeps the bare label it has always had, so a single-source
/// dump's header is unchanged. Two get tagged, because a header naming only
/// one of them is indistinguishable from a dump of only one of them.
fn sources_label(sources: &LogSources) -> String {
    match (&sources.bn, &sources.vc) {
        (Some(bn), None) => target_label(bn),
        (None, Some(vc)) => target_label(vc),
        (Some(bn), Some(vc)) => format!("bn {}, vc {}", target_label(bn), target_label(vc)),
        (None, None) => "no source".to_string(),
    }
}

/// A human label for what was read, for the header.
fn target_label(target: &LogTarget) -> String {
    match target {
        LogTarget::File(p) => format!("file {}", p.display()),
        LogTarget::Container(c) => format!("container {c}"),
    }
}

/// Where the dump gets written, if anywhere.
#[derive(Debug, PartialEq, Eq)]
enum Destination {
    File(PathBuf),
    /// `-o -`: the dump alone on stdout, with no path line after it.
    Stdout,
    /// `--gist` with no `-o`: the gist is the artifact.
    Nowhere,
}

/// `-o` always wins. With no `-o`, `--gist` writes nothing and a plain run
/// writes the timestamped default. Pure, so the composition is testable without
/// an upload.
///
/// `-o -` with `--gist` is refused: `--gist` puts the URL alone on stdout so
/// it can be piped, and the whole dump in front of it would defeat that.
fn destination(output: &Option<PathBuf>, gist: bool, now: u64) -> Result<Destination, DumpError> {
    match (output, gist) {
        (Some(p), true) if p.as_os_str() == "-" => Err(DumpError::Io(
            "-o - and --gist both write to stdout; pick one, or give -o a file".to_string(),
        )),
        (Some(p), _) if p.as_os_str() == "-" => Ok(Destination::Stdout),
        (Some(p), _) => Ok(Destination::File(p.clone())),
        (None, false) => Ok(Destination::File(PathBuf::from(default_filename(now)))),
        (None, true) => Ok(Destination::Nowhere),
    }
}

fn check_gist_size(bytes: usize) -> Result<(), DumpError> {
    if bytes > MAX_GIST_BYTES {
        return Err(DumpError::TooLarge {
            bytes,
            limit: MAX_GIST_BYTES,
        });
    }
    Ok(())
}

/// `$GITHUB_TOKEN` then `$GH_TOKEN`, matching what `gh` itself reads.
fn read_token() -> Result<String, DumpError> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .map_err(|_| DumpError::Gist(crate::gist::GistError::MissingToken))
}

/// Writes the dump with owner-only permissions. It is a file of node logs
/// landing in whatever directory the operator happened to be standing in.
fn write_dump(path: &Path, content: &str) -> Result<(), DumpError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| DumpError::Io(format!("could not write {}: {e}", path.display())))?;
    f.write_all(content.as_bytes())
        .map_err(|e| DumpError::Io(format!("could not write {}: {e}", path.display())))
}

/// Writes the dump to stdout for `-o -`.
///
/// A reader that stops early (`| head`) is not an error: the dump is read,
/// redacted and written, and nobody wanting the rest is the reader's business.
fn write_stdout(content: &str) -> Result<(), DumpError> {
    let mut out = std::io::stdout().lock();
    match out.write_all(content.as_bytes()).and_then(|()| out.flush()) {
        Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
            Err(DumpError::Io(format!("could not write to stdout: {e}")))
        }
        _ => Ok(()),
    }
}

/// Shows what is about to leave the machine, and asks.
///
/// The prompt and the preview go to **stderr**, unlike `confirm_install` and
/// `confirm` in cli.rs which use stdout. This command's stdout carries the
/// gist URL alone so `tekops dump-logs --gist | pbcopy` works, and a prompt on
/// that stream would corrupt it.
///
/// This is the mitigation that matches the real risk. The redactor is a
/// hand-rolled scanner; a missed pattern is possible, and the thing standing
/// between a missed pattern and a leak is a human looking before the upload.
fn confirm_upload(rendered: &str) -> Result<bool, DumpError> {
    let total = rendered.lines().count();
    eprintln!(
        "about to upload {total} lines ({} KB) to a secret gist",
        rendered.len() / 1024
    );
    eprintln!();
    for l in rendered.lines().take(5) {
        eprintln!("  {l}");
    }
    if total > 5 {
        eprintln!("  ...");
    }
    eprintln!();
    eprint!("upload? [y/N] ");
    std::io::stderr()
        .flush()
        .map_err(|e| DumpError::Io(e.to_string()))?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|e| DumpError::Io(e.to_string()))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

pub struct DumpConfig {
    pub sources: LogSources,
    pub lines: Lines,
    pub output: Option<PathBuf>,
    pub gist: bool,
    pub yes: bool,
    pub header: bool,
    pub stack: Option<Stack>,
    pub doctor: Option<crate::doctor::ProbeConfig>,
    pub now: u64,
}

pub fn run_dump(cfg: DumpConfig) -> Result<(), DumpError> {
    // Checked before anything is read, so a missing token fails in
    // milliseconds rather than after a thousand-line read. Same principle as
    // `update.rs` running `probe_writable` before the download rather than
    // after it.
    let token = if cfg.gist { Some(read_token()?) } else { None };
    let dest = destination(&cfg.output, cfg.gist, cfg.now)?;

    let mut r = Redactor::new();

    // Header field values are redacted individually as they are gathered,
    // rather than by running the finished header through the redactor: the
    // header reports the run's redaction total, so redacting it afterwards
    // would let it invalidate the number it has already printed.
    let header_bits = cfg.header.then(|| {
        (
            redact_line(&mut r, &stack_label(cfg.stack)),
            redact_line(&mut r, &sources_label(&cfg.sources)),
        )
    });

    // A doctor failure never fails the dump. This command exists for the case
    // where the node is sick; aborting because the sick node did not answer
    // would be exactly backwards.
    let doctor_text = cfg.doctor.as_ref().map(|pc| {
        let facts = crate::doctor::probe(pc);
        let findings = crate::doctor::evaluate(&facts);
        crate::output::format_doctor_report(&facts, &findings)
            .lines()
            .map(|l| redact_line(&mut r, l))
            .collect::<Vec<_>>()
            .join("\n")
    });

    let lines = read_sources(&cfg.sources, cfg.lines, &mut r)?;

    let summary = r.summary();
    let header = header_bits.map(|(stack, source)| Header {
        version: env!("CARGO_PKG_VERSION").to_string(),
        generated: format_utc(cfg.now),
        stack,
        source,
        lines: cfg.lines,
        redacted: summary.to_string(),
    });

    let rendered = render_dump(header.as_ref(), doctor_text.as_deref(), &lines);

    match &dest {
        Destination::File(p) => write_dump(p, &rendered)?,
        Destination::Stdout => write_stdout(&rendered)?,
        Destination::Nowhere => {}
    }

    eprintln!("redacted {summary}");
    if let Destination::File(p) = &dest {
        println!("{}", p.display());
    }

    if let Some(token) = token {
        check_gist_size(rendered.len())?;
        // Declining is the mitigation doing its job, not a failure: the
        // operator looked at the preview and said no. `log-level` answers its
        // own prompt the same way - see `run_log_level`.
        if !cfg.yes && !confirm_upload(&rendered)? {
            eprintln!("aborted, nothing was uploaded");
            return Ok(());
        }
        let url = crate::gist::upload(&token, &default_filename(cfg.now), &rendered)?;
        println!("{url}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            version: "0.6.0".to_string(),
            generated: "2026-09-14T21:30:00Z".to_string(),
            stack: "eth-docker".to_string(),
            source: "container eth-docker-consensus-1".to_string(),
            lines: Lines::Last(1000),
            redacted: "47 values across 5 categories".to_string(),
        }
    }

    /// The no-flag default must be byte-identical to the raw log, so the file
    /// is still pipeable and diffable.
    #[test]
    fn with_no_header_and_no_doctor_the_output_is_only_the_lines() {
        let lines = vec!["one".to_string(), "two".to_string()];
        assert_eq!(render_dump(None, None, &lines), "one\ntwo\n");
    }

    #[test]
    fn the_header_is_rendered_above_a_logs_marker() {
        let out = render_dump(Some(&header()), None, &["one".to_string()]);
        assert!(out.starts_with("=== tekops dump-logs ===\n"));
        assert!(out.contains("tekops:    0.6.0\n"));
        assert!(out.contains("stack:     eth-docker\n"));
        assert!(out.contains("redacted:  47 values across 5 categories\n"));
        assert!(out.contains("--- logs ---\none\n"));
    }

    #[test]
    fn the_doctor_block_is_rendered_between_header_and_logs() {
        let out = render_dump(
            Some(&header()),
            Some("peers  WARN  22"),
            &["one".to_string()],
        );
        let doctor_at = out.find("--- doctor ---").expect("no doctor block");
        let logs_at = out.find("--- logs ---").expect("no logs block");
        assert!(doctor_at < logs_at);
        assert!(out.contains("peers  WARN  22\n"));
    }

    #[test]
    fn a_doctor_block_alone_still_gets_a_logs_marker() {
        let out = render_dump(None, Some("all good"), &["one".to_string()]);
        assert!(out.contains("--- doctor ---"));
        assert!(out.contains("--- logs ---\none\n"));
    }

    /// Known epochs, so these are checkable against `date -u -r <n>`.
    #[test]
    fn utc_formatting_matches_known_epochs() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn the_default_filename_is_sortable_and_has_no_colons() {
        let name = default_filename(1_700_000_000);
        assert_eq!(name, "tekops-dump-20231114T221320Z.txt");
        assert!(!name.contains(':'));
    }

    /// Sanitize must run *before* redaction, or a control byte hides inside a
    /// token and dodges classification. This pins the order at the place the
    /// order is actually decided.
    #[test]
    fn control_characters_are_stripped_before_classification() {
        let mut r = Redactor::new();
        let hostile = "peer \u{1b}[31m93.184.216.34\u{1b}[0m joined";
        let out = redact_line(&mut r, hostile);
        assert!(!out.contains('\u{1b}'));
        assert!(out.contains("<ip-1>"), "got: {out}");
    }

    #[test]
    fn a_rendered_dump_contains_no_escape_bytes() {
        let mut r = Redactor::new();
        let lines: Vec<String> = ["a\u{1b}[31mb", "c\u{7}d"]
            .iter()
            .map(|l| redact_line(&mut r, l))
            .collect();
        let out = render_dump(None, None, &lines);
        assert!(!out.contains('\u{1b}'));
        assert_eq!(out.matches('\u{7}').count(), 0);
    }

    #[test]
    fn reading_a_file_target_yields_its_last_lines_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("teku.log");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 0..10 {
            writeln!(f, "line {i} from 93.184.216.34").unwrap();
        }
        drop(f);

        let mut r = Redactor::new();
        let sources = LogSources {
            bn: Some(LogTarget::File(path)),
            vc: None,
        };
        let lines = read_sources(&sources, Lines::Last(3), &mut r).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("<ip-1>"));
        assert!(!lines[0].contains("93.184.216.34"));
        assert!(
            !lines[0].starts_with("[bn] "),
            "a single source carries no tag: {:?}",
            lines[0]
        );
    }

    /// `-n all` is how a whole log, from any client, gets anonymised. Order is
    /// the file's own, and a line with no timestamp stays where it was.
    #[test]
    fn reading_all_lines_yields_the_whole_file_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other-client.log");
        std::fs::write(
            &path,
            "Sep 24 10:00:01 node lighthouse: peer 93.184.216.34\n\
             no timestamp here\n\
             Sep 24 09:59:00 node an earlier stamp, later in the file\n",
        )
        .unwrap();

        let mut r = Redactor::new();
        let sources = LogSources {
            bn: Some(LogTarget::File(path)),
            vc: None,
        };
        let lines = read_sources(&sources, Lines::All, &mut r).unwrap();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("<ip-1>"), "{:?}", lines[0]);
        assert_eq!(lines[1], "no timestamp here");
        assert!(lines[2].contains("an earlier stamp"), "{:?}", lines[2]);
    }

    /// An endless reader is the proof the cap bounds the *read*, not just the
    /// output: without it this never returns.
    #[test]
    fn a_capped_read_stops_at_the_cap_even_on_an_endless_reader() {
        let mut got = 0;
        let hit = read_capped(BufReader::new(std::io::repeat(b'\n')), 100, |_| got += 1).unwrap();
        assert!(hit);
        assert_eq!(got, 100, "each empty line costs one byte, its newline");
    }

    #[test]
    fn a_capped_read_under_the_cap_reads_everything() {
        let mut got = Vec::new();
        let hit = read_capped("a\nb\n".as_bytes(), 100, |l| got.push(l.to_string())).unwrap();
        assert!(!hit);
        assert_eq!(got, vec!["a", "b"]);
    }

    /// `-n all` on a months-old log must not read it whole into memory. The
    /// reader stops at the cap, and says so in the artifact - a dump that
    /// quietly ends early looks like the node stopped logging.
    #[test]
    fn reading_all_of_an_oversized_file_stops_at_the_cap_with_a_note() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.log");
        let line = format!("{}\n", "x".repeat(1023));
        let count = MAX_DUMP_BYTES / line.len() + 100;
        std::fs::write(&path, line.repeat(count)).unwrap();

        let mut r = Redactor::new();
        let sources = LogSources {
            bn: Some(LogTarget::File(path)),
            vc: None,
        };
        let lines = read_sources(&sources, Lines::All, &mut r).unwrap();
        assert!(lines.len() < count, "read all {count} lines");
        assert!(
            lines[0].starts_with("*** tekops: stopped reading the bn log"),
            "{:?}",
            lines[0]
        );
    }

    #[test]
    fn a_missing_file_target_is_an_error_not_an_empty_dump() {
        let mut r = Redactor::new();
        let sources = LogSources {
            bn: Some(LogTarget::File(PathBuf::from("/nope/absent.log"))),
            vc: None,
        };
        assert!(read_sources(&sources, Lines::Last(10), &mut r).is_err());
    }

    /// The dump carries the same interleaving the operator saw, so whoever
    /// they paste it to sees what they saw. This also pins that the tag
    /// survives redaction: it is prepended after the redactor has run, so the
    /// redactor only ever sees the node's own bytes and cannot rewrite the one
    /// column saying which process a line came from.
    #[test]
    fn a_separated_dump_is_one_merged_tagged_redacted_stream() {
        let dir = tempfile::tempdir().unwrap();
        let bn = dir.path().join("bn.log");
        let vc = dir.path().join("vc.log");
        std::fs::write(
            &bn,
            "2026-09-14 01:00:00.000 INFO  - bn one from 93.184.216.34\n\
             2026-09-14 01:00:02.000 INFO  - bn two\n",
        )
        .unwrap();
        std::fs::write(&vc, "2026-09-14 01:00:01.000 INFO  - vc one\n").unwrap();

        let mut r = Redactor::new();
        let sources = LogSources {
            bn: Some(LogTarget::File(bn)),
            vc: Some(LogTarget::File(vc)),
        };
        let lines = read_sources(&sources, Lines::Last(10), &mut r).unwrap();

        assert_eq!(lines.len(), 3, "{lines:?}");
        // Ordered by timestamp across sources, not concatenated per source.
        assert!(lines[0].starts_with("[bn] ") && lines[0].contains("bn one"));
        assert!(lines[1].starts_with("[vc] ") && lines[1].contains("vc one"));
        assert!(lines[2].starts_with("[bn] ") && lines[2].contains("bn two"));
        // Redacted, with the tag intact in front of it.
        assert!(lines[0].contains("<ip-1>"), "{:?}", lines[0]);
        assert!(!lines[0].contains("93.184.216.34"), "{:?}", lines[0]);
    }

    /// A source that cannot be read does not cost the dump its working half,
    /// but it does not vanish either - the artifact says so, because it exists
    /// to be handed to someone else.
    #[test]
    fn an_unreadable_source_becomes_a_visible_note_rather_than_a_silent_omission() {
        let dir = tempfile::tempdir().unwrap();
        let vc = dir.path().join("vc.log");
        std::fs::write(&vc, "2026-09-14 01:00:01.000 INFO  - vc one\n").unwrap();

        let mut r = Redactor::new();
        let sources = LogSources {
            bn: Some(LogTarget::File(PathBuf::from("/nope/absent.log"))),
            vc: Some(LogTarget::File(vc)),
        };
        let lines = read_sources(&sources, Lines::Last(10), &mut r).unwrap();

        assert!(
            lines[0].contains("could not read the bn log"),
            "{:?}",
            lines[0]
        );
        assert!(lines.iter().any(|l| l.contains("vc one")), "{lines:?}");
    }

    #[test]
    fn the_header_names_both_sources() {
        let label = sources_label(&LogSources {
            bn: Some(LogTarget::File(PathBuf::from("/var/log/teku/teku.log"))),
            vc: Some(LogTarget::Container("rocketpool_validator".into())),
        });
        assert!(label.contains("file /var/log/teku/teku.log"), "{label}");
        assert!(label.contains("container rocketpool_validator"), "{label}");
        assert!(label.contains("bn ") && label.contains("vc "), "{label}");
    }

    /// A single-source dump's header must not grow a tag it never had.
    #[test]
    fn a_single_source_header_label_is_unchanged() {
        let label = sources_label(&LogSources {
            bn: Some(LogTarget::Container("eth-docker-consensus-1".into())),
            vc: None,
        });
        assert_eq!(label, "container eth-docker-consensus-1");
    }

    #[test]
    fn an_oversized_dump_is_refused_rather_than_silently_truncated_by_github() {
        // The gist ceiling has to sit below the in-memory one, or the size
        // guard could never fire before the dump was already truncated.
        const _: () = assert!(MAX_GIST_BYTES < MAX_DUMP_BYTES);
        let err = check_gist_size(MAX_GIST_BYTES + 1).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("-n"), "got: {text}");
        assert!(text.contains("-o"), "got: {text}");
    }

    #[test]
    fn a_dump_at_the_limit_is_allowed() {
        assert!(check_gist_size(MAX_GIST_BYTES).is_ok());
    }

    /// The composition table from the spec: --gist alone writes no file, -o
    /// alone writes there, both does both, neither writes the timestamped
    /// default.
    #[test]
    fn destination_composes_with_the_gist_flag() {
        let p = PathBuf::from("chosen.txt");
        assert_eq!(
            destination(&Some(p.clone()), true, 0).unwrap(),
            Destination::File(p.clone())
        );
        assert_eq!(
            destination(&Some(p.clone()), false, 0).unwrap(),
            Destination::File(p)
        );
        assert_eq!(destination(&None, true, 0).unwrap(), Destination::Nowhere);
        assert_eq!(
            destination(&None, false, 1_700_000_000).unwrap(),
            Destination::File(PathBuf::from("tekops-dump-20231114T221320Z.txt"))
        );
    }

    #[test]
    fn a_dash_output_is_stdout() {
        assert_eq!(
            destination(&Some(PathBuf::from("-")), false, 0).unwrap(),
            Destination::Stdout
        );
    }

    /// `--gist` prints the URL alone on stdout so it can be piped to `pbcopy`.
    /// The dump on the same stream would bury it, so the pair is refused.
    #[test]
    fn a_dash_output_with_gist_is_refused() {
        let err = destination(&Some(PathBuf::from("-")), true, 0).unwrap_err();
        assert!(err.to_string().contains("--gist"), "{err}");
    }

    /// The dump file is node logs sitting in whatever directory the operator
    /// was standing in.
    #[test]
    fn the_dump_file_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        write_dump(&path, "hello").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
