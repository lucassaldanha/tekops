use crate::beaconapi::BeaconClient;
use crate::logs::{stream_logs, LogSource};
use crate::output::format_health_summary;
use clap::{Parser, Subcommand};
use std::env;
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
    /// Query the node's Beacon API
    Beacon {
        #[command(subcommand)]
        command: BeaconCommand,
        /// Beacon API base URL (default: http://localhost:5051, or $TEKU_OP_API_URL)
        #[arg(long, global = true)]
        api_url: Option<String>,
        /// Print the raw API response as JSON instead of a summary
        #[arg(long, global = true)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum BeaconCommand {
    /// Node health and sync status
    Health,
    /// Chain head slot/root and finality checkpoints
    Head,
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Logs { source, path } => run_logs(source, path),
        Commands::Beacon { command, api_url, json } => {
            let base_url = api_url
                .or_else(|| env::var("TEKU_OP_API_URL").ok())
                .unwrap_or_else(|| "http://localhost:5051".to_string());
            let client = BeaconClient::new(base_url);
            run_beacon(client, command, json)
        }
    }
}

fn run_beacon(client: BeaconClient, command: BeaconCommand, json: bool) -> ExitCode {
    match command {
        BeaconCommand::Health => {
            let syncing = match client.syncing() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let health = match client.health() {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if json {
                println!(
                    "{{\"is_syncing\":{},\"is_optimistic\":{},\"head_slot\":\"{}\",\"sync_distance\":\"{}\"}}",
                    syncing.is_syncing, syncing.is_optimistic, syncing.head_slot, syncing.sync_distance
                );
            } else {
                println!("{}", format_health_summary(&health, &syncing));
            }
            ExitCode::SUCCESS
        }
        BeaconCommand::Head => {
            let header = match client.header_head() {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let finality = match client.finality_checkpoints() {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            if json {
                println!(
                    "{{\"slot\":\"{}\",\"root\":\"{}\",\"current_justified_epoch\":\"{}\",\"finalized_epoch\":\"{}\"}}",
                    header.slot, header.root, finality.current_justified_epoch, finality.finalized_epoch
                );
            } else {
                println!("{}", crate::output::format_head_summary(&header, &finality));
            }
            ExitCode::SUCCESS
        }
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
