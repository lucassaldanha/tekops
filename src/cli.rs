use crate::beaconapi::{
    BeaconClient, BlockHeader, FinalityCheckpoints, HealthState, SyncingStatus,
};
use crate::completions::{self, detect_shell, CompletionError, RcOutcome};
use crate::http::ApiError;
use crate::logs::{resolve_logs_target, run_logs, LogSource};
use crate::metrics::MetricsClient;
use crate::output::{
    format_about, format_attester_duties, format_duties_table, format_head_table,
    format_health_table, format_peers_table, format_proposer_duties,
    format_validator_metrics_table, format_validators_table, format_version_table, PeerRow,
};
use crate::protocol::classify_protocol;
use crate::stack::{detect_stack, Stack};
use crate::update::{self, resolve_update_target, UpdateError, UpdateTarget};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use std::env;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

#[derive(Parser)]
#[command(
    name = "tekops",
    about = "Helper tools for operating a Teku/Besu node",
    // Reports the tekops build itself, which is distinct from `tekops version`
    // (the running Teku's version, read from the metrics endpoint). The deploy
    // model is a hand-copied binary, so a node's build is otherwise unknowable.
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// The live `clap::Command` for the whole CLI.
///
/// Exposed so `completions` can render a shell script from the same definition
/// that parses arguments, rather than from a second, drift-prone description of
/// it.
pub fn command() -> clap::Command {
    <Cli as clap::CommandFactory>::command()
}

/// The Beacon API connection flags, shared by every command that talks to it
/// rather than repeated per command.
///
/// `global` keeps `tekops beacon validators 1 --json` working - the flags used
/// to be declared that way on the `beacon` command specifically, so dropping it
/// here would silently break trailing flags on its subcommands. On the leaf
/// commands, which have no subcommands to propagate to, it's a no-op.
#[derive(clap::Args)]
struct ApiArgs {
    /// Beacon API base URL (default: http://localhost:5051, or $TEKOPS_API_URL)
    #[arg(long, global = true)]
    api_url: Option<String>,
    /// Deployment to take port defaults from (or $TEKOPS_STACK)
    #[arg(long, global = true)]
    stack: Option<Stack>,
    /// Print a JSON-serialized summary instead of a formatted table
    #[arg(long, global = true)]
    json: bool,
}

