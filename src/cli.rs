use crate::beaconapi::{BeaconClient, BlockHeader, FinalityCheckpoints, HealthState, SyncingStatus};
use crate::http::ApiError;
use crate::logs::{resolve_log_path, stream_logs, LogSource};
use crate::metrics::MetricsClient;
use crate::protocol::classify_protocol;
use crate::output::{
    format_attester_duties, format_duties_table, format_head_table, format_health_table,
    format_peers_table, format_proposer_duties, format_validators_table, PeerRow,
};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{generate, Shell};
use serde::Serialize;
use std::env;
use std::io::{self, BufReader, LineWriter};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::thread;

#[derive(Parser)]
#[command(name = "tekops", about = "Helper tools for operating a Teku/Besu node")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Tail and colorize a Teku or Besu JSON log file (defaults to teku)
    Logs {
        source: Option<LogSource>,
        path: Option<PathBuf>,
    },
    /// Query the node's Beacon API
    Beacon {
        #[command(subcommand)]
        command: BeaconCommand,
        /// Beacon API base URL (default: http://localhost:5051, or $TEKOPS_API_URL)
        #[arg(long, global = true)]
        api_url: Option<String>,
        /// Print a JSON-serialized summary instead of a formatted table
        #[arg(long, global = true)]
        json: bool,
    },
    /// List connected peers, grouped by direction/protocol, with peer counts
    Peers {
        /// Beacon API base URL (default: http://localhost:5051, or $TEKOPS_API_URL)
        #[arg(long)]
        api_url: Option<String>,
        /// Print a JSON-serialized summary instead of a formatted table
        #[arg(long)]
        json: bool,
    },
    /// Node health and sync status
    Health {
        /// Beacon API base URL (default: http://localhost:5051, or $TEKOPS_API_URL)
        #[arg(long)]
        api_url: Option<String>,
        /// Print a JSON-serialized summary instead of a formatted table
        #[arg(long)]
        json: bool,
    },
    /// Chain head slot/root and finality checkpoints
    Head {
        /// Beacon API base URL (default: http://localhost:5051, or $TEKOPS_API_URL)
        #[arg(long)]
        api_url: Option<String>,
        /// Print a JSON-serialized summary instead of a formatted table
        #[arg(long)]
        json: bool,
    },
    /// Published blocks, attestations, sync committee messages, and aggregates
    Duties {
        /// Prometheus metrics URL (default: http://localhost:8081/metrics, or $TEKOPS_METRIC_URL)
        #[arg(long)]
        metric_url: Option<String>,
        /// Print a JSON-serialized summary instead of a formatted table
        #[arg(long)]
        json: bool,
    },
    /// Print a shell completion script
    Completion { shell: Shell },
}

#[derive(Subcommand)]
enum BeaconCommand {
    /// Show status for one or more validators (by index or pubkey)
    Validators {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Attester or proposer duties for a given epoch
    Duties {
        #[command(subcommand)]
        kind: DutiesKind,
    },
}

#[derive(Subcommand)]
enum DutiesKind {
    /// Attester duties for a set of validator indices in a given epoch
    Attester {
        #[arg(long)]
        epoch: u64,
        #[arg(required = true)]
        indices: Vec<String>,
    },
    /// Proposer duties for a given epoch
    Proposer {
        #[arg(long)]
        epoch: u64,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Logs { source, path } => run_logs(source.unwrap_or(LogSource::Teku), path),
        Commands::Beacon { command, api_url, json } => {
            let client = BeaconClient::new(resolve_base_url(api_url));
            run_beacon(client, command, json)
        }
        Commands::Peers { api_url, json } => {
            let client = BeaconClient::new(resolve_base_url(api_url));
            exit_for(beacon_peers(&client, json))
        }
        Commands::Health { api_url, json } => {
            let client = BeaconClient::new(resolve_base_url(api_url));
            exit_for(beacon_health(&client, json))
        }
        Commands::Head { api_url, json } => {
            let client = BeaconClient::new(resolve_base_url(api_url));
            exit_for(beacon_head(&client, json))
        }
        Commands::Duties { metric_url, json } => {
            let client = MetricsClient::new(resolve_metric_url(metric_url));
            exit_for(metrics_duties(&client, json))
        }
        Commands::Completion { shell } => {
            let mut cmd = Cli::command();
            generate(shell, &mut cmd, "tekops", &mut io::stdout());
            ExitCode::SUCCESS
        }
    }
}

fn resolve_base_url(api_url: Option<String>) -> String {
    api_url
        .or_else(|| env::var("TEKOPS_API_URL").ok())
        .unwrap_or_else(|| "http://localhost:5051".to_string())
}

fn resolve_metric_url(metric_url: Option<String>) -> String {
    metric_url
        .or_else(|| env::var("TEKOPS_METRIC_URL").ok())
        .unwrap_or_else(|| "http://localhost:8081/metrics".to_string())
}

fn exit_for(result: Result<(), ApiError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Serialize)]
struct HealthJson<'a> {
    health: &'a HealthState,
    #[serde(flatten)]
    syncing: &'a SyncingStatus,
}

