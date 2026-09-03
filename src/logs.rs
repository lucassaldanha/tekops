use crate::logfmt::format_log_line;
use clap::ValueEnum;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

/// How much colorized output one session may buffer before it stops following.
///
/// The buffer is a real file (so `less` can search it - see the note in
/// `cli::run_logs`), and it only ever grows. On a busy node an open-ended
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
            writeln!(writer, "*** Scrollback and search still work. Quit and rerun to resume.")?;
            writer.flush()?;
            return Ok(());
        }
        writeln!(writer, "{formatted}")?;
        written = next;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum LogSource {
    Teku,
    Besu,
}

impl LogSource {
    pub fn default_path(&self) -> PathBuf {
        match self {
            LogSource::Teku => PathBuf::from("/var/log/teku/teku.log"),
            LogSource::Besu => PathBuf::from("/var/log/besu/besu.log"),
        }
    }
}

/// Resolves the log file path for `tekops logs`: an explicit positional
/// `path` wins, then `$TEKOPS_LOGS_FILE` (Teku only, since that's the source
/// this env var's own fallback value describes), then the source's default.
pub fn resolve_log_path(
    source: LogSource,
    path: Option<PathBuf>,
    teku_logs_file_env: Option<String>,
) -> PathBuf {
    path.or_else(|| match source {
        LogSource::Teku => teku_logs_file_env.map(PathBuf::from),
        LogSource::Besu => None,
    })
    .unwrap_or_else(|| source.default_path())
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
        assert!(output.contains("buffer limit reached"), "no notice emitted: {output:?}");
        assert!(output.contains("Quit and rerun to resume"));
        assert!(output.contains("line0"), "content before the cap should survive");
        assert!(!output.contains("line499"), "content past the cap should be dropped");
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
        assert!(!output.contains("buffer limit"), "notice on an under-cap stream: {output:?}");
        assert_eq!(output.lines().count(), 1);
    }

    #[test]
    fn hitting_the_cap_is_not_an_error() {
        let input: String = (0..100).map(|i| log_line(&format!("line{i}"))).collect();
        let mut output = Vec::new();
        assert!(stream_logs_capped(Cursor::new(input), &mut output, 50).is_ok());
    }

    #[test]
    fn default_paths_match_existing_bashrc_function() {
        assert_eq!(LogSource::Teku.default_path(), PathBuf::from("/var/log/teku/teku.log"));
        assert_eq!(LogSource::Besu.default_path(), PathBuf::from("/var/log/besu/besu.log"));
    }

    #[test]
    fn resolve_log_path_prefers_explicit_positional_path() {
        let path = resolve_log_path(
            LogSource::Teku,
            Some(PathBuf::from("/custom.log")),
            Some("/env.log".to_string()),
        );
        assert_eq!(path, PathBuf::from("/custom.log"));
    }

    #[test]
    fn resolve_log_path_falls_back_to_env_var_for_teku() {
        let path = resolve_log_path(LogSource::Teku, None, Some("/env.log".to_string()));
        assert_eq!(path, PathBuf::from("/env.log"));
    }

    #[test]
    fn resolve_log_path_falls_back_to_default_when_unset() {
        let path = resolve_log_path(LogSource::Teku, None, None);
        assert_eq!(path, PathBuf::from("/var/log/teku/teku.log"));
    }

    #[test]
    fn resolve_log_path_ignores_env_var_for_besu() {
        let path = resolve_log_path(LogSource::Besu, None, Some("/env.log".to_string()));
        assert_eq!(path, PathBuf::from("/var/log/besu/besu.log"));
    }
}