/// The Prometheus scrape flags, shared by every command that reads metrics.
#[derive(clap::Args)]
struct MetricArgs {
    /// Prometheus metrics URL (default: http://localhost:8010/metrics, or $TEKOPS_METRIC_URL)
    #[arg(long, global = true)]
    metric_url: Option<String>,
    /// Deployment to take port defaults from (or $TEKOPS_STACK)
    #[arg(long, global = true)]
    stack: Option<Stack>,
    /// Print a JSON-serialized summary instead of a formatted table
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Tail and colorize a Teku or Besu log file, or a container's logs
    Logs {
        /// teku, besu, or a path to a log file (defaults to teku)
        //
        // `conflicts_with` goes on BOTH positionals, not just `path`. A lone
        // positional path lands in `source`, not `path` (see
        // `resolve_logs_target`), so guarding only `path` would let
        // `tekops logs /a.log --container c` parse with two answers to one
        // question. Naming a container fully determines what gets read, which
        // makes a source or a path alongside it meaningless rather than merely
        // redundant.
        #[arg(conflicts_with = "container")]
        source: Option<String>,
        #[arg(conflicts_with = "container")]
        path: Option<PathBuf>,
        /// Lines of existing log to show before following new output
        #[arg(short = 'n', long = "lines", default_value_t = 500)]
        lines: u32,
        /// Docker container to read logs from (or $TEKOPS_CONTAINER)
        #[arg(long)]
        container: Option<String>,
        /// Deployment to narrow container detection to (or $TEKOPS_STACK)
        #[arg(long)]
        stack: Option<Stack>,
    },
    /// Query the node's Beacon API
    Beacon {
        #[command(subcommand)]
        command: BeaconCommand,
        #[command(flatten)]
        api: ApiArgs,
    },
    /// List connected peers, grouped by direction/protocol, with peer counts
    Peers {
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Node health and sync status
    Health {
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Chain head slot/root and finality checkpoints
    Head {
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Published blocks, attestations, sync committee messages, and aggregates
    Duties {
        #[command(flatten)]
        metrics: MetricArgs,
    },
    /// Validator key counts by status, and total locally-stated ETH balance
    Validators {
        #[command(flatten)]
        metrics: MetricArgs,
    },
    /// Running Teku version, read from the beacon node or validator client
    /// metrics (whichever is present on the scrape)
    Version {
        #[command(flatten)]
        metrics: MetricArgs,
    },
    /// Set the node's runtime log level, optionally scoped to specific loggers
    LogLevel {
        /// Log level to set (e.g. INFO, DEBUG, WARN, TRACE)
        level: String,
        /// Logger name(s) to scope the change to (e.g. org.hyperledger.besu);
        /// omit to change the global level
        #[arg(long = "filter")]
        log_filter: Vec<String>,
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Install shell completions for tekops (detects your shell if not named)
    Autocomplete {
        /// bash, zsh, or fish; omit to detect from $SHELL
        shell: Option<completions::Shell>,
        /// Write the script to stdout and install nothing
        #[arg(long)]
        print: bool,
        /// Install without asking for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Print what tekops is, the build version, and where to find the source
    About,
    /// Update the tekops binary itself from GitHub releases
    Update {
        /// check, latest, or a version (e.g. 0.3.0); omit to check and confirm
        target: Option<String>,
        /// Print a JSON summary instead of a table (applies to `check`)
        #[arg(long)]
        json: bool,
        /// Install without asking for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },
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
        Commands::Logs {
            source,
            path,
            lines,
            container,
            stack,
        } => match resolve_logs_target(source, path) {
            Ok((source, path)) => {
                // Detection only runs when nothing else has answered, so a
                // configured operator never pays for a `docker ps` spawn. Note
                // `path` here is the *resolved* path, which is what catches the
                // lone-positional case where a path arrived in `source`.
                //
                // This condition must stay the logical negation of every branch
                // in `logs::resolve_log_target` that fires before its `detected`
                // parameter - nothing ties the two together at compile time, so
                // a change to that ladder has to be mirrored here by hand.
                let logs_file_env = env::var("TEKOPS_LOGS_FILE").ok();
                let needs_detection = container.is_none()
                    && path.is_none()
                    && env::var("TEKOPS_CONTAINER").is_err()
                    && !(matches!(source, LogSource::Teku) && logs_file_env.is_some());
                let detected = if needs_detection {
                    let only = resolve_stack(stack, env::var("TEKOPS_STACK").ok());
                    docker_ps_names()
                        .and_then(|ps| detect_stack(&ps, only).ok())
                        .map(|(_, name)| name)
                } else {
                    None
                };
                run_logs(source, path, lines, container, detected)
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Commands::Beacon { command, api } => {
            let stack = resolve_stack(api.stack, env::var("TEKOPS_STACK").ok());
            let client = BeaconClient::new(resolve_base_url(api.api_url, stack));
            run_beacon(client, command, api.json)
        }
        Commands::Peers { api } => {
            let stack = resolve_stack(api.stack, env::var("TEKOPS_STACK").ok());
            let client = BeaconClient::new(resolve_base_url(api.api_url, stack));
            exit_for_api(beacon_peers(&client, api.json))
        }
        Commands::Health { api } => {
            let stack = resolve_stack(api.stack, env::var("TEKOPS_STACK").ok());
            let client = BeaconClient::new(resolve_base_url(api.api_url, stack));
            exit_for_api(beacon_health(&client, api.json))
        }
        Commands::Head { api } => {
            let stack = resolve_stack(api.stack, env::var("TEKOPS_STACK").ok());
            let client = BeaconClient::new(resolve_base_url(api.api_url, stack));
            exit_for_api(beacon_head(&client, api.json))
        }
        Commands::Duties { metrics } => {
            let stack = resolve_stack(metrics.stack, env::var("TEKOPS_STACK").ok());
            let client = MetricsClient::new(resolve_metric_url(metrics.metric_url, stack));
            exit_for_api(metrics_duties(&client, metrics.json))
        }
        Commands::Validators { metrics } => {
            let stack = resolve_stack(metrics.stack, env::var("TEKOPS_STACK").ok());
            let client = MetricsClient::new(resolve_metric_url(metrics.metric_url, stack));
            exit_for_api(metrics_validators(&client, metrics.json))
        }
        Commands::Version { metrics } => {
            let stack = resolve_stack(metrics.stack, env::var("TEKOPS_STACK").ok());
            let client = MetricsClient::new(resolve_metric_url(metrics.metric_url, stack));
            exit_for_api(metrics_version(&client, metrics.json))
        }
        Commands::LogLevel {
            level,
            log_filter,
            api,
        } => {
            let stack = resolve_stack(api.stack, env::var("TEKOPS_STACK").ok());
            let client = BeaconClient::new(resolve_base_url(api.api_url, stack));
            exit_for_api(beacon_log_level(&client, &level, log_filter, api.json))
        }
        Commands::Autocomplete { shell, print, yes } => {
            exit_for(run_autocomplete(shell, print, yes))
        }
        Commands::About => {
            println!("{}", format_about());
            ExitCode::SUCCESS
        }
        Commands::Update { target, json, yes } => {
            exit_for(run_update(resolve_update_target(target), json, yes))
        }
    }
}

/// The stack profile in force, if any. The flag beats `$TEKOPS_STACK`.
///
/// An unparseable environment value is ignored rather than fatal. It is set
/// once in a shell rc and would otherwise break every command in the session,
/// including the ones that never needed it.
fn resolve_stack(flag: Option<Stack>, env: Option<String>) -> Option<Stack> {
    flag.or_else(|| env.and_then(|v| Stack::from_str(&v, true).ok()))
}

fn resolve_base_url(api_url: Option<String>, stack: Option<Stack>) -> String {
    api_url
        .or_else(|| env::var("TEKOPS_API_URL").ok())
        .unwrap_or_else(|| stack.unwrap_or(Stack::BareMetal).api_url().to_string())
}

fn resolve_metric_url(metric_url: Option<String>, stack: Option<Stack>) -> String {
    metric_url
        .or_else(|| env::var("TEKOPS_METRIC_URL").ok())
        .unwrap_or_else(|| stack.unwrap_or(Stack::BareMetal).metric_url().to_string())
}

/// The names of running containers, or `None` if Docker cannot be asked.
///
/// Every failure mode collapses to `None` on purpose: Docker not installed, the
/// daemon not running, and the user not being in the `docker` group are all
/// just "no detection available" as far as callers are concerned, and none of
/// them should produce an error on a bare-metal host that never wanted Docker.
fn docker_ps_names() -> Option<String> {
    let out = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Generic over the error type so `UpdateError` shares the exit path with
/// `ApiError` rather than duplicating it.
fn exit_for<E: fmt::Display>(result: Result<(), E>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The hint text for a given `docker ps` output, or `None` if there is nothing
/// confident to say.
///
/// Split from `docker_stack_hint` so the message is testable without Docker.
fn hint_from_ps(ps: &str) -> Option<String> {
    let (stack, _) = detect_stack(ps, None).ok()?;
    let name = stack.to_possible_value()?;
    Some(format!(
        "hint: detected a {} stack; try --stack {} or set $TEKOPS_STACK",
        name.get_name(),
        name.get_name()
    ))
}

/// Probes Docker once to suggest a stack, for use only on a connection failure.
///
/// This is the one place an HTTP command touches Docker, and it is on the
/// failure path exclusively: putting a `docker ps` spawn on `tekops health`'s
/// happy path would add a dependency and a process spawn to a command that has
/// neither today.
fn docker_stack_hint() -> Option<String> {
    hint_from_ps(&docker_ps_names()?)
}

/// The exit path for commands that talk to a node over HTTP.
///
/// Identical to `exit_for` except that an unreachable endpoint gets a stack
/// hint appended, since "could not reach endpoint" on a Docker host almost
/// always means the ports are the default bare-metal ones.
fn exit_for_api(result: Result<(), ApiError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            if matches!(e, ApiError::Unreachable(_)) {
                if let Some(hint) = docker_stack_hint() {
                    eprintln!("{hint}");
                }
            }
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

#[derive(Serialize)]
struct LogLevelJson<'a> {
    level: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_filter: Option<Vec<String>>,
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
    exit_for_api(result)
}

fn beacon_health(client: &BeaconClient, json: bool) -> Result<(), ApiError> {
    let syncing = client.syncing()?;
    let health = client.health()?;
    if json {
        let payload = HealthJson {
            health: &health,
            syncing: &syncing,
        };
        println!(
            "{}",
            serde_json::to_string(&payload).expect("serialize health json")
        );
    } else {
        println!("{}", format_health_table(&health, &syncing));
    }
    Ok(())
}

fn beacon_head(client: &BeaconClient, json: bool) -> Result<(), ApiError> {
    let header = client.header_head()?;
    let finality = client.finality_checkpoints()?;
    if json {
        let payload = HeadJson {
            header: &header,
            finality: &finality,
        };
        println!(
            "{}",
            serde_json::to_string(&payload).expect("serialize head json")
        );
    } else {
        println!("{}", format_head_table(&header, &finality));
    }
    Ok(())
}

fn metrics_duties(client: &MetricsClient, json: bool) -> Result<(), ApiError> {
    let metrics = client.duties()?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&metrics).expect("serialize duties metrics json")
        );
    } else {
        println!("{}", format_duties_table(&metrics));
    }
    Ok(())
}

fn metrics_validators(client: &MetricsClient, json: bool) -> Result<(), ApiError> {
    let metrics = client.validators()?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&metrics).expect("serialize validator metrics json")
        );
    } else {
        println!("{}", format_validator_metrics_table(&metrics));
    }
    Ok(())
}

fn metrics_version(client: &MetricsClient, json: bool) -> Result<(), ApiError> {
    let info = client.version()?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&info).expect("serialize version json")
        );
    } else {
        println!("{}", format_version_table(&info));
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
        println!(
            "{}",
            serde_json::to_string(&rows).expect("serialize peers json")
        );
    } else {
        println!("{}", format_peers_table(&rows));
    }
    Ok(())
}