#[derive(Serialize)]
struct HeadJson<'a> {
    #[serde(flatten)]
    header: &'a BlockHeader,
    #[serde(flatten)]
    finality: &'a FinalityCheckpoints,
}

fn run_beacon(client: BeaconClient, command: BeaconCommand, json: bool) -> ExitCode {
    let result = match command {
        BeaconCommand::Validators { ids } => beacon_validators(&client, &ids, json),
        BeaconCommand::Duties { kind } => match kind {
            DutiesKind::Attester { epoch, indices } => {
                beacon_duties_attester(&client, epoch, &indices, json)
            }
            DutiesKind::Proposer { epoch } => beacon_duties_proposer(&client, epoch, json),
        },
    };
    exit_for(result)
}

fn beacon_health(client: &BeaconClient, json: bool) -> Result<(), ApiError> {
    let syncing = client.syncing()?;
    let health = client.health()?;
    if json {
        let payload = HealthJson { health: &health, syncing: &syncing };
        println!("{}", serde_json::to_string(&payload).expect("serialize health json"));
    } else {
        println!("{}", format_health_table(&health, &syncing));
    }
    Ok(())
}

fn beacon_head(client: &BeaconClient, json: bool) -> Result<(), ApiError> {
    let header = client.header_head()?;
    let finality = client.finality_checkpoints()?;
    if json {
        let payload = HeadJson { header: &header, finality: &finality };
        println!("{}", serde_json::to_string(&payload).expect("serialize head json"));
    } else {
        println!("{}", format_head_table(&header, &finality));
    }
    Ok(())
}

fn metrics_duties(client: &MetricsClient, json: bool) -> Result<(), ApiError> {
    let metrics = client.duties()?;
    if json {
        println!("{}", serde_json::to_string(&metrics).expect("serialize duties metrics json"));
    } else {
        println!("{}", format_duties_table(&metrics));
    }
    Ok(())
}

fn beacon_peers(client: &BeaconClient, json: bool) -> Result<(), ApiError> {
    let peers = client.peers()?;
    let rows: Vec<PeerRow> = peers
        .into_iter()
        .map(|p| PeerRow {
            peer_id: p.peer_id,
            direction: p.direction,
            state: p.state,
            protocol: classify_protocol(&p.last_seen_p2p_address),
        })
        .collect();
    if json {
        println!("{}", serde_json::to_string(&rows).expect("serialize peers json"));
    } else {
        println!("{}", format_peers_table(&rows));
    }
    Ok(())
}

fn beacon_validators(client: &BeaconClient, ids: &[String], json: bool) -> Result<(), ApiError> {
    let validators = client.validators(ids)?;
    if json {
        println!("{}", serde_json::to_string(&validators).expect("serialize validators json"));
    } else {
        println!("{}", format_validators_table(&validators));
    }
    Ok(())
}

fn beacon_duties_attester(
    client: &BeaconClient,
    epoch: u64,
    indices: &[String],
    json: bool,
) -> Result<(), ApiError> {
    let duties = client.duties_attester(epoch, indices)?;
    if json {
        println!("{}", serde_json::to_string(&duties).expect("serialize attester duties json"));
    } else {
        println!("{}", format_attester_duties(&duties));
    }
    Ok(())
}

fn beacon_duties_proposer(client: &BeaconClient, epoch: u64, json: bool) -> Result<(), ApiError> {
    let duties = client.duties_proposer(epoch)?;
    if json {
        println!("{}", serde_json::to_string(&duties).expect("serialize proposer duties json"));
    } else {
        println!("{}", format_proposer_duties(&duties));
    }
    Ok(())
}

