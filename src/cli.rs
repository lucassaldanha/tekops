use crate::beaconapi::{
    BeaconClient, BlockHeader, FinalityCheckpoints, HealthState, SyncingStatus,
};
use crate::completions::{self, detect_shell, CompletionError, RcOutcome};
use crate::http::ApiError;
use crate::logs::{resolve_log_path, resolve_logs_target, stream_logs, LogSource};
use crate::metrics::MetricsClient;
use crate::output::{
    format_about, format_attester_duties, format_duties_table, format_head_table,
    format_health_table, format_peers_table, format_proposer_duties,
    format_validator_metrics_table, format_validators_table, format_version_table, PeerRow,
};
use crate::protocol::classify_protocol;
use crate::update::{self, resolve_update_target, UpdateError, UpdateTarget};
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::env;
use std::fmt;
use std::io::{self, BufRead, BufReader, LineWriter, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::thread;

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
    /// Print a JSON-serialized summary instead of a formatted table
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Tail and colorize a Teku or Besu JSON log file (defaults to teku)
    Logs {
        /// teku, besu, or a path to a log file (defaults to teku)
        source: Option<String>,
        path: Option<PathBuf>,
        /// Lines of existing log to show before following new output
        #[arg(short = 'n', long = "lines", default_value_t = 500)]
        lines: u32,
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
        } => match resolve_logs_target(source, path) {
            Ok((source, path)) => run_logs(source, path, lines),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Commands::Beacon { command, api } => {
            let client = BeaconClient::new(resolve_base_url(api.api_url));
            run_beacon(client, command, api.json)
        }
        Commands::Peers { api } => {
            let client = BeaconClient::new(resolve_base_url(api.api_url));
            exit_for(beacon_peers(&client, api.json))
        }
        Commands::Health { api } => {
            let client = BeaconClient::new(resolve_base_url(api.api_url));
            exit_for(beacon_health(&client, api.json))
        }
        Commands::Head { api } => {
            let client = BeaconClient::new(resolve_base_url(api.api_url));
            exit_for(beacon_head(&client, api.json))
        }
        Commands::Duties { metrics } => {
            let client = MetricsClient::new(resolve_metric_url(metrics.metric_url));
            exit_for(metrics_duties(&client, metrics.json))
        }
        Commands::Validators { metrics } => {
            let client = MetricsClient::new(resolve_metric_url(metrics.metric_url));
            exit_for(metrics_validators(&client, metrics.json))
        }
        Commands::Version { metrics } => {
            let client = MetricsClient::new(resolve_metric_url(metrics.metric_url));
            exit_for(metrics_version(&client, metrics.json))
        }
        Commands::LogLevel {
            level,
            log_filter,
            api,
        } => {
            let client = BeaconClient::new(resolve_base_url(api.api_url));
            exit_for(beacon_log_level(&client, &level, log_filter, api.json))
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

fn resolve_base_url(api_url: Option<String>) -> String {
    api_url
        .or_else(|| env::var("TEKOPS_API_URL").ok())
        .unwrap_or_else(|| "http://localhost:5051".to_string())
}

fn resolve_metric_url(metric_url: Option<String>) -> String {
    metric_url
        .or_else(|| env::var("TEKOPS_METRIC_URL").ok())
        .unwrap_or_else(|| "http://localhost:8010/metrics".to_string())
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
    exit_for(result)
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

fn run_logs(source: LogSource, path: Option<PathBuf>, lines: u32) -> ExitCode {
    let path = resolve_log_path(source, path, env::var("TEKOPS_LOGS_FILE").ok());
    if !path.exists() {
        eprintln!("error: log file not found: {}", path.display());
        return ExitCode::FAILURE;
    }

    // The terminal delivers Ctrl+C to the whole foreground process group. `less`
    // is designed to catch it and drop out of follow mode, but tekops itself has
    // no handler and would otherwise die right along with it, tearing down the
    // pager it's supervising. Ignoring it here lets tekops outlive the keypress;
    // `tail` is put in its own process group so the same Ctrl+C doesn't kill it
    // too, which would otherwise permanently break `less`'s `F` (resume follow).
    //
    // The `termination` feature extends this same ignore to SIGTERM/SIGHUP (e.g.
    // a dropped SSH session). Unlike SIGINT, `less` has no special handling for
    // those and just dies normally, so `pager.wait()` below still returns and
    // the existing cleanup (kill `tail`, let the temp file's Drop run) still
    // executes. Without this, tekops and `less` would die immediately alongside
    // the signal, skipping that cleanup entirely: `tail` (isolated into its own
    // process group above, specifically so Ctrl+C can't reach it) would be
    // orphaned and keep running forever, and the temp file backing `less` would
    // never be removed - a real leak, one per dropped session, not hypothetical.
    // Failing to install this is not cosmetic: it silently reverts the process
    // to the exact behaviour the handler exists to prevent - Ctrl+C kills
    // tekops mid-session, leaving `tail` (deliberately in its own process
    // group, out of the signal's reach) orphaned forever and the temp file
    // below undeleted. Swallowing the error hides a guaranteed leak, so say so
    // and bail rather than starting a session that can't clean up after itself.
    if let Err(e) = ctrlc::set_handler(|| {}) {
        eprintln!("error: could not install signal handler: {e}");
        eprintln!("refusing to start: Ctrl+C or a dropped session would orphan the log tailer");
        return ExitCode::FAILURE;
    }

    let mut tail = match Command::new("tail")
        .args(["-F", "-n"])
        .arg(lines.to_string())
        .arg(&path)
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("error: failed to spawn tail: {e}");
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
            let _ = tail.kill();
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
            let _ = tail.kill();
            return ExitCode::FAILURE;
        }
    };

    // Both of these can fail for real (fd exhaustion, /tmp remounted read-only)
    // and both happen with the pager already on screen, so a panic here would
    // dump a Rust backtrace over a live `less` and skip the cleanup below.
    let Some(stdout) = tail.stdout.take() else {
        eprintln!("error: tail stdout was not piped");
        let _ = pager.kill();
        let _ = tail.kill();
        return ExitCode::FAILURE;
    };
    let writer = match sink.reopen() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: failed to open temp file for writing: {e}");
            let _ = pager.kill();
            let _ = tail.kill();
            return ExitCode::FAILURE;
        }
    };
    match supervise_pager(pager, tail, BufReader::new(stdout), LineWriter::new(writer)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error while streaming logs: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the streaming loop against `reader` while the pager owns process
/// lifetime, then tears the tailer down.
///
/// The ordering here is the whole point, and it has regressed before. `tail -F`
/// never reaches EOF, so the streaming loop cannot be what ends the process -
/// blocking the main thread on it hangs until the log happens to move, which on
/// a quiet node is forever. So: stream on a worker thread, block the main
/// thread on the pager, and only once the pager has exited kill the tailer.
/// Killing it is what closes the pipe and gives the worker its EOF; joining
/// before the kill deadlocks instead.
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
    use crate::beaconapi::{AttesterDuty, ProposerDuty, ValidatorInfo};
    use crate::protocol::Protocol;

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
        let code = run_logs(LogSource::Teku, Some(missing), 500);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::FAILURE));
    }

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
}