fn beacon_validators(client: &BeaconClient, ids: &[String], json: bool) -> Result<(), ApiError> {
    let validators = client.validators(ids)?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&validators).expect("serialize validators json")
        );
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
        println!(
            "{}",
            serde_json::to_string(&duties).expect("serialize attester duties json")
        );
    } else {
        println!("{}", format_attester_duties(&duties));
    }
    Ok(())
}

fn beacon_duties_proposer(client: &BeaconClient, epoch: u64, json: bool) -> Result<(), ApiError> {
    let duties = client.duties_proposer(epoch)?;
    if json {
        println!(
            "{}",
            serde_json::to_string(&duties).expect("serialize proposer duties json")
        );
    } else {
        println!("{}", format_proposer_duties(&duties));
    }
    Ok(())
}

fn beacon_log_level(
    client: &BeaconClient,
    level: &str,
    log_filter: Vec<String>,
    json: bool,
) -> Result<(), ApiError> {
    let log_filter = if log_filter.is_empty() {
        None
    } else {
        Some(log_filter)
    };
    client.set_log_level(level, log_filter.clone())?;
    if json {
        let payload = LogLevelJson { level, log_filter };
        println!(
            "{}",
            serde_json::to_string(&payload).expect("serialize log level json")
        );
    } else {
        match &log_filter {
            Some(loggers) => println!("log level set to {level} for: {}", loggers.join(", ")),
            None => println!("log level set to {level} (global)"),
        }
    }
    Ok(())
}