fn run_logs(source: LogSource, path: Option<PathBuf>) -> ExitCode {
    let path = resolve_log_path(source, path, env::var("TEKOPS_LOGS_FILE").ok());
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
            let _ = tail.kill();
            return ExitCode::FAILURE;
        }
    };

    let reader = BufReader::new(tail.stdout.take().expect("tail stdout piped"));
    let mut writer = LineWriter::new(pager.stdin.take().expect("less stdin piped"));

    // tail -F never reaches EOF, so the pager (not the streaming loop) owns
    // process lifetime: run streaming on its own thread and wait on the pager.
    let streaming = thread::spawn(move || stream_logs(reader, &mut writer));

    let _ = pager.wait();
    let _ = tail.kill();
    let _ = tail.wait();

    match streaming.join().expect("log streaming thread panicked") {
        Ok(()) => ExitCode::SUCCESS,
        // The pager closing its stdin (a clean quit) surfaces here as a broken pipe.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error while streaming logs: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beaconapi::{AttesterDuty, ProposerDuty, ValidatorInfo};
    use crate::protocol::Protocol;
    use clap::CommandFactory;
    use clap_complete::{generate, Shell};

    #[test]
    fn generates_non_empty_bash_completion() {
        let mut cmd = Cli::command();
        let mut buf = Vec::new();
        generate(Shell::Bash, &mut cmd, "tekops", &mut buf);
        let script = String::from_utf8(buf).unwrap();
        assert!(!script.is_empty());
        assert!(script.contains("tekops"));
    }

    #[test]
    fn completion_subcommand_parses() {
        let cli = Cli::try_parse_from(["tekops", "completion", "bash"]).unwrap();
        assert!(matches!(cli.command, Commands::Completion { shell: Shell::Bash }));
    }

    #[test]
    fn validators_requires_at_least_one_id() {
        let result = Cli::try_parse_from(["tekops", "beacon", "validators"]);
        assert!(result.is_err());
    }

    #[test]
    fn logs_without_source_parses_with_source_none() {
        let cli = Cli::try_parse_from(["tekops", "logs"]).unwrap();
        assert!(matches!(cli.command, Commands::Logs { source: None, path: None }));
    }

    #[test]
    fn logs_with_explicit_source_still_parses() {
        let cli = Cli::try_parse_from(["tekops", "logs", "besu"]).unwrap();
        assert!(matches!(cli.command, Commands::Logs { source: Some(LogSource::Besu), path: None }));
    }

    #[test]
    fn peers_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "peers"]).unwrap();
        assert!(matches!(cli.command, Commands::Peers { api_url: None, json: false }));
    }

    #[test]
    fn health_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "health"]).unwrap();
        assert!(matches!(cli.command, Commands::Health { api_url: None, json: false }));
    }

    #[test]
    fn head_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "head"]).unwrap();
        assert!(matches!(cli.command, Commands::Head { api_url: None, json: false }));
    }

    #[test]
    fn duties_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "duties"]).unwrap();
        assert!(matches!(cli.command, Commands::Duties { metric_url: None, json: false }));
    }

    #[test]
    fn duties_attester_requires_at_least_one_index() {
        let result = Cli::try_parse_from(["tekops", "beacon", "duties", "attester", "--epoch", "1"]);
        assert!(result.is_err());
    }

    #[test]
    fn health_json_output_is_valid_json() {
        let syncing = SyncingStatus {
            is_syncing: true,
            is_optimistic: false,
            head_slot: "123".to_string(),
            sync_distance: "4".to_string(),
        };
        let health = HealthState::Ready;
        let payload = HealthJson { health: &health, syncing: &syncing };
        let json = serde_json::to_string(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["health"], "ready");
        assert_eq!(value["is_syncing"], true);
        assert_eq!(value["head_slot"], "123");
    }

    #[test]
    fn head_json_output_is_valid_json() {
        let header = BlockHeader { slot: "999".to_string(), root: "0xabc".to_string() };
        let finality = FinalityCheckpoints {
            previous_justified_epoch: "10".to_string(),
            current_justified_epoch: "11".to_string(),
            finalized_epoch: "9".to_string(),
        };
        let payload = HeadJson { header: &header, finality: &finality };
        let json = serde_json::to_string(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["slot"], "999");
        assert_eq!(value["finalized_epoch"], "9");
    }

    #[test]
    fn peers_json_output_is_valid_json() {
        let rows = vec![PeerRow {
            peer_id: "p1".to_string(),
            direction: "inbound".to_string(),
            state: "connected".to_string(),
            protocol: Protocol::Tcp,
        }];
        let json = serde_json::to_string(&rows).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value[0]["protocol"], "tcp");
    }

    #[test]
    fn peers_json_output_escapes_untrusted_fields() {
        let rows = vec![PeerRow {
            peer_id: "p\"1".to_string(),
            direction: "inbound".to_string(),
            state: "connected".to_string(),
            protocol: Protocol::Tcp,
        }];
        let json = serde_json::to_string(&rows).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value[0]["peer_id"], "p\"1");
    }

    #[test]
    fn validators_json_output_is_valid_json() {
        let validators = vec![ValidatorInfo {
            index: "1".to_string(),
            pubkey: "0xabc".to_string(),
            balance: "32000000000".to_string(),
            status: "active_ongoing".to_string(),
        }];
        let json = serde_json::to_string(&validators).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value[0]["index"], "1");
    }

    #[test]
    fn attester_duties_json_output_is_valid_json() {
        let duties = vec![AttesterDuty {
            pubkey: "0xabc".to_string(),
            validator_index: "1".to_string(),
            committee_index: "2".to_string(),
            slot: "100".to_string(),
        }];
        let json = serde_json::to_string(&duties).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value[0]["slot"], "100");
    }

    #[test]
    fn duties_metrics_json_output_is_valid_json() {
        use crate::metrics::DutiesMetrics;
        let metrics = DutiesMetrics {
            published_blocks: 1,
            published_attestations: 2,
            published_sync_committee_messages: 3,
            published_aggregates: 4,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["published_blocks"], 1);
        assert_eq!(value["published_attestations"], 2);
        assert_eq!(value["published_sync_committee_messages"], 3);
        assert_eq!(value["published_aggregates"], 4);
    }

    #[test]
    fn proposer_duties_json_output_is_valid_json() {
        let duties = vec![ProposerDuty {
            pubkey: "0xdef".to_string(),
            validator_index: "3".to_string(),
            slot: "101".to_string(),
        }];
        let json = serde_json::to_string(&duties).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value[0]["pubkey"], "0xdef");
    }
}
