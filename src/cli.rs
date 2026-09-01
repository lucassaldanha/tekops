use crate::logs::{stream_logs, LogSource};
use clap::{Parser, Subcommand};
use std::io::{BufReader, LineWriter};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

#[derive(Parser)]
#[command(name = "teku-op", about = "Helper tools for operating a Teku/Besu node")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Tail and colorize a Teku or Besu JSON log file
    Logs {
        source: LogSource,
        path: Option<PathBuf>,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Logs { source, path } => run_logs(source, path),
    }
}

fn run_logs(source: LogSource, path: Option<PathBuf>) -> ExitCode {
    let path = path.unwrap_or_else(|| source.default_path());
    if !path.exists() {
        eprintln!("error: log file not found: {}", path.display());
        return ExitCode::FAILURE;
    }

    let mut tail = match Command::new("tail")
        .args(["-F", "-n", "200"])
        .arg(&path)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("error: failed to spawn tail: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut pager = match Command::new("less")
        .args(["-R", "+F"])
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("error: failed to spawn less: {e}");
            return ExitCode::FAILURE;
        }
    };

    let reader = BufReader::new(tail.stdout.take().expect("tail stdout piped"));
    let mut writer = LineWriter::new(pager.stdin.take().expect("less stdin piped"));

    if let Err(e) = stream_logs(reader, &mut writer) {
        eprintln!("error while streaming logs: {e}");
    }
    drop(writer);

    let _ = pager.wait();
    let _ = tail.kill();
    ExitCode::SUCCESS
}