fn run_update(target: UpdateTarget, json: bool, yes: bool) -> Result<(), UpdateError> {
    match target {
        UpdateTarget::Check => {
            let result = update::check(update::fetch)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&result).map_err(|e| UpdateError::Io(e.to_string()))?
                );
            } else if result.update_available {
                println!(
                    "tekops {} is available (running {})",
                    result.latest, result.current
                );
            } else {
                println!("already on {} (latest)", result.current);
            }
            Ok(())
        }
        UpdateTarget::Prompt | UpdateTarget::Latest => {
            let prompt = matches!(target, UpdateTarget::Prompt);
            let result = update::check(update::fetch)?;
            if !result.update_available {
                println!("already on {} (latest)", result.current);
                return Ok(());
            }
            if prompt && !yes && !confirm(&result.current, &result.latest)? {
                println!("aborted");
                return Ok(());
            }
            install_version(&result.latest)
        }
        // An explicit tag means "make the binary be exactly this", which is
        // both the rollback path and the repair path for a corrupt install, so
        // it neither prompts nor refuses to go backwards.
        UpdateTarget::Version(version) => install_version(&version),
    }
}

/// The environment-reading half of `tekops autocomplete`; everything it decides
/// is computed by `completions`, which stays pure and testable.
fn run_autocomplete(
    shell: Option<completions::Shell>,
    print: bool,
    yes: bool,
) -> Result<(), CompletionError> {
    let shell = match shell {
        Some(shell) => shell,
        None => detect_shell(env::var("SHELL").ok().as_deref())
            .ok_or(CompletionError::UndetectedShell)?,
    };

    let script = completions::generate(shell, &mut command());
    if print {
        print!("{script}");
        return Ok(());
    }

    let plan = completions::plan(shell, &dirs_from_env()?);

    println!("shell:        {shell:?}");
    println!("will write:   {}", plan.script_path.display());
    if let Some(rc) = &plan.rc {
        println!("will append to {}:", rc.path.display());
        for line in rc.stanza.lines().filter(|l| !l.is_empty()) {
            println!("    {line}");
        }
    }
    if !yes && !confirm_install()? {
        println!("aborted");
        return Ok(());
    }

    let applied = completions::apply(&plan, &script)?;
    println!("installed {}", applied.script_path.display());
    match applied.rc {
        RcOutcome::NotNeeded => {}
        RcOutcome::Appended(path) => println!("updated {}", path.display()),
        // Reached on every re-run after the first, which is the common case
        // once someone reinstalls to pick up a new command.
        RcOutcome::AlreadyPresent(path) => {
            println!("{} already set up, left unchanged", path.display())
        }
    }
    println!("restart your shell to pick it up");
    Ok(())
}

fn dirs_from_env() -> Result<completions::Dirs, CompletionError> {
    Ok(completions::Dirs {
        home: env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or(CompletionError::NoHome)?,
        xdg_data: env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        xdg_config: env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
    })
}

