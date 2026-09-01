use crate::logfmt::format_log_line;
use clap::ValueEnum;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

pub fn stream_logs<R: BufRead, W: Write>(reader: R, writer: &mut W) -> io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        writeln!(writer, "{}", format_log_line(&line))?;
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

    #[test]
    fn default_paths_match_existing_bashrc_function() {
        assert_eq!(LogSource::Teku.default_path(), PathBuf::from("/var/log/teku/teku.log"));
        assert_eq!(LogSource::Besu.default_path(), PathBuf::from("/var/log/besu/besu.log"));
    }
}
