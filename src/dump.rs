//! `tekops dump-logs`: a shareable, anonymised snapshot of the node's log.
//!
//! Split the way `logfmt.rs` and `host.rs` are: `render_dump` and the date
//! helpers are pure and hold every formatting decision, and `run_dump` holds
//! all the I/O.

use crate::logs::{producer_argv, LogTarget, Mode};
use crate::redact::{Redactor, Summary};
use crate::stack::Stack;
use crate::term::sanitize;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A ceiling on how much a single dump will hold in memory.
///
/// `tail -n` and `docker logs --tail` already bound the line *count*, so this
/// only matters for a log whose individual lines are enormous - a stack-trace
/// storm, or a line carrying an embedded payload. Sixteen mebibytes is far
/// above any honest 1000-line dump and far below anything that would trouble
/// the node.
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
    Cancelled,
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
            DumpError::Cancelled => write!(f, "cancelled"),
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
    pub lines: u32,
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
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// Spawns the producer, reads its output, and redacts each line.
fn read_lines(target: &LogTarget, lines: u32, r: &mut Redactor) -> Result<Vec<String>, DumpError> {
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

    let mut out = Vec::new();
    let mut bytes = 0usize;
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| DumpError::Io(e.to_string()))?;
        let redacted = redact_line(r, &line);
        bytes = bytes.saturating_add(redacted.len() + 1);
        if bytes > MAX_DUMP_BYTES {
            out.push(format!(
                "*** tekops: {} MiB limit reached, dump truncated here.",
                MAX_DUMP_BYTES / (1024 * 1024)
            ));
            break;
        }
        out.push(redacted);
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

/// A human label for what was read, for the header.
fn target_label(target: &LogTarget) -> String {
    match target {
        LogTarget::File(p) => format!("file {}", p.display()),
        LogTarget::Container(c) => format!("container {c}"),
    }
}

/// Where the dump gets written, if anywhere.
///
/// `-o` always wins. With no `-o`, `--gist` writes nothing (the gist is the
/// artifact) and a plain run writes the timestamped default. Pure, so the
/// composition is testable without an upload.
fn output_path(output: &Option<PathBuf>, gist: bool, now: u64) -> Option<PathBuf> {
    match (output, gist) {
        (Some(p), _) => Some(p.clone()),
        (None, false) => Some(PathBuf::from(default_filename(now))),
        (None, true) => None,
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
fn confirm_upload(rendered: &str, summary: &Summary) -> Result<bool, DumpError> {
    let total = rendered.lines().count();
    eprintln!(
        "about to upload {total} lines ({} KB) to a secret gist",
        rendered.len() / 1024
    );
    eprintln!("redacted {summary}");
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
    pub target: LogTarget,
    pub lines: u32,
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

    let mut r = Redactor::new();

    // Header field values are redacted individually as they are gathered,
    // rather than by running the finished header through the redactor: the
    // header reports the run's redaction total, so redacting it afterwards
    // would let it invalidate the number it has already printed.
    let header_bits = cfg.header.then(|| {
        (
            redact_line(&mut r, &stack_label(cfg.stack)),
            redact_line(&mut r, &target_label(&cfg.target)),
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

    let lines = read_lines(&cfg.target, cfg.lines, &mut r)?;

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

    let wrote = output_path(&cfg.output, cfg.gist, cfg.now);
    if let Some(p) = &wrote {
        write_dump(p, &rendered)?;
    }

    eprintln!("redacted {summary}");
    if let Some(p) = &wrote {
        println!("{}", p.display());
    }

    if let Some(token) = token {
        check_gist_size(rendered.len())?;
        if !cfg.yes && !confirm_upload(&rendered, &summary)? {
            return Err(DumpError::Cancelled);
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
            lines: 1000,
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
        let lines = read_lines(&LogTarget::File(path), 3, &mut r).unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("<ip-1>"));
        assert!(!lines[0].contains("93.184.216.34"));
    }

    #[test]
    fn a_missing_file_target_is_an_error_not_an_empty_dump() {
        let mut r = Redactor::new();
        let target = LogTarget::File(PathBuf::from("/nope/absent.log"));
        assert!(read_lines(&target, 10, &mut r).is_err());
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
    fn output_path_composes_with_the_gist_flag() {
        let p = PathBuf::from("chosen.txt");
        assert_eq!(output_path(&Some(p.clone()), true, 0), Some(p.clone()));
        assert_eq!(output_path(&Some(p.clone()), false, 0), Some(p));
        assert_eq!(output_path(&None, true, 0), None);
        assert_eq!(
            output_path(&None, false, 1_700_000_000),
            Some(PathBuf::from("tekops-dump-20231114T221320Z.txt"))
        );
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