fn confirm_install() -> Result<bool, CompletionError> {
    print!("proceed? [y/N] ");
    io::stdout().flush().map_err(|e| CompletionError::Io {
        path: PathBuf::from("<stdout>"),
        message: e.to_string(),
    })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|e| CompletionError::Io {
            path: PathBuf::from("<stdin>"),
            message: e.to_string(),
        })?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn confirm(current: &str, latest: &str) -> Result<bool, UpdateError> {
    print!("update tekops {current} -> {latest}? [y/N] ");
    io::stdout()
        .flush()
        .map_err(|e| UpdateError::Io(e.to_string()))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|e| UpdateError::Io(e.to_string()))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn install_version(version: &str) -> Result<(), UpdateError> {
    let dest = env::current_exe().map_err(|e| UpdateError::Io(e.to_string()))?;
    println!("downloading tekops {version} for {}...", dest.display());
    update::install(update::fetch, version, &dest)?;
    println!("installed tekops {version}");
    refresh_completions(&dest);
    Ok(())
}

/// Re-renders any already-installed completion script against the binary that
/// was just installed, so a release adding a command does not leave a stale
/// script behind.
///
/// Deliberately not part of `install_version`'s result: the new binary is in
/// place and working by this point, so reporting the update as failed because a
/// completion file could not be rewritten would be a lie. A warning names what
/// to re-run instead.
fn refresh_completions(binary: &Path) {
    let refreshed = dirs_from_env().and_then(|dirs| completions::refresh_installed(binary, &dirs));
    match refreshed {
        Ok(paths) => {
            for path in paths {
                println!("refreshed completions at {}", path.display());
            }
        }
        Err(e) => eprintln!(
            "warning: could not refresh shell completions ({e}); re-run `tekops autocomplete`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beaconapi::{AttesterDuty, ProposerDuty, ValidatorInfo};
    use crate::protocol::Protocol;

    #[test]
    fn validators_requires_at_least_one_id() {
        let result = Cli::try_parse_from(["tekops", "beacon", "validators"]);
        assert!(result.is_err());
    }

    #[test]
    fn logs_without_source_parses_with_source_none() {
        let cli = Cli::try_parse_from(["tekops", "logs"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Logs {
                source: None,
                path: None,
                ..
            }
        ));
    }

    #[test]
    fn logs_with_explicit_source_still_parses() {
        let cli = Cli::try_parse_from(["tekops", "logs", "besu"]).unwrap();
        match cli.command {
            Commands::Logs { source, path, .. } => {
                assert_eq!(source.as_deref(), Some("besu"));
                assert_eq!(path, None);
            }
            _ => panic!("expected a Logs command"),
        }
    }

    // Regression: clap used to type this positional as a LogSource, so a bare
    // path was rejected outright with "invalid value for [SOURCE]" despite
    // being the form the README documents. Meaning is assigned by
    // logs::resolve_logs_target, which is where the behaviour is tested; this
    // only asserts the parser lets a path through at all.
    #[test]
    fn logs_accepts_a_bare_path_as_the_first_positional() {
        let cli = Cli::try_parse_from(["tekops", "logs", "/var/log/x.log"]).unwrap();
        match cli.command {
            Commands::Logs { source, path, .. } => {
                assert_eq!(source.as_deref(), Some("/var/log/x.log"));
                assert_eq!(path, None);
            }
            _ => panic!("expected a Logs command"),
        }
    }

    // Commands deliberately doesn't derive Debug, so these match rather than
    // assert_eq! on the variant and panic with a fixed string.
    fn parsed_log_lines(args: &[&str]) -> u32 {
        let cli = Cli::try_parse_from(args).unwrap();
        match cli.command {
            Commands::Logs { lines, .. } => lines,
            _ => panic!("expected a Logs command"),
        }
    }

    #[test]
    fn logs_defaults_to_500_lines_of_scrollback() {
        assert_eq!(parsed_log_lines(&["tekops", "logs"]), 500);
    }

    #[test]
    fn logs_accepts_a_short_and_long_line_count() {
        assert_eq!(parsed_log_lines(&["tekops", "logs", "-n", "50"]), 50);
        assert_eq!(parsed_log_lines(&["tekops", "logs", "--lines", "50"]), 50);
    }

    #[test]
    fn logs_line_count_combines_with_source_and_path() {
        let cli = Cli::try_parse_from(["tekops", "logs", "besu", "/tmp/x.log", "-n", "7"]).unwrap();
        match cli.command {
            Commands::Logs {
                source,
                path,
                lines,
                ..
            } => {
                assert_eq!(source.as_deref(), Some("besu"));
                assert_eq!(path, Some(PathBuf::from("/tmp/x.log")));
                assert_eq!(lines, 7);
            }
            _ => panic!("expected a Logs command"),
        }
    }

    #[test]
    fn logs_rejects_a_negative_line_count() {
        assert!(Cli::try_parse_from(["tekops", "logs", "-n", "-5"]).is_err());
    }

    #[test]
    fn logs_allows_zero_lines_to_follow_only_new_output() {
        assert_eq!(parsed_log_lines(&["tekops", "logs", "-n", "0"]), 0);
    }

    #[test]
    fn peers_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "peers"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Peers {
                api: ApiArgs {
                    api_url: None,
                    stack: None,
                    json: false
                }
            }
        ));
    }

    #[test]
    fn health_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "health"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Health {
                api: ApiArgs {
                    api_url: None,
                    stack: None,
                    json: false
                }
            }
        ));
    }

    #[test]
    fn head_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "head"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Head {
                api: ApiArgs {
                    api_url: None,
                    stack: None,
                    json: false
                }
            }
        ));
    }

    #[test]
    fn duties_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "duties"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Duties {
                metrics: MetricArgs {
                    metric_url: None,
                    stack: None,
                    json: false
                }
            }
        ));
    }

    #[test]
    fn validators_is_a_top_level_command_distinct_from_beacon_validators() {
        let cli = Cli::try_parse_from(["tekops", "validators"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Validators {
                metrics: MetricArgs {
                    metric_url: None,
                    stack: None,
                    json: false
                }
            }
        ));
    }

    #[test]
    fn version_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "version"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Version {
                metrics: MetricArgs {
                    metric_url: None,
                    stack: None,
                    json: false
                }
            }
        ));
    }

    #[test]
    fn autocomplete_takes_an_optional_shell_and_defaults_to_detecting_one() {
        let cli = Cli::try_parse_from(["tekops", "autocomplete"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Autocomplete { shell: None, .. }
        ));

        let cli = Cli::try_parse_from(["tekops", "autocomplete", "zsh"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Autocomplete {
                shell: Some(completions::Shell::Zsh),
                ..
            }
        ));
    }

    /// Unlike `logs` and `update`, this positional never doubles as a path or a
    /// subcommand name, so clap can type it and reject the rest at parse time
    /// rather than the command failing at runtime.
    #[test]
    fn autocomplete_rejects_a_shell_it_cannot_install_for() {
        assert!(Cli::try_parse_from(["tekops", "autocomplete", "elvish"]).is_err());
    }

    #[test]
    fn autocomplete_accepts_print_and_yes() {
        let cli = Cli::try_parse_from(["tekops", "autocomplete", "bash", "--print"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Autocomplete { print: true, .. }
        ));

        let cli = Cli::try_parse_from(["tekops", "autocomplete", "-y"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Autocomplete { yes: true, .. }
        ));
    }

    #[test]
    fn about_is_a_top_level_command_taking_no_arguments() {
        let cli = Cli::try_parse_from(["tekops", "about"]).unwrap();
        assert!(matches!(cli.command, Commands::About));
    }

    #[test]
    fn duties_attester_requires_at_least_one_index() {
        let result =
            Cli::try_parse_from(["tekops", "beacon", "duties", "attester", "--epoch", "1"]);
        assert!(result.is_err());
    }

    #[test]
    fn log_level_requires_a_level_argument() {
        let result = Cli::try_parse_from(["tekops", "log-level"]);
        assert!(result.is_err());
    }

    #[test]
    fn log_level_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "log-level", "DEBUG"]).unwrap();
        match cli.command {
            Commands::LogLevel {
                level,
                log_filter,
                api,
            } => {
                assert_eq!(level, "DEBUG");
                assert!(log_filter.is_empty());
                assert_eq!(api.api_url, None);
                assert!(!api.json);
            }
            _ => panic!("expected LogLevel command"),
        }
    }

    #[test]
    fn log_level_parses_with_multiple_filters() {
        let cli = Cli::try_parse_from([
            "tekops",
            "log-level",
            "DEBUG",
            "--filter",
            "org.a",
            "--filter",
            "org.b",
        ])
        .unwrap();
        match cli.command {
            Commands::LogLevel {
                level, log_filter, ..
            } => {
                assert_eq!(level, "DEBUG");
                assert_eq!(log_filter, vec!["org.a".to_string(), "org.b".to_string()]);
            }
            _ => panic!("expected LogLevel command"),
        }
    }

    /// The `beacon` flags were declared `global` before they were flattened
    /// into `ApiArgs`; without that, a flag after the subcommand stops parsing.
    #[test]
    fn beacon_subcommand_accepts_flags_after_the_subcommand() {
        let cli = Cli::try_parse_from(["tekops", "beacon", "validators", "1", "--json"]).unwrap();
        match cli.command {
            Commands::Beacon { api, .. } => assert!(api.json),
            _ => panic!("expected Beacon command"),
        }
    }

    #[test]
    fn beacon_subcommand_accepts_flags_before_the_subcommand() {
        let cli = Cli::try_parse_from(["tekops", "beacon", "--json", "validators", "1"]).unwrap();
        match cli.command {
            Commands::Beacon { api, .. } => assert!(api.json),
            _ => panic!("expected Beacon command"),
        }
    }

    #[test]
    fn api_url_flag_still_reaches_each_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "health", "--api-url", "http://x:1/"]).unwrap();
        match cli.command {
            Commands::Health { api } => assert_eq!(api.api_url.as_deref(), Some("http://x:1/")),
            _ => panic!("expected Health command"),
        }
    }

    #[test]
    fn metric_url_flag_still_reaches_each_metrics_command() {
        let cli =
            Cli::try_parse_from(["tekops", "duties", "--metric-url", "http://x:2/m"]).unwrap();
        match cli.command {
            Commands::Duties { metrics } => {
                assert_eq!(metrics.metric_url.as_deref(), Some("http://x:2/m"))
            }
            _ => panic!("expected Duties command"),
        }
    }

    /// `--version` reports the tekops build; `tekops version` reports the
    /// running Teku's. Both must exist, and they answer different questions.
    #[test]
    fn version_flag_reports_the_tekops_build() {
        let err = match Cli::try_parse_from(["tekops", "--version"]) {
            Err(e) => e,
            Ok(_) => panic!("--version should short-circuit parsing"),
        };
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        let rendered = err.to_string();
        assert!(
            rendered.contains(env!("CARGO_PKG_VERSION")),
            "got {rendered:?}"
        );
    }

    #[test]
    fn version_subcommand_is_still_the_teku_version_query() {
        let cli = Cli::try_parse_from(["tekops", "version"]).unwrap();
        assert!(matches!(cli.command, Commands::Version { .. }));
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
        let payload = HealthJson {
            health: &health,
            syncing: &syncing,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["health"], "ready");
        assert_eq!(value["is_syncing"], true);
        assert_eq!(value["head_slot"], "123");
    }

    #[test]
    fn head_json_output_is_valid_json() {
        let header = BlockHeader {
            slot: "999".to_string(),
            root: "0xabc".to_string(),
        };
        let finality = FinalityCheckpoints {
            previous_justified_epoch: "10".to_string(),
            current_justified_epoch: "11".to_string(),
            finalized_epoch: "9".to_string(),
        };
        let payload = HeadJson {
            header: &header,
            finality: &finality,
        };
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
    fn validator_metrics_json_output_is_valid_json() {
        use crate::metrics::ValidatorMetrics;
        use std::collections::BTreeMap;
        let mut counts_by_status = BTreeMap::new();
        counts_by_status.insert("active_ongoing".to_string(), 100);
        let metrics = ValidatorMetrics {
            counts_by_status,
            total_eth: 63.5,
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["counts_by_status"]["active_ongoing"], 100);
        assert_eq!(value["total_eth"], 63.5);
    }

    #[test]
    fn version_json_output_is_valid_json() {
        use crate::metrics::VersionInfo;
        let info = VersionInfo {
            versions: vec!["teku/v24.9.0".to_string()],
        };
        let json = serde_json::to_string(&info).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["versions"][0], "teku/v24.9.0");
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

    #[test]
    fn log_level_json_output_includes_filter_when_scoped() {
        let payload = LogLevelJson {
            level: "DEBUG",
            log_filter: Some(vec!["org.example".to_string()]),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["level"], "DEBUG");
        assert_eq!(value["log_filter"][0], "org.example");
    }

    #[test]
    fn log_level_json_output_omits_filter_when_global() {
        let payload = LogLevelJson {
            level: "DEBUG",
            log_filter: None,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["level"], "DEBUG");
        assert!(value.get("log_filter").is_none());
    }

    #[test]
    fn update_takes_no_argument() {
        let cli = Cli::try_parse_from(["tekops", "update"]).unwrap();
        match cli.command {
            Commands::Update { target, json, yes } => {
                assert_eq!(target, None);
                assert!(!json);
                assert!(!yes);
            }
            _ => panic!("expected Update"),
        }
    }

    /// `check` and `latest` are positional values, not subcommands. Declaring
    /// them as subcommands alongside an optional positional version is the
    /// same ambiguity that broke `tekops logs /var/log/x.log`.
    #[test]
    fn update_accepts_check_latest_and_a_version() {
        for arg in ["check", "latest", "0.3.0", "v0.3.0"] {
            let cli = Cli::try_parse_from(["tekops", "update", arg]).unwrap();
            match cli.command {
                Commands::Update { target, .. } => assert_eq!(target.as_deref(), Some(arg)),
                _ => panic!("expected Update for {arg}"),
            }
        }
    }

    #[test]
    fn update_accepts_its_flags() {
        let cli = Cli::try_parse_from(["tekops", "update", "check", "--json"]).unwrap();
        match cli.command {
            Commands::Update { json, .. } => assert!(json),
            _ => panic!("expected Update"),
        }
        let cli = Cli::try_parse_from(["tekops", "update", "-y"]).unwrap();
        match cli.command {
            Commands::Update { yes, .. } => assert!(yes),
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn update_rejects_a_second_positional() {
        assert!(Cli::try_parse_from(["tekops", "update", "check", "0.3.0"]).is_err());
    }

    #[test]
    fn url_precedence_is_flag_then_env_then_stack_then_bare_metal() {
        // Flag beats a stack profile.
        assert_eq!(
            resolve_base_url(Some("http://x:1".into()), Some(Stack::RocketPool)),
            "http://x:1"
        );
        // Stack profile beats the bare-metal default.
        assert_eq!(
            resolve_base_url(None, Some(Stack::RocketPool)),
            "http://localhost:5052"
        );
        // Nothing at all keeps today's behaviour.
        assert_eq!(resolve_base_url(None, None), "http://localhost:5051");
    }

    #[test]
    fn metric_url_precedence_matches_the_api_url_ladder() {
        assert_eq!(
            resolve_metric_url(Some("http://x:2/m".into()), Some(Stack::EthDocker)),
            "http://x:2/m"
        );
        assert_eq!(
            resolve_metric_url(None, Some(Stack::EthDocker)),
            "http://localhost:8009/metrics"
        );
        assert_eq!(
            resolve_metric_url(None, None),
            "http://localhost:8010/metrics"
        );
    }

    /// Mixing is explicitly supported: a profile is one layer in the chain, not
    /// a mode that locks the other values.
    #[test]
    fn a_stack_profile_and_an_explicit_url_can_be_combined() {
        assert_eq!(
            resolve_base_url(Some("http://custom:5099".into()), Some(Stack::RocketPool)),
            "http://custom:5099"
        );
        assert_eq!(
            resolve_metric_url(None, Some(Stack::RocketPool)),
            "http://localhost:9101/metrics"
        );
    }

    #[test]
    fn stack_flag_beats_the_environment() {
        assert_eq!(
            resolve_stack(Some(Stack::EthDocker), Some("rocketpool".into())),
            Some(Stack::EthDocker)
        );
        assert_eq!(
            resolve_stack(None, Some("rocketpool".into())),
            Some(Stack::RocketPool)
        );
        assert_eq!(resolve_stack(None, None), None);
    }

    /// An unparseable $TEKOPS_STACK is ignored rather than fatal: it must not
    /// break commands that would have worked without it.
    #[test]
    fn an_unknown_stack_env_value_is_ignored() {
        assert_eq!(resolve_stack(None, Some("nonsense".into())), None);
    }

    #[test]
    fn container_flag_reaches_the_logs_command() {
        let cli = Cli::try_parse_from(["tekops", "logs", "--container", "mynode"]).unwrap();
        match cli.command {
            Commands::Logs { container, .. } => {
                assert_eq!(container.as_deref(), Some("mynode"))
            }
            _ => panic!("expected Logs"),
        }
    }

    /// A path and a container name are two answers to one question, so clap
    /// rejects them together rather than leaving a precedence rule to invent.
    /// Both positionals are guarded: a lone path arrives in `source`, not
    /// `path`, so guarding only the latter would miss the common spelling.
    #[test]
    fn a_path_and_a_container_cannot_both_be_given() {
        assert!(
            Cli::try_parse_from(["tekops", "logs", "/a.log", "--container", "c"]).is_err(),
            "a lone positional path lands in `source` and must still conflict"
        );
        assert!(
            Cli::try_parse_from(["tekops", "logs", "teku", "/a.log", "--container", "c"]).is_err(),
            "both positionals given must also conflict"
        );
    }

    #[test]
    fn stack_flag_reaches_each_api_and_metric_command() {
        let cli = Cli::try_parse_from(["tekops", "health", "--stack", "rocketpool"]).unwrap();
        match cli.command {
            Commands::Health { api } => assert_eq!(api.stack, Some(Stack::RocketPool)),
            _ => panic!("expected Health"),
        }
        let cli = Cli::try_parse_from(["tekops", "duties", "--stack", "eth-docker"]).unwrap();
        match cli.command {
            Commands::Duties { metrics } => assert_eq!(metrics.stack, Some(Stack::EthDocker)),
            _ => panic!("expected Duties"),
        }
    }

    #[test]
    fn hint_names_the_detected_stack_and_the_flag_that_fixes_it() {
        let ps = "rocketpool_node\nrocketpool_eth2\n";
        let hint = hint_from_ps(ps).expect("expected a hint");
        assert!(hint.contains("rocketpool"), "got: {hint}");
        assert!(hint.contains("--stack"), "got: {hint}");
    }

    #[test]
    fn no_hint_when_no_docker_stack_is_present() {
        assert_eq!(hint_from_ps("postgres\nredis\n"), None);
    }

    #[test]
    fn no_hint_when_detection_is_ambiguous() {
        // Suggesting one of two stacks would be a guess, and the operator is
        // better served by the plain connection error.
        assert_eq!(
            hint_from_ps("eth-docker-consensus-1\nrocketpool_eth2\n"),
            None
        );
    }
}
