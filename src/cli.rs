use crate::beaconapi::{
    BeaconClient, BlockHeader, FinalityCheckpoints, HealthState, SyncingStatus,
};
use crate::completions::{self, detect_shell, CompletionError, RcOutcome};
use crate::curl;
use crate::http::ApiError;
use crate::loglevel::{
    self, resolve_log_level_target, LogLevelError, LogLevelSpec, LogLevelTarget,
};
use crate::logs::run_logs;
use crate::metrics::MetricsClient;
use crate::output::{
    format_about, format_doctor_report, format_duties_table, format_head_table,
    format_health_table, format_log_level_preview, format_peers_table,
    format_validator_metrics_table, format_version_table, PeerRow,
};
use crate::protocol::classify_protocol;
use crate::stack::{detect_stack, detect_validator_stack, DetectError, Stack};
use crate::term::sanitize;
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
    about = "Helper tools for operating a Teku node",
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
#[derive(clap::Args)]
struct ApiArgs {
    /// Beacon API base URL (default: http://localhost:5051, or $TEKOPS_API_URL)
    #[arg(long)]
    api_url: Option<String>,
    /// Deployment to take port defaults from (or $TEKOPS_STACK)
    #[arg(long)]
    stack: Option<Stack>,
    /// Print a JSON-serialized summary instead of a formatted table
    #[arg(long)]
    json: bool,
}

/// The Prometheus scrape flags for `version`, the one command that reads
/// both processes.
///
/// `Duties` and `Validators` flatten `VcMetricArgs` instead: the beacon-node
/// spellings here (`--bn-metric-url` and the legacy `--metric-url`) name an
/// endpoint those two commands never read, so accepting them would mean
/// parsing successfully and doing nothing - clap rejects them there rather
/// than silently discarding them.
#[derive(clap::Args)]
struct MetricArgs {
    /// Beacon node Prometheus metrics URL (or $TEKOPS_BN_METRIC_URL)
    #[arg(long)]
    bn_metric_url: Option<String>,
    /// Validator client Prometheus metrics URL (or $TEKOPS_VC_METRIC_URL)
    #[arg(long)]
    vc_metric_url: Option<String>,
    /// Deprecated alias for --bn-metric-url (or $TEKOPS_METRIC_URL)
    //
    // Hidden rather than removed: existing scripts and rc files still say it.
    // `conflicts_with` because the two name one endpoint, so accepting both
    // would mean silently honouring one and dropping the other.
    #[arg(long, hide = true, conflicts_with = "bn_metric_url")]
    metric_url: Option<String>,
    /// Deployment to take port defaults from (or $TEKOPS_STACK)
    #[arg(long)]
    stack: Option<Stack>,
    /// Print a JSON-serialized summary instead of a formatted table
    #[arg(long)]
    json: bool,
}

/// The Prometheus scrape flags for `duties` and `validators`, which read only
/// the validator client and have no use for a beacon node URL.
///
/// This is deliberately not `MetricArgs`: that struct also accepts
/// `--bn-metric-url` and the legacy `--metric-url`, and before this branch
/// the legacy spelling was *the* way to steer these two commands. Now that it
/// means the beacon node (matching `--api-url`), silently reading it here
/// would scrape the wrong process while exiting zero. Rejecting it is the
/// better failure: clap's error names `--vc-metric-url`, which is exactly the
/// migration an old script or rc file needs.
#[derive(clap::Args)]
struct VcMetricArgs {
    /// Validator client Prometheus metrics URL (or $TEKOPS_VC_METRIC_URL)
    #[arg(long)]
    vc_metric_url: Option<String>,
    /// Deployment to take port defaults from (or $TEKOPS_STACK)
    #[arg(long)]
    stack: Option<Stack>,
    /// Print a JSON-serialized summary instead of a formatted table
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Tail and colorize the Teku log, or a container's logs
    Logs {
        /// Path to a log file (defaults to the Teku log)
        //
        // Naming a container fully determines what gets read, so a path
        // alongside it is meaningless.
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
    /// Write the last N log lines to a shareable file or gist, anonymised
    DumpLogs {
        /// Path to a log file (defaults to the Teku log, or a detected container)
        #[arg(conflicts_with = "container")]
        path: Option<PathBuf>,
        /// How many lines to dump
        #[arg(short = 'n', long = "lines", default_value_t = 1000)]
        lines: u32,
        /// Write the dump here (default: ./tekops-dump-<timestamp>.txt)
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
        /// Upload to a secret GitHub gist (needs $GITHUB_TOKEN or $GH_TOKEN)
        #[arg(long)]
        gist: bool,
        /// Upload without asking for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
        /// Prepend a provenance block naming the build, stack and source
        #[arg(long)]
        header: bool,
        /// Run the doctor checks and embed the report above the logs
        #[arg(long)]
        doctor: bool,
        /// Docker container to read logs from (or $TEKOPS_CONTAINER)
        #[arg(long)]
        container: Option<String>,
        /// Deployment to narrow container detection to (or $TEKOPS_STACK)
        #[arg(long)]
        stack: Option<Stack>,
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
        metrics: VcMetricArgs,
    },
    /// Validator key counts by status, and total locally-stated ETH balance
    Validators {
        #[command(flatten)]
        metrics: VcMetricArgs,
    },
    /// Running Teku version, read from both the beacon node and the
    /// validator client metrics, reported as two independent rows
    Version {
        #[command(flatten)]
        metrics: MetricArgs,
    },
    /// Set the node's runtime log level, optionally scoped to specific loggers
    LogLevel {
        /// Log level to set (e.g. INFO, DEBUG, WARN, TRACE), or an https url
        /// serving a prepared request body (e.g. a gist)
        target: String,
        /// Logger name(s) to scope the change to (e.g. tech.pegasys.teku.networking);
        /// omit to change the global level. Not valid with a url, which
        /// carries its own filters
        #[arg(long = "filter")]
        log_filter: Vec<String>,
        /// Apply a fetched body without asking for confirmation
        #[arg(short = 'y', long)]
        yes: bool,
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
    /// Run a series of checks across the node and report what needs attention
    Doctor {
        #[command(flatten)]
        api: ApiArgs,
        /// Beacon node Prometheus metrics URL (or $TEKOPS_BN_METRIC_URL)
        //
        // Declared here rather than by flattening `MetricArgs`: that struct
        // also declares `stack` and `json`, and flattening both is a duplicate
        // arg id, which clap turns into a panic at startup.
        #[arg(long)]
        bn_metric_url: Option<String>,
        /// Validator client Prometheus metrics URL (or $TEKOPS_VC_METRIC_URL)
        #[arg(long)]
        vc_metric_url: Option<String>,
        /// Deprecated alias for --bn-metric-url (or $TEKOPS_METRIC_URL)
        #[arg(long, hide = true, conflicts_with = "bn_metric_url")]
        metric_url: Option<String>,
        /// Filesystem to check for free space (or $TEKOPS_DATA_DIR)
        //
        // Only consulted on bare-metal, where nothing else can answer. On a
        // Docker stack the container's own mounts do, and a value here
        // overrides them, matching the repo's flags-beat-detection rule.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

pub fn run() -> ExitCode {
    let cli = Cli::parse();

    // Loaded once, before dispatch, so every command sees the same config and
    // a broken file is reported by whichever command the operator runs first.
    // "The config file is broken" is a fact about the installation rather than
    // about one command, so `about` and `update` fail on it too even though
    // neither reads any of the six keys. The alternative, loading lazily per
    // command, makes the same typo invisible until it is maximally confusing.
    //
    // This does not go through `exit_for_api`: its `--stack` hint points at
    // the node's ports, and this failure happened in a file. Same distinction
    // `run_log_level` draws for a bad fetched body.
    let cfg = match config_from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    // `Option<Stack>` is `Copy`, so this sidesteps borrowing `cfg` across the
    // whole `match cli.command` block below; `cfg` itself is still borrowed
    // where a handler needs more than the stack (`run_doctor`).
    let cfg_stack = cfg.stack;

    match cli.command {
        Commands::Logs {
            path,
            lines,
            container,
            stack,
        } => {
            // Detection only runs when nothing else has answered - see
            // `needs_detection`.
            let container_env = env::var("TEKOPS_CONTAINER").ok();
            let logs_file_env = env::var("TEKOPS_LOGS_FILE").ok();
            let detected = if needs_detection(
                path.as_ref(),
                container.as_ref(),
                container_env.as_ref(),
                logs_file_env.as_ref(),
                &cfg,
            ) {
                let only = resolve_stack(stack, env::var("TEKOPS_STACK").ok(), cfg_stack);
                docker_ps_names(should_report_unaskable_docker(only))
                    .and_then(|ps| detect_or_note(&ps, only).map(|(_, name)| name))
            } else {
                None
            };
            run_logs(
                path,
                lines,
                container,
                cfg.container.clone(),
                cfg.logs_file.clone(),
                detected,
            )
        }
        Commands::DumpLogs {
            path,
            lines,
            output,
            gist,
            yes,
            header,
            doctor,
            container,
            stack,
        } => {
            // The same detection gate `logs` uses - see `needs_detection`.
            let logs_file_env = env::var("TEKOPS_LOGS_FILE").ok();
            let container_env = env::var("TEKOPS_CONTAINER").ok();
            let given_stack = resolve_stack(stack, env::var("TEKOPS_STACK").ok(), cfg_stack);
            let detected: Option<(Stack, String)> = if needs_detection(
                path.as_ref(),
                container.as_ref(),
                container_env.as_ref(),
                logs_file_env.as_ref(),
                &cfg,
            ) {
                docker_ps_names(should_report_unaskable_docker(given_stack))
                    .and_then(|ps| detect_or_note(&ps, given_stack))
            } else {
                None
            };
            // Unlike `logs`, which only ever wants the container name, the
            // header reports which stack this came from. Detection already
            // knows, so a detected stack beats "unknown" - the same
            // flag > env > config > detection ladder the rest of the command
            // uses.
            let resolved_stack = given_stack.or(detected.as_ref().map(|(s, _)| *s));
            let target = crate::logs::resolve_log_target(
                path,
                container,
                container_env,
                logs_file_env,
                cfg.container.clone(),
                cfg.logs_file.clone(),
                detected.map(|(_, name)| name),
            );
            // `--doctor` runs its own `docker ps` through `doctor_probe_config`
            // rather than reusing the detection above: doctor's ladder also
            // needs the container name and the two URLs, and duplicating that
            // here would be a second approximation of the config `doctor`
            // itself builds. The extra spawn is paid only with `--doctor`.
            let probe = doctor
                .then(|| doctor_probe_config(stack, None, MetricUrlFlags::default(), None, &cfg));
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            exit_for(crate::dump::run_dump(crate::dump::DumpConfig {
                target,
                lines,
                output,
                gist,
                yes,
                header,
                stack: resolved_stack,
                doctor: probe,
                now,
            }))
        }
        Commands::Peers { api } => {
            let stack = resolve_stack(api.stack, env::var("TEKOPS_STACK").ok(), cfg_stack);
            let client = BeaconClient::new(resolve_base_url(
                api.api_url,
                env::var("TEKOPS_API_URL").ok(),
                cfg.api_url.clone(),
                stack,
            ));
            exit_for_api(beacon_peers(&client, api.json), stack)
        }
        Commands::Health { api } => {
            let stack = resolve_stack(api.stack, env::var("TEKOPS_STACK").ok(), cfg_stack);
            let client = BeaconClient::new(resolve_base_url(
                api.api_url,
                env::var("TEKOPS_API_URL").ok(),
                cfg.api_url.clone(),
                stack,
            ));
            exit_for_api(beacon_health(&client, api.json), stack)
        }
        Commands::Head { api } => {
            let stack = resolve_stack(api.stack, env::var("TEKOPS_STACK").ok(), cfg_stack);
            let client = BeaconClient::new(resolve_base_url(
                api.api_url,
                env::var("TEKOPS_API_URL").ok(),
                cfg.api_url.clone(),
                stack,
            ));
            exit_for_api(beacon_head(&client, api.json), stack)
        }
        Commands::Duties { metrics } => {
            let stack = resolve_stack(metrics.stack, env::var("TEKOPS_STACK").ok(), cfg_stack);
            let client = MetricsClient::new(resolve_vc_metric_url(
                metrics.vc_metric_url,
                env::var("TEKOPS_VC_METRIC_URL").ok(),
                cfg.vc_metric_url.clone(),
                stack,
            ));
            exit_for_api(metrics_duties(&client, metrics.json), stack)
        }
        Commands::Validators { metrics } => {
            let stack = resolve_stack(metrics.stack, env::var("TEKOPS_STACK").ok(), cfg_stack);
            let client = MetricsClient::new(resolve_vc_metric_url(
                metrics.vc_metric_url,
                env::var("TEKOPS_VC_METRIC_URL").ok(),
                cfg.vc_metric_url.clone(),
                stack,
            ));
            exit_for_api(metrics_validators(&client, metrics.json), stack)
        }
        Commands::Version { metrics } => {
            let stack = resolve_stack(metrics.stack, env::var("TEKOPS_STACK").ok(), cfg_stack);
            let bn_url = resolve_bn_metric_url(
                metrics.bn_metric_url,
                metrics.metric_url,
                env::var("TEKOPS_BN_METRIC_URL").ok(),
                env::var("TEKOPS_METRIC_URL").ok(),
                cfg.bn_metric_url.clone(),
                cfg.metric_url.clone(),
                stack,
            );
            let vc_url = resolve_vc_metric_url(
                metrics.vc_metric_url,
                env::var("TEKOPS_VC_METRIC_URL").ok(),
                cfg.vc_metric_url.clone(),
                stack,
            );
            exit_for_api(metrics_version(&bn_url, &vc_url, metrics.json), stack)
        }
        Commands::LogLevel {
            target,
            log_filter,
            yes,
            api,
        } => run_log_level(target, log_filter, yes, api, &cfg),
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
        Commands::Doctor {
            api,
            bn_metric_url,
            vc_metric_url,
            metric_url,
            data_dir,
        } => run_doctor(
            api,
            MetricUrlFlags {
                bn: bn_metric_url,
                vc: vc_metric_url,
                legacy: metric_url,
            },
            data_dir,
            &cfg,
        ),
    }
}

/// Doctor's stack ladder, which differs from every other API command's.
///
/// The others read the flag, the env var, and the config file, then suggest
/// `--stack` in a hint when the endpoint turns out to be unreachable. Doctor
/// is already running `docker ps` for the container checks, so it adds a
/// detection rung the others lack, applied to the URLs as well:
/// `flag > env > config > detection > default`, the same ladder `logs` uses.
/// Because detection has already been applied, doctor does not print the
/// hint; there would be nothing left for it to suggest.
///
/// The ladder terminates in `Stack::BareMetal` rather than `None`:
/// `resolve_base_url`/`resolve_vc_metric_url` already default to bare-metal
/// independently when handed `None`, so leaving this rung off let the report
/// header say "unknown stack" directly above two confidently bare-metal URLs -
/// two different facts that must not disagree.
///
/// Called once per process, with that process's own detection. A stated stack
/// applies to both: an operator who says `--stack rocketpool` has described
/// their deployment, and detection is the rung below, not above.
fn resolve_doctor_stack(stated: Option<Stack>, detected: Option<Stack>) -> Option<Stack> {
    stated.or(detected).or(Some(Stack::BareMetal))
}

/// The validator client's stack ladder: the beacon node's, plus one rung.
///
/// The extra rung is a consensus container found under some stack, and the
/// asymmetry with `resolve_doctor_stack` is the point, because the two
/// absences mean opposite things.
///
/// A validator container with no consensus container beside it is a beacon
/// node running somewhere else - Rocket Pool's External Consensus Client mode,
/// and Eth Docker's equivalent - so the beacon node's own ladder is right to
/// terminate in bare-metal. A consensus container with no validator beside it
/// is the ordinary combined deployment, where Teku runs both processes in the
/// one container: that container's stack is the validator's stack too, and
/// terminating in bare-metal here would hand an eth-docker node the bare-metal
/// validator metrics port instead of its own.
///
/// Extracted rather than inlined at the one call site for the same reason as
/// `doctor_needs_docker_ps`: it is a precedence rule guarding a silent wrong
/// answer, and `doctor_probe_config` cannot be tested without Docker.
fn resolve_doctor_vc_stack(
    stated: Option<Stack>,
    detected_vc: Option<Stack>,
    detected_bn: Option<Stack>,
) -> Option<Stack> {
    resolve_doctor_stack(stated, detected_vc.or(detected_bn))
}

/// Teku's conventional bare-metal data directory. `--data-dir` (or
/// `$TEKOPS_DATA_DIR`, or the config file's `data_dir`) exists for a node
/// that relocated it.
const DEFAULT_BARE_METAL_DATA_DIR: &str = "/var/lib/teku";

/// The disk-free check's data-dir ladder: `--data-dir` > `$TEKOPS_DATA_DIR` >
/// the config file's `data_dir` > a bare-metal default.
///
/// The default is gated to bare-metal specifically. `doctor::probe` layers a
/// container-mount fallback beneath whatever this returns, so handing down a
/// value unconditionally on a Docker stack would win over that mount and make
/// eth-docker/rocketpool measure the wrong filesystem. On those stacks this
/// stays `None` unless the operator stated a path - by flag, variable, or
/// config file, all three of which count as stating one.
fn resolve_doctor_data_dir(
    flag: Option<PathBuf>,
    env: Option<String>,
    cfg: Option<PathBuf>,
    stack: Option<Stack>,
) -> Option<PathBuf> {
    flag.or_else(|| env.map(PathBuf::from)).or(cfg).or_else(|| {
        matches!(stack, Some(Stack::BareMetal)).then(|| PathBuf::from(DEFAULT_BARE_METAL_DATA_DIR))
    })
}

fn exit_for_findings(findings: &[crate::doctor::Finding]) -> ExitCode {
    if findings
        .iter()
        .any(|f| f.status == crate::doctor::Status::Fail)
    {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Whether `doctor_probe_config` has anything to gain from running `docker ps`.
///
/// False for exactly one answer: bare-metal, stated outright by flag,
/// environment or config file. That operator has said there are no containers,
/// so there is neither a stack left to detect nor a container left to name.
///
/// Every other case spawns, a stated Docker stack included - stating
/// `--stack rocketpool` fixes the ports but not the container names, which are
/// only knowable from `docker ps` because both stacks let the operator rename
/// the project prefix. Extracted, the same shape `logs::needs_detection` uses
/// for the identical failure mode, so the bare-metal spawn-skip is directly
/// testable without mocking Docker.
fn doctor_needs_docker_ps(stated: Option<Stack>) -> bool {
    stated != Some(Stack::BareMetal)
}

/// The three spellings of the metrics endpoints, as given on the command line.
///
/// A struct rather than three more positional parameters: `doctor_probe_config`
/// already takes five, and `dump-logs --doctor` passes nothing for any of them.
#[derive(Default)]
struct MetricUrlFlags {
    bn: Option<String>,
    vc: Option<String>,
    legacy: Option<String>,
}

/// Doctor's full probe configuration, `docker ps` detection included.
///
/// Lifted out of `run_doctor` so `dump-logs --doctor` builds the identical
/// config rather than a second approximation of it. Doctor's stack ladder is
/// deliberately different from every other API command's - it applies
/// detection to the URLs too, because it is already running `docker ps` for
/// the container checks and detection costs nothing at that point. Two callers
/// now depend on that being true in exactly one place.
fn doctor_probe_config(
    stack_flag: Option<Stack>,
    api_url: Option<String>,
    metrics: MetricUrlFlags,
    data_dir: Option<PathBuf>,
    cfg: &crate::config::Config,
) -> crate::doctor::ProbeConfig {
    // Read once, used for both `doctor_needs_docker_ps` (below) and the two
    // stack ladders.
    let stack_env = env::var("TEKOPS_STACK").ok();
    let stated = resolve_stack(stack_flag, stack_env, cfg.stack);

    // One `docker ps`, read for both roles. Detection is the rung beneath a
    // stated stack, so on the face of it a stated stack should skip the spawn
    // entirely - but the container names are only knowable from `docker ps`,
    // and doctor inspects both containers. The gate is therefore "is there
    // anything left to find", not "is the stack still unknown": bare-metal,
    // stated outright, is the one answer that makes the spawn pointless.
    //
    // When nothing is stated this rung is speculative by construction, which
    // is why `should_report_unaskable_docker(None)` is false: a `docker ps`
    // that cannot answer is not news to a host whose ladder is about to
    // terminate in bare-metal. That is issue #16.
    let ps: Option<String> = if doctor_needs_docker_ps(stated) {
        docker_ps_names(should_report_unaskable_docker(stated))
    } else {
        None
    };
    let detected = ps.as_deref().and_then(|ps| detect_or_note(ps, stated));
    let detected_vc = ps
        .as_deref()
        .and_then(|ps| detect_validator_or_note(ps, stated));

    // Two ladders, not one. The beacon node's stack answers the Beacon API,
    // its own metrics endpoint and the disk target; the validator client's
    // answers its metrics endpoint. On an all-in-one node they resolve to the
    // same value and nothing downstream can tell the difference.
    //
    // The validator's ladder has one rung the beacon node's does not - see
    // `resolve_doctor_vc_stack` for why the asymmetry is the correct one.
    //
    // `as_ref` here, not `map`: the tuple carries a String and is not Copy, so
    // consuming it would leave nothing for the container names below.
    let detected_stack = detected.as_ref().map(|(s, _)| *s);
    let stack = resolve_doctor_stack(stated, detected_stack);
    let vc_stack = resolve_doctor_vc_stack(
        stated,
        detected_vc.as_ref().map(|(s, _)| *s),
        detected_stack,
    );

    crate::doctor::ProbeConfig {
        stack,
        vc_stack,
        api_url: resolve_base_url(
            api_url,
            env::var("TEKOPS_API_URL").ok(),
            cfg.api_url.clone(),
            stack,
        ),
        bn_metric_url: resolve_bn_metric_url(
            metrics.bn,
            metrics.legacy,
            env::var("TEKOPS_BN_METRIC_URL").ok(),
            env::var("TEKOPS_METRIC_URL").ok(),
            cfg.bn_metric_url.clone(),
            cfg.metric_url.clone(),
            stack,
        ),
        vc_metric_url: resolve_vc_metric_url(
            metrics.vc,
            env::var("TEKOPS_VC_METRIC_URL").ok(),
            cfg.vc_metric_url.clone(),
            vc_stack,
        ),
        container: detected.map(|(_, name)| name),
        vc_container: detected_vc.map(|(_, name)| name),
        data_dir: resolve_doctor_data_dir(
            data_dir,
            env::var("TEKOPS_DATA_DIR").ok(),
            cfg.data_dir.clone(),
            stack,
        ),
    }
}

fn run_doctor(
    api: ApiArgs,
    metrics: MetricUrlFlags,
    data_dir: Option<PathBuf>,
    cfg: &crate::config::Config,
) -> ExitCode {
    let probe_cfg = doctor_probe_config(api.stack, api.api_url, metrics, data_dir, cfg);

    let facts = crate::doctor::probe(&probe_cfg);
    let findings = crate::doctor::evaluate(&facts);

    if api.json {
        let fails = findings
            .iter()
            .filter(|f| f.status == crate::doctor::Status::Fail)
            .count();
        let warns = findings
            .iter()
            .filter(|f| f.status == crate::doctor::Status::Warn)
            .count();
        let pass = findings.len() - fails - warns;
        let doc = serde_json::json!({
            "facts": facts,
            "findings": findings,
            "summary": { "pass": pass, "warn": warns, "fail": fails },
        });
        println!("{}", serde_json::to_string(&doc).unwrap_or_default());
    } else {
        print!("{}", format_doctor_report(&facts, &findings));
    }

    exit_for_findings(&findings)
}

/// The stack profile in force, if any. Flag beats `$TEKOPS_STACK` beats config.
///
/// An unparseable *environment* value is ignored rather than fatal: it is set
/// once in a shell rc and would otherwise break every command in the session,
/// including the ones that never needed it. A config file is the opposite kind
/// of object - a single deliberate artifact - so a bad value there is rejected
/// at parse time by `config::parse`, and nothing unparseable can reach here.
fn resolve_stack(flag: Option<Stack>, env: Option<String>, cfg: Option<Stack>) -> Option<Stack> {
    flag.or_else(|| env.and_then(|v| Stack::from_str(&v, true).ok()))
        .or(cfg)
}

/// The Beacon API base URL: flag, then `$TEKOPS_API_URL`, then config, then
/// the stack's default port.
///
/// The environment arrives as a parameter rather than being read here, the
/// same shape `resolve_stack` uses, so the ladder is testable without
/// environment races.
fn resolve_base_url(
    flag: Option<String>,
    env: Option<String>,
    cfg: Option<String>,
    stack: Option<Stack>,
) -> String {
    flag.or(env)
        .or(cfg)
        .unwrap_or_else(|| stack.unwrap_or(Stack::BareMetal).api_url().to_string())
}

/// The beacon node's Prometheus scrape URL.
///
/// The deprecated `--metric-url` / `$TEKOPS_METRIC_URL` / `metric_url` trio
/// feeds this ladder rather than the validator client's. The unprefixed
/// spelling means the beacon node because `--api-url` is likewise unprefixed
/// and likewise the beacon node's: the tool's primary subject gets the plain
/// name, and the validator client is the one that has to say so.
///
/// New spellings beat legacy ones *within* a tier and never across one, which
/// keeps the repo's flag > environment > config > default discipline intact.
///
/// This is a change of meaning for the deprecated trio, which used to resolve
/// to the validator client. `doctor`'s "metrics layout" check recognises a
/// config written under the old meaning and names the fix.
fn resolve_bn_metric_url(
    flag: Option<String>,
    legacy_flag: Option<String>,
    env: Option<String>,
    legacy_env: Option<String>,
    cfg: Option<String>,
    legacy_cfg: Option<String>,
    stack: Option<Stack>,
) -> String {
    flag.or(legacy_flag)
        .or(env)
        .or(legacy_env)
        .or(cfg)
        .or(legacy_cfg)
        .unwrap_or_else(|| {
            stack
                .unwrap_or(Stack::BareMetal)
                .bn_metric_url()
                .to_string()
        })
}

/// The validator client's Prometheus scrape URL.
///
/// No legacy rung: nothing reaches this but its own spellings and the stack
/// default. An operator migrating from the old `metric_url` has to say
/// `vc_metric_url` explicitly, which is the point.
fn resolve_vc_metric_url(
    flag: Option<String>,
    env: Option<String>,
    cfg: Option<String>,
    stack: Option<Stack>,
) -> String {
    flag.or(env).or(cfg).unwrap_or_else(|| {
        stack
            .unwrap_or(Stack::BareMetal)
            .vc_metric_url()
            .to_string()
    })
}

/// The names of running containers, or `None` if Docker cannot be asked.
///
/// Two different failures both end up `None`, and neither is reported here.
/// Docker being entirely absent (the `.output()` call itself errors - no
/// `docker` on $PATH) says nothing at all: a bare-metal host that never wanted
/// Docker must not be nagged about it. Docker being present but refusing (not
/// in the `docker` group, or the daemon down) is a different situation and can
/// be worth naming - but only the caller knows whether this invocation needed
/// an answer, which is what `report_if_unaskable` carries. Deciding that here
/// is what made `tekops doctor` announce a Docker problem to an operator whose
/// report is bare-metal (issue #16): the spawn happens *before* doctor's stack
/// ladder resolves, so at this point there is nothing to gate on.
fn docker_ps_names(report_if_unaskable: bool) -> Option<String> {
    let out = Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .ok()?;
    if !out.status.success() {
        if report_if_unaskable {
            eprintln!("note: `docker ps` failed; container detection unavailable");
        }
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Whether a `docker ps` that could not answer is worth reporting, given the
/// stack already in force for this invocation.
///
/// Split out for the same reason as `should_hint` just below: it is a couple
/// of tokens of condition guarding a message, exactly the shape that drifts
/// unnoticed, so the rule is machine-checked rather than inspection-checked.
///
/// The stack has to be both known *and* one that has containers. A stack still
/// being undecided means this call is the speculative detection itself, whose
/// failure just moves the ladder along to its next rung - reporting there is
/// reporting a question, not an answer, and on a bare-metal host the answer
/// turns out to be that Docker was never part of the picture. Bare-metal named
/// outright is the same conclusion reached earlier. On eth-docker or
/// rocketpool it is neither: doctor's container check is going to come up
/// empty *because* of this failure, and without the note that reads as "no
/// consensus container" rather than "Docker would not say".
fn should_report_unaskable_docker(stack: Option<Stack>) -> bool {
    stack.is_some_and(|s| s.container_suffix().is_some())
}

/// `detect_stack`, with an ambiguous result reported rather than swallowed.
///
/// Shared by the `logs` arm and both of doctor's detection call sites, so an
/// ambiguous host (two consensus containers, no `--stack` to narrow with)
/// gets the same treatment everywhere a caller folds the result down to an
/// `Option`: falling through the rest of that caller's own precedence ladder
/// rather than silently picking bare-metal defaults that are confidently
/// wrong. Names come from `docker ps`, which tekops did not author, so
/// they're sanitized before they reach the terminal - see term.rs's entry in
/// CLAUDE.md.
fn detect_or_note(ps: &str, only: Option<Stack>) -> Option<(Stack, String)> {
    note_ambiguity(detect_stack(ps, only))
}

/// `detect_validator_stack`, with an ambiguous result reported rather than
/// swallowed - the validator counterpart to `detect_or_note`, and the rung
/// that lets doctor name a stack on a host whose only container is a
/// validator.
fn detect_validator_or_note(ps: &str, only: Option<Stack>) -> Option<(Stack, String)> {
    note_ambiguity(detect_validator_stack(ps, only))
}

/// Folds a detection down to an `Option`, reporting only the failure the
/// operator can act on. `DetectError` already words itself for the role it was
/// given, so one body serves both.
fn note_ambiguity(found: Result<(Stack, String), DetectError>) -> Option<(Stack, String)> {
    match found {
        Ok(found) => Some(found),
        // A bare-metal host that never wanted Docker must not be nagged
        // about detection finding nothing.
        Err(DetectError::NotFound(_)) => None,
        Err(e @ DetectError::Ambiguous(..)) => {
            eprintln!("note: {}", sanitize(&e.to_string()));
            None
        }
    }
}

/// Whether `docker ps` has anything left to answer.
///
/// The logical negation of every rung in `logs::resolve_log_target` above
/// `detected`. Extracted so the two call sites cannot drift apart and so the
/// condition is testable without spawning Docker.
fn needs_detection(
    path: Option<&PathBuf>,
    container_flag: Option<&String>,
    container_env: Option<&String>,
    logs_file_env: Option<&String>,
    cfg: &crate::config::Config,
) -> bool {
    path.is_none()
        && container_flag.is_none()
        && container_env.is_none()
        && logs_file_env.is_none()
        && cfg.container.is_none()
        && cfg.logs_file.is_none()
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
        "hint: detected {} containers; try --stack {} or set $TEKOPS_STACK",
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
/// `false` because `should_hint` has already established that no stack was
/// named: there is no Docker problem to report to an operator this code has
/// yet to establish uses Docker at all, only a suggestion it now cannot make.
fn docker_stack_hint() -> Option<String> {
    hint_from_ps(&docker_ps_names(false)?)
}

/// Whether an unreachable-endpoint failure should carry a stack hint.
///
/// Split out from `exit_for_api` so the rule is machine-checked rather than
/// inspection-checked: it is two tokens of condition, and it is precisely the
/// part a later refactor could drop without any test noticing. The endpoint
/// has to be unreachable at all - a wrong status code or a malformed body
/// means the ports were right and the stack profile was never the problem -
/// and the operator must not have already named one, since suggesting
/// `--stack` to someone who passed `--stack` is just restating their own
/// input back at them.
fn should_hint(stack: Option<Stack>, e: &ApiError) -> bool {
    stack.is_none() && matches!(e, ApiError::Unreachable(_))
}

/// The exit path for commands that talk to a node over HTTP.
///
/// Identical to `exit_for` except that an unreachable endpoint gets a stack
/// hint appended, since "could not reach endpoint" on a Docker host almost
/// always means the ports are the default bare-metal ones. `stack` is the
/// profile already resolved for this invocation (flag, then $TEKOPS_STACK, then
/// the config file's `stack`) - when the operator has named one, by any of those
/// three, the hint would just be telling them to do what they already did, so it
/// is skipped: if their explicitly chosen profile still cannot connect, the
/// stack name was never the problem.
fn exit_for_api(result: Result<(), ApiError>, stack: Option<Stack>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            if should_hint(stack, &e) {
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

/// One row of the version report, from a scrape result and the family to read.
///
/// An endpoint that answered but exports no version metric is reported
/// distinctly from one that could not be reached: the first is "this is not a
/// Teku process", the second is "this process is not running".
fn process_version(
    url: &str,
    families: &Result<crate::metrics::EndpointFamilies, ApiError>,
    pick: fn(&crate::metrics::EndpointFamilies) -> &Vec<String>,
) -> crate::output::ProcessVersion {
    let (versions, error) = match families {
        Ok(f) if !pick(f).is_empty() => (pick(f).clone(), None),
        Ok(_) => (
            Vec::new(),
            Some("responded, but exports no Teku version metric".to_string()),
        ),
        Err(e) => (Vec::new(), Some(sanitize(&e.to_string()))),
    };
    crate::output::ProcessVersion {
        url: url.to_string(),
        versions,
        error,
    }
}

/// Scrapes both processes and builds the report.
///
/// Equal URLs mean one process serving both families, so the endpoint is
/// scraped once and both rows are read off that single result rather than
/// asking the same URL twice.
fn build_version_report(bn_url: &str, vc_url: &str) -> crate::output::VersionReport {
    let bn_families = MetricsClient::new(bn_url.to_string()).families();
    let vc_families = (bn_url != vc_url).then(|| MetricsClient::new(vc_url.to_string()).families());
    let vc_source = vc_families.as_ref().unwrap_or(&bn_families);

    crate::output::VersionReport {
        beacon_node: process_version(bn_url, &bn_families, |f| &f.beacon_versions),
        validator_client: process_version(vc_url, vc_source, |f| &f.validator_versions),
    }
}

fn metrics_version(bn_url: &str, vc_url: &str, json: bool) -> Result<(), ApiError> {
    let report = build_version_report(bn_url, vc_url);

    // Neither process answered, which is a failed command rather than a report
    // of two absences. One answering is a useful result and exits zero.
    if report.beacon_node.versions.is_empty() && report.validator_client.versions.is_empty() {
        // Equal URLs mean one scrape covers both rows (see
        // `build_version_report`), so naming the endpoint twice would read as
        // two different, both-wrong URLs rather than one.
        let endpoints = if bn_url == vc_url {
            format!("at {bn_url}")
        } else {
            format!("at {bn_url} or {vc_url}")
        };
        return Err(ApiError::Malformed(format!(
            "no Teku version metric found {endpoints}; are these Teku metrics endpoints?"
        )));
    }

    if json {
        println!(
            "{}",
            serde_json::to_string(&report).expect("serialize version json")
        );
    } else {
        println!("{}", format_version_table(&report));
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

/// `log-level` gets a handler of its own, dispatched inline like `run_doctor`
/// and for a related reason: its exit code comes from two different error
/// channels. A body that could not be fetched or parsed is not an `ApiError`,
/// so it cannot go through `exit_for_api` - and it must not, since the
/// `--stack` hint that adds would point at the node's ports for a failure that
/// happened at a gist. Only the request itself is handed to `exit_for_api`.
fn run_log_level(
    target: String,
    log_filter: Vec<String>,
    yes: bool,
    api: ApiArgs,
    cfg: &crate::config::Config,
) -> ExitCode {
    let spec = match resolve_spec(target, log_filter, yes) {
        Ok(Some(spec)) => spec,
        Ok(None) => {
            eprintln!("aborted, nothing was sent");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let stack = resolve_stack(api.stack, env::var("TEKOPS_STACK").ok(), cfg.stack);
    let client = BeaconClient::new(resolve_base_url(
        api.api_url,
        env::var("TEKOPS_API_URL").ok(),
        cfg.api_url.clone(),
        stack,
    ));
    exit_for_api(beacon_log_level(&client, &spec, api.json), stack)
}

/// Works out what to send, or `None` if the operator declined it.
///
/// Nothing is fetched for a level typed on the command line, so `tekops
/// log-level debug` never touches the network and never prompts.
fn resolve_spec(
    target: String,
    log_filter: Vec<String>,
    yes: bool,
) -> Result<Option<LogLevelSpec>, LogLevelError> {
    let url = match resolve_log_level_target(target, log_filter)? {
        LogLevelTarget::Direct(spec) => return Ok(Some(spec)),
        LogLevelTarget::Url(url) => url,
    };

    let spec = loglevel::load(&url)?;
    if yes {
        return Ok(Some(spec));
    }
    // The premise of fetching a body is that the operator could not have
    // written those logger names themselves, which is exactly why they get to
    // see them before they land on the node.
    eprint!("{}", format_log_level_preview(&url, &spec));
    Ok(confirm_apply()?.then_some(spec))
}

/// Prompts on stderr rather than stdout, so `--json` still emits nothing but
/// JSON on stdout.
fn confirm_apply() -> Result<bool, LogLevelError> {
    eprint!("apply? [y/N] ");
    io::stderr()
        .flush()
        .map_err(|e| LogLevelError::Io(e.to_string()))?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|e| LogLevelError::Io(e.to_string()))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

fn beacon_log_level(
    client: &BeaconClient,
    spec: &LogLevelSpec,
    json: bool,
) -> Result<(), ApiError> {
    let level = &spec.level;
    let log_filter = spec.log_filter.clone();
    client.set_log_level(level, log_filter.clone())?;
    if json {
        let payload = LogLevelJson { level, log_filter };
        println!(
            "{}",
            serde_json::to_string(&payload).expect("serialize log level json")
        );
    } else {
        match &spec.log_filter {
            Some(loggers) => println!("log level set to {level} for: {}", loggers.join(", ")),
            None => println!("log level set to {level} (global)"),
        }
    }
    Ok(())
}

fn run_update(target: UpdateTarget, json: bool, yes: bool) -> Result<(), UpdateError> {
    match target {
        UpdateTarget::Check => {
            let result = update::check(curl::fetch)?;
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
            let result = update::check(curl::fetch)?;
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

/// The config file for this invocation, or an empty one.
///
/// The only place `$XDG_CONFIG_HOME`/`$HOME` are read for the config file,
/// mirroring `dirs_from_env`'s role for `autocomplete`.
fn config_from_env() -> Result<crate::config::Config, crate::config::ConfigError> {
    let xdg = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = env::var_os("HOME").map(PathBuf::from);
    crate::config::load(crate::config::path(xdg.as_deref(), home.as_deref()).as_deref())
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
    update::install(curl::fetch, version, &dest)?;
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
    use crate::protocol::Protocol;
    use clap::CommandFactory;

    #[test]
    fn logs_without_a_path_parses_with_path_none() {
        let cli = Cli::try_parse_from(["tekops", "logs"]).unwrap();
        assert!(matches!(cli.command, Commands::Logs { path: None, .. }));
    }

    // `teku` used to be a source name rather than a path. The source concept
    // is gone, so a bare word now carries no special meaning any more.
    #[test]
    fn logs_a_bare_word_parses_as_an_ordinary_path() {
        let cli = Cli::try_parse_from(["tekops", "logs", "teku"]).unwrap();
        match cli.command {
            Commands::Logs { path, .. } => assert_eq!(path, Some(PathBuf::from("teku"))),
            _ => panic!("expected a Logs command"),
        }
    }

    #[test]
    fn logs_accepts_a_bare_path_as_the_first_positional() {
        let cli = Cli::try_parse_from(["tekops", "logs", "/var/log/x.log"]).unwrap();
        match cli.command {
            Commands::Logs { path, .. } => {
                assert_eq!(path, Some(PathBuf::from("/var/log/x.log")))
            }
            _ => panic!("expected a Logs command"),
        }
    }

    #[test]
    fn dump_logs_defaults_to_a_thousand_lines() {
        let cli = Cli::try_parse_from(["tekops", "dump-logs"]).unwrap();
        match cli.command {
            Commands::DumpLogs { lines, .. } => assert_eq!(lines, 1000),
            _ => panic!("expected a DumpLogs command"),
        }
    }

    #[test]
    fn dump_logs_takes_a_path_positional_like_logs_does() {
        let cli = Cli::try_parse_from(["tekops", "dump-logs", "/var/log/x.log"]).unwrap();
        match cli.command {
            Commands::DumpLogs { path, .. } => {
                assert_eq!(path, Some(PathBuf::from("/var/log/x.log")))
            }
            _ => panic!("expected a DumpLogs command"),
        }
    }

    /// Naming a container fully determines what gets read, which makes a path
    /// alongside it meaningless rather than merely redundant - the same rule
    /// `logs` applies.
    #[test]
    fn dump_logs_rejects_a_path_alongside_a_container() {
        assert!(
            Cli::try_parse_from(["tekops", "dump-logs", "/x.log", "--container", "c"]).is_err()
        );
    }

    #[test]
    fn dump_logs_accepts_its_flags() {
        let cli = Cli::try_parse_from([
            "tekops",
            "dump-logs",
            "-n",
            "50",
            "-o",
            "out.txt",
            "--gist",
            "--yes",
            "--header",
            "--doctor",
        ])
        .unwrap();
        match cli.command {
            Commands::DumpLogs {
                lines,
                output,
                gist,
                yes,
                header,
                doctor,
                ..
            } => {
                assert_eq!(lines, 50);
                assert_eq!(output, Some(PathBuf::from("out.txt")));
                assert!(gist && yes && header && doctor);
            }
            _ => panic!("expected a DumpLogs command"),
        }
    }

    /// `dump-logs --doctor` must probe with exactly the config `doctor` itself
    /// would build, not a second approximation of it. The bare-metal case is
    /// enough to catch the helper being wired up wrong, and needs no Docker.
    #[test]
    fn doctor_probe_config_applies_the_stack_to_both_urls() {
        let cfg = doctor_probe_config(
            Some(Stack::BareMetal),
            None,
            MetricUrlFlags::default(),
            None,
            &crate::config::Config::default(),
        );
        assert_eq!(cfg.stack, Some(Stack::BareMetal));
        assert_eq!(
            cfg.api_url,
            resolve_base_url(None, None, None, Some(Stack::BareMetal))
        );
        assert_eq!(
            cfg.bn_metric_url,
            resolve_bn_metric_url(None, None, None, None, None, None, Some(Stack::BareMetal))
        );
        assert_eq!(
            cfg.vc_metric_url,
            resolve_vc_metric_url(None, None, None, Some(Stack::BareMetal))
        );
    }

    /// The legacy trio (`--metric-url` / `$TEKOPS_METRIC_URL` / `metric_url`)
    /// used to mean the validator client; `doctor` now reads it as the beacon
    /// node's URL. An operator who set it to steer `doctor` specifically must
    /// still have it reach *some* field of `ProbeConfig`, not be silently
    /// dropped - see `resolve_bn_metric_url`'s doc comment.
    #[test]
    fn doctor_probe_config_still_honours_the_legacy_metric_url_flag() {
        let cfg = doctor_probe_config(
            Some(Stack::BareMetal),
            None,
            MetricUrlFlags {
                bn: None,
                vc: None,
                legacy: Some("http://legacy.invalid:9000".to_string()),
            },
            None,
            &crate::config::Config::default(),
        );
        assert_eq!(cfg.bn_metric_url, "http://legacy.invalid:9000");
    }

    #[test]
    fn doctor_probe_config_prefers_an_explicit_url_over_the_stack_default() {
        let cfg = doctor_probe_config(
            Some(Stack::BareMetal),
            Some("http://example.invalid:1234".to_string()),
            MetricUrlFlags::default(),
            None,
            &crate::config::Config::default(),
        );
        assert_eq!(cfg.api_url, "http://example.invalid:1234");
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
    fn logs_line_count_combines_with_a_path() {
        let cli = Cli::try_parse_from(["tekops", "logs", "/tmp/x.log", "-n", "7"]).unwrap();
        match cli.command {
            Commands::Logs { path, lines, .. } => {
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
                metrics: VcMetricArgs {
                    vc_metric_url: None,
                    stack: None,
                    json: false
                }
            }
        ));
    }

    #[test]
    fn validators_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "validators"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Validators {
                metrics: VcMetricArgs {
                    vc_metric_url: None,
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
                    bn_metric_url: None,
                    vc_metric_url: None,
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
    fn log_level_requires_a_level_argument() {
        let result = Cli::try_parse_from(["tekops", "log-level"]);
        assert!(result.is_err());
    }

    #[test]
    fn log_level_is_a_top_level_command() {
        let cli = Cli::try_parse_from(["tekops", "log-level", "DEBUG"]).unwrap();
        match cli.command {
            Commands::LogLevel {
                target,
                log_filter,
                yes,
                api,
            } => {
                assert_eq!(target, "DEBUG");
                assert!(log_filter.is_empty());
                assert!(!yes);
                assert_eq!(api.api_url, None);
                assert!(!api.json);
            }
            _ => panic!("expected LogLevel command"),
        }
    }

    /// The positional takes a level or a URL, hand-matched by
    /// `resolve_log_level_target` - clap must not try to type it as either.
    #[test]
    fn log_level_accepts_a_url_in_place_of_a_level() {
        let cli = Cli::try_parse_from([
            "tekops",
            "log-level",
            "https://gist.github.com/someone/abc123",
        ])
        .unwrap();
        match cli.command {
            Commands::LogLevel { target, .. } => {
                assert_eq!(target, "https://gist.github.com/someone/abc123");
            }
            _ => panic!("expected LogLevel command"),
        }
    }

    #[test]
    fn log_level_accepts_a_confirmation_skip() {
        let cli = Cli::try_parse_from([
            "tekops",
            "log-level",
            "https://gist.github.com/someone/abc123",
            "-y",
        ])
        .unwrap();
        match cli.command {
            Commands::LogLevel { yes, .. } => assert!(yes),
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
                target, log_filter, ..
            } => {
                assert_eq!(target, "DEBUG");
                assert_eq!(log_filter, vec!["org.a".to_string(), "org.b".to_string()]);
            }
            _ => panic!("expected LogLevel command"),
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

    /// `duties` and `validators` read only the validator client, and the
    /// legacy `--metric-url` now names the beacon node - so it must be
    /// rejected here rather than silently steering nothing, same as
    /// `--bn-metric-url`. Rejection is the correct outcome: clap's error
    /// names `--vc-metric-url`, which is the flag an old script needs to
    /// switch to.
    #[test]
    fn duties_and_validators_reject_the_beacon_node_metric_flags() {
        for cmd in ["duties", "validators"] {
            // `Cli` derives no `Debug`, so `.err()` rather than `.expect_err()`.
            let err = Cli::try_parse_from(["tekops", cmd, "--metric-url", "http://x:2/m"])
                .err()
                .unwrap_or_else(|| panic!("{cmd} must reject the legacy beacon-node flag"));
            assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);

            let err = Cli::try_parse_from(["tekops", cmd, "--bn-metric-url", "http://x:2/m"])
                .err()
                .unwrap_or_else(|| panic!("{cmd} must reject --bn-metric-url"));
            assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn duties_and_validators_still_accept_the_vc_metric_flag() {
        let cli =
            Cli::try_parse_from(["tekops", "duties", "--vc-metric-url", "http://x:2/m"]).unwrap();
        match cli.command {
            Commands::Duties { metrics } => {
                assert_eq!(metrics.vc_metric_url.as_deref(), Some("http://x:2/m"))
            }
            _ => panic!("expected Duties command"),
        }

        let cli = Cli::try_parse_from(["tekops", "validators", "--vc-metric-url", "http://x:2/m"])
            .unwrap();
        match cli.command {
            Commands::Validators { metrics } => {
                assert_eq!(metrics.vc_metric_url.as_deref(), Some("http://x:2/m"))
            }
            _ => panic!("expected Validators command"),
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
            el_offline: false,
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
            total_eth: Some(63.5),
        };
        let json = serde_json::to_string(&metrics).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["counts_by_status"]["active_ongoing"], 100);
        assert_eq!(value["total_eth"], 63.5);
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
    /// them as subcommands alongside an optional positional version would
    /// reproduce the same "one slot, two meanings" problem `tekops logs` used
    /// to have historically: its first positional was typed as an enum of
    /// source names, so clap rejected any value outside that set before the
    /// command ever ran. `logs` no longer has a source argument at all.
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
            resolve_base_url(
                Some("http://x:1".into()),
                None,
                None,
                Some(Stack::RocketPool)
            ),
            "http://x:1"
        );
        // Stack profile beats the bare-metal default.
        assert_eq!(
            resolve_base_url(None, None, None, Some(Stack::RocketPool)),
            "http://localhost:5052"
        );
        // Nothing at all keeps today's behaviour.
        assert_eq!(
            resolve_base_url(None, None, None, None),
            "http://localhost:5051"
        );
    }

    /// Mixing is explicitly supported: a profile is one layer in the chain, not
    /// a mode that locks the other values.
    #[test]
    fn a_stack_profile_and_an_explicit_url_can_be_combined() {
        assert_eq!(
            resolve_base_url(
                Some("http://custom:5099".into()),
                None,
                None,
                Some(Stack::RocketPool)
            ),
            "http://custom:5099"
        );
        assert_eq!(
            resolve_vc_metric_url(None, None, None, Some(Stack::RocketPool)),
            "http://localhost:9101/metrics"
        );
    }

    #[test]
    fn the_api_url_flag_beats_the_env_and_the_config() {
        let got = resolve_base_url(
            Some("http://flag:1".into()),
            Some("http://env:2".into()),
            Some("http://cfg:3".into()),
            Some(Stack::EthDocker),
        );
        assert_eq!(got, "http://flag:1");
    }

    #[test]
    fn the_api_url_env_beats_the_config() {
        let got = resolve_base_url(
            None,
            Some("http://env:2".into()),
            Some("http://cfg:3".into()),
            Some(Stack::EthDocker),
        );
        assert_eq!(got, "http://env:2");
    }

    #[test]
    fn the_config_api_url_beats_the_stack_default() {
        let got = resolve_base_url(
            None,
            None,
            Some("http://cfg:3".into()),
            Some(Stack::EthDocker),
        );
        assert_eq!(got, "http://cfg:3");
    }

    #[test]
    fn the_stack_default_applies_when_nothing_states_an_api_url() {
        let got = resolve_base_url(None, None, None, Some(Stack::EthDocker));
        assert_eq!(got, Stack::EthDocker.api_url());
    }

    /// The beacon node's ladder, one tier at a time. New spellings beat legacy
    /// ones *within* a tier and never across one, so a legacy flag still beats
    /// a new environment variable.
    #[test]
    fn bn_metric_url_ladder_prefers_each_tier_in_order() {
        let all = || {
            (
                Some("http://flag".to_string()),
                Some("http://legacy-flag".to_string()),
                Some("http://env".to_string()),
                Some("http://legacy-env".to_string()),
                Some("http://cfg".to_string()),
                Some("http://legacy-cfg".to_string()),
            )
        };

        let (f, lf, e, le, c, lc) = all();
        assert_eq!(
            resolve_bn_metric_url(f, lf, e, le, c, lc, None),
            "http://flag"
        );

        let (_, lf, e, le, c, lc) = all();
        assert_eq!(
            resolve_bn_metric_url(None, lf, e, le, c, lc, None),
            "http://legacy-flag"
        );

        let (_, _, e, le, c, lc) = all();
        assert_eq!(
            resolve_bn_metric_url(None, None, e, le, c, lc, None),
            "http://env"
        );

        let (_, _, _, le, c, lc) = all();
        assert_eq!(
            resolve_bn_metric_url(None, None, None, le, c, lc, None),
            "http://legacy-env"
        );

        let (_, _, _, _, c, lc) = all();
        assert_eq!(
            resolve_bn_metric_url(None, None, None, None, c, lc, None),
            "http://cfg"
        );

        let (_, _, _, _, _, lc) = all();
        assert_eq!(
            resolve_bn_metric_url(None, None, None, None, None, lc, None),
            "http://legacy-cfg"
        );
    }

    #[test]
    fn bn_metric_url_falls_back_to_the_stacks_beacon_port() {
        assert_eq!(
            resolve_bn_metric_url(None, None, None, None, None, None, Some(Stack::RocketPool)),
            "http://localhost:9100/metrics"
        );
        assert_eq!(
            resolve_bn_metric_url(None, None, None, None, None, None, None),
            "http://localhost:8008/metrics"
        );
    }

    /// The meaning change, pinned. The deprecated spelling used to resolve to
    /// the validator client; it now resolves to the beacon node, agreeing with
    /// the unprefixed `--api-url`. If someone later "fixes" the alias back,
    /// this test is the thing that objects.
    #[test]
    fn the_deprecated_spelling_feeds_the_beacon_node_not_the_validator() {
        let legacy = "http://legacy".to_string();

        assert_eq!(
            resolve_bn_metric_url(
                None,
                None,
                None,
                None,
                None,
                Some(legacy.clone()),
                Some(Stack::EthDocker)
            ),
            "http://legacy"
        );

        // The same value must not reach the validator client's ladder at all.
        assert_eq!(
            resolve_vc_metric_url(None, None, None, Some(Stack::EthDocker)),
            "http://localhost:8009/metrics"
        );
    }

    #[test]
    fn vc_metric_url_ladder_prefers_each_tier_in_order() {
        assert_eq!(
            resolve_vc_metric_url(
                Some("http://flag".into()),
                Some("http://env".into()),
                Some("http://cfg".into()),
                None
            ),
            "http://flag"
        );
        assert_eq!(
            resolve_vc_metric_url(
                None,
                Some("http://env".into()),
                Some("http://cfg".into()),
                None
            ),
            "http://env"
        );
        assert_eq!(
            resolve_vc_metric_url(None, None, Some("http://cfg".into()), None),
            "http://cfg"
        );
        assert_eq!(
            resolve_vc_metric_url(None, None, None, Some(Stack::RocketPool)),
            "http://localhost:9101/metrics"
        );
        assert_eq!(
            resolve_vc_metric_url(None, None, None, None),
            "http://localhost:8010/metrics"
        );
    }

    /// The deprecated flag and its replacement name the same endpoint, so
    /// allowing both would mean silently honouring one and dropping the other.
    #[test]
    fn metric_url_and_bn_metric_url_cannot_both_be_given() {
        let err = command().try_get_matches_from([
            "tekops",
            "version",
            "--metric-url",
            "http://a",
            "--bn-metric-url",
            "http://b",
        ]);
        assert!(err.is_err(), "clap must reject both spellings at once");
    }

    /// The deprecated flag is accepted but not advertised.
    #[test]
    fn the_deprecated_metric_url_flag_is_hidden_but_still_parses() {
        assert!(command()
            .try_get_matches_from(["tekops", "version", "--metric-url", "http://a"])
            .is_ok());

        // A separate binding: `find_subcommand_mut` borrows mutably, so it
        // cannot be called on the temporary `command()` returns.
        let mut cmd = command();
        let help = cmd
            .find_subcommand_mut("version")
            .expect("version subcommand")
            .render_help()
            .to_string();
        assert!(
            !help.contains("--metric-url"),
            "the deprecated flag must not appear in help: {help}"
        );
        assert!(help.contains("--bn-metric-url"), "{help}");
        assert!(help.contains("--vc-metric-url"), "{help}");
    }

    /// Completions are generated from `command()`, so the new flags arrive
    /// automatically. Nothing proved that, though, and `hide = true` sitting
    /// on a neighbouring flag is exactly the kind of thing that could quietly
    /// take the others with it.
    #[test]
    fn generated_completions_offer_both_metric_flags() {
        let mut out = Vec::new();
        clap_complete::generate(
            clap_complete::shells::Bash,
            &mut command(),
            "tekops",
            &mut out,
        );
        let script = String::from_utf8(out).expect("clap_complete emits UTF-8");
        assert!(script.contains("bn-metric-url"), "{script}");
        assert!(script.contains("vc-metric-url"), "{script}");
    }

    #[test]
    fn stack_flag_beats_the_environment() {
        assert_eq!(
            resolve_stack(Some(Stack::EthDocker), Some("rocketpool".into()), None),
            Some(Stack::EthDocker)
        );
        assert_eq!(
            resolve_stack(None, Some("rocketpool".into()), None),
            Some(Stack::RocketPool)
        );
        assert_eq!(resolve_stack(None, None, None), None);
    }

    /// An unparseable $TEKOPS_STACK is ignored rather than fatal: it must not
    /// break commands that would have worked without it.
    #[test]
    fn an_unknown_stack_env_value_is_ignored() {
        assert_eq!(resolve_stack(None, Some("nonsense".into()), None), None);
    }

    #[test]
    fn the_stack_flag_beats_both_the_env_and_the_config() {
        let got = resolve_stack(
            Some(Stack::BareMetal),
            Some("eth-docker".into()),
            Some(Stack::RocketPool),
        );
        assert_eq!(got, Some(Stack::BareMetal));
    }

    #[test]
    fn the_stack_env_beats_the_config() {
        let got = resolve_stack(None, Some("eth-docker".into()), Some(Stack::RocketPool));
        assert_eq!(got, Some(Stack::EthDocker));
    }

    #[test]
    fn the_config_stack_is_used_when_nothing_else_says() {
        let got = resolve_stack(None, None, Some(Stack::RocketPool));
        assert_eq!(got, Some(Stack::RocketPool));
    }

    /// The env value is ignored when unparseable, and must fall through to the
    /// config rather than swallowing it.
    #[test]
    fn an_unparseable_stack_env_falls_through_to_the_config() {
        let got = resolve_stack(None, Some("nonsense".into()), Some(Stack::RocketPool));
        assert_eq!(got, Some(Stack::RocketPool));
    }

    #[test]
    fn nothing_stated_anywhere_still_needs_detection() {
        let cfg = crate::config::Config::default();
        assert!(needs_detection(None, None, None, None, &cfg));
    }

    /// The regression this guards: a configured operator paying for a spawn whose
    /// answer cannot be used. It is silent when it breaks - the symptom is a
    /// `docker ps`, not an error.
    #[test]
    fn a_config_container_removes_the_need_to_detect() {
        let cfg = crate::config::Config {
            container: Some("c".into()),
            ..Default::default()
        };
        assert!(!needs_detection(None, None, None, None, &cfg));
    }

    #[test]
    fn a_config_logs_file_removes_the_need_to_detect() {
        let cfg = crate::config::Config {
            logs_file: Some(PathBuf::from("/x.log")),
            ..Default::default()
        };
        assert!(!needs_detection(None, None, None, None, &cfg));
    }

    #[test]
    fn any_stated_source_removes_the_need_to_detect() {
        let cfg = crate::config::Config::default();
        let p = PathBuf::from("/x.log");
        let s = "c".to_string();
        assert!(!needs_detection(Some(&p), None, None, None, &cfg));
        assert!(!needs_detection(None, Some(&s), None, None, &cfg));
        assert!(!needs_detection(None, None, Some(&s), None, &cfg));
        assert!(!needs_detection(None, None, None, Some(&s), &cfg));
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
    #[test]
    fn a_path_and_a_container_cannot_both_be_given() {
        assert!(Cli::try_parse_from(["tekops", "logs", "/a.log", "--container", "c"]).is_err());
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

    /// "a eth-docker stack" / "a rocketpool stack" both read wrong - the
    /// wording was changed to "detected <name> containers" specifically to
    /// avoid ever needing an article in front of a stack name.
    #[test]
    fn hint_wording_never_puts_an_article_before_the_stack_name() {
        for ps in [
            "rocketpool_node\nrocketpool_eth2\n",
            "eth-docker-consensus-1\n",
        ] {
            let hint = hint_from_ps(ps).expect("expected a hint");
            assert!(!hint.contains(" a "), "got: {hint}");
        }
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

    /// The C1 regression: an ambiguous host (two consensus containers, no
    /// `--stack` to narrow with) must fall through to `None` - never a
    /// silently wrong guess - so callers land on their own next rung
    /// (`resolve_doctor_stack`'s bare-metal terminal, say) with a note on
    /// stderr rather than a confidently incorrect report on stdout.
    #[test]
    fn detect_or_note_returns_none_on_ambiguity_rather_than_a_guess() {
        let ps = "eth-docker-consensus-1\nrocketpool_eth2\n";
        assert_eq!(detect_or_note(ps, None), None);
    }

    #[test]
    fn detect_or_note_returns_the_match_when_unambiguous() {
        let ps = "eth-docker-consensus-1\n";
        assert_eq!(
            detect_or_note(ps, None),
            Some((Stack::EthDocker, "eth-docker-consensus-1".to_string()))
        );
    }

    #[test]
    fn detect_or_note_returns_none_when_nothing_matches() {
        assert_eq!(detect_or_note("postgres\nredis\n", None), None);
    }

    /// Issue #16. A Docker daemon that will not answer is only news to a node
    /// that has containers; on the two stacks that have none it is a report
    /// about software the node's operator never chose to involve.
    #[test]
    fn an_unaskable_docker_is_reported_only_on_a_stack_that_has_containers() {
        assert!(should_report_unaskable_docker(Some(Stack::EthDocker)));
        assert!(should_report_unaskable_docker(Some(Stack::RocketPool)));

        assert!(!should_report_unaskable_docker(Some(Stack::BareMetal)));
        // Undecided: this is the speculative detection rung itself, whose
        // failure only moves the ladder along.
        assert!(!should_report_unaskable_docker(None));
    }

    /// The rung `docker_ps_names(false)` in `run_doctor` is paired with: when
    /// detection is what failed, the ladder terminates in bare-metal, so the
    /// report that prints is one the note would have contradicted.
    #[test]
    fn failed_detection_leaves_doctor_on_a_stack_that_reports_no_docker() {
        let stack = resolve_doctor_stack(None, None);
        assert_eq!(stack, Some(Stack::BareMetal));
        assert!(!should_report_unaskable_docker(stack));
    }

    #[test]
    fn hint_is_offered_only_for_an_unreachable_endpoint_with_no_stack_named() {
        let unreachable = ApiError::Unreachable("x".into());
        let status = ApiError::Status(404, "x".into());
        let malformed = ApiError::Malformed("x".into());

        // The one case that earns a hint.
        assert!(should_hint(None, &unreachable));

        // Already named a stack: suggesting one would restate their own input.
        assert!(!should_hint(Some(Stack::EthDocker), &unreachable));

        // The endpoint answered, so the ports are right and the stack is not
        // the problem.
        assert!(!should_hint(None, &status));
        assert!(!should_hint(None, &malformed));
        assert!(!should_hint(Some(Stack::RocketPool), &status));
    }

    #[test]
    fn doctor_parses_with_no_arguments() {
        let cli = Cli::try_parse_from(["tekops", "doctor"]).unwrap();
        assert!(matches!(cli.command, Commands::Doctor { .. }));
    }

    #[test]
    fn doctor_accepts_every_url_flag_without_an_argument_id_collision() {
        // clap panics at startup on a duplicate arg id, which is why
        // MetricArgs is not flattened here alongside ApiArgs.
        Cli::command().debug_assert();

        let cli = Cli::try_parse_from([
            "tekops",
            "doctor",
            "--api-url",
            "http://a:5052",
            "--bn-metric-url",
            "http://a:8008/metrics",
            "--vc-metric-url",
            "http://a:8009/metrics",
            "--stack",
            "rocketpool",
            "--data-dir",
            "/data",
            "--json",
        ])
        .unwrap();
        match cli.command {
            Commands::Doctor {
                api,
                bn_metric_url,
                vc_metric_url,
                metric_url,
                data_dir,
            } => {
                assert_eq!(api.api_url.as_deref(), Some("http://a:5052"));
                assert_eq!(bn_metric_url.as_deref(), Some("http://a:8008/metrics"));
                assert_eq!(vc_metric_url.as_deref(), Some("http://a:8009/metrics"));
                assert_eq!(metric_url, None);
                assert_eq!(data_dir.as_deref(), Some(Path::new("/data")));
                assert_eq!(api.stack, Some(Stack::RocketPool));
                assert!(api.json);
            }
            _ => panic!("expected Doctor"),
        }
    }

    #[test]
    fn doctor_exit_code_is_failure_only_when_a_check_fails() {
        use crate::doctor::{Finding, Status};
        assert_eq!(
            format!(
                "{:?}",
                exit_for_findings(&[Finding::new("a", Status::Pass, "")])
            ),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert_eq!(
            format!(
                "{:?}",
                exit_for_findings(&[Finding::new("a", Status::Warn, "")])
            ),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert_eq!(
            format!(
                "{:?}",
                exit_for_findings(&[Finding::new("a", Status::Fail, "")])
            ),
            format!("{:?}", ExitCode::FAILURE)
        );
    }

    /// Doctor is the one API command that applies detection to the URL, not
    /// just to a container name, because it runs `docker ps` anyway.
    #[test]
    fn doctor_stack_ladder_prefers_a_stated_stack_then_detection() {
        // Flag, env and config are already folded into `stated` by
        // `resolve_stack`, whose own precedence is tested above.
        assert_eq!(
            resolve_doctor_stack(
                resolve_stack(Some(Stack::BareMetal), None, None),
                Some(Stack::EthDocker)
            ),
            Some(Stack::BareMetal)
        );
        assert_eq!(
            resolve_doctor_stack(
                resolve_stack(None, Some("rocketpool".to_string()), None),
                Some(Stack::EthDocker)
            ),
            Some(Stack::RocketPool)
        );
        assert_eq!(
            resolve_doctor_stack(None, Some(Stack::EthDocker)),
            Some(Stack::EthDocker)
        );
        // The ladder terminates in bare-metal, not None: the report header
        // and the (bare-metal-defaulted) URLs it prints alongside it must
        // never disagree about what "nothing was given" means.
        assert_eq!(resolve_doctor_stack(None, None), Some(Stack::BareMetal));
    }

    /// The combined deployment: a consensus container and no validator
    /// container beside it means the validator is inside that container, so
    /// its stack answers for both. Terminating in bare-metal instead would
    /// hand an eth-docker node running Teku in combined mode the bare-metal
    /// validator metrics port.
    #[test]
    fn an_undetected_validator_inherits_the_consensus_containers_stack() {
        assert_eq!(
            resolve_doctor_vc_stack(None, None, Some(Stack::EthDocker)),
            Some(Stack::EthDocker)
        );
    }

    /// The reported bug, at the rung that decides it: Rocket Pool supervising
    /// only a validator, against a beacon node it did not start. The validator
    /// is on rocketpool and the beacon node is not - the whole report used to
    /// say "bare-metal" and mean it about both.
    #[test]
    fn a_validator_only_stack_does_not_drag_the_beacon_node_with_it() {
        let detected_bn = None;
        let detected_vc = Some(Stack::RocketPool);

        assert_eq!(
            resolve_doctor_stack(None, detected_bn),
            Some(Stack::BareMetal)
        );
        assert_eq!(
            resolve_doctor_vc_stack(None, detected_vc, detected_bn),
            Some(Stack::RocketPool)
        );
    }

    /// A stated stack describes the whole deployment and outranks both
    /// detections, the same way it does on every other command.
    #[test]
    fn a_stated_stack_beats_validator_detection_too() {
        assert_eq!(
            resolve_doctor_vc_stack(
                Some(Stack::BareMetal),
                Some(Stack::RocketPool),
                Some(Stack::EthDocker)
            ),
            Some(Stack::BareMetal)
        );
    }

    #[test]
    fn doctor_data_dir_defaults_to_the_bare_metal_path_when_nothing_else_answers() {
        assert_eq!(
            resolve_doctor_data_dir(None, None, None, Some(Stack::BareMetal)),
            Some(PathBuf::from(DEFAULT_BARE_METAL_DATA_DIR))
        );
    }

    #[test]
    fn doctor_data_dir_flag_beats_the_bare_metal_default() {
        assert_eq!(
            resolve_doctor_data_dir(
                Some(PathBuf::from("/custom")),
                None,
                None,
                Some(Stack::BareMetal)
            ),
            Some(PathBuf::from("/custom"))
        );
    }

    /// On a Docker stack, nothing given must resolve to `None`, not the
    /// bare-metal default: `doctor::probe` layers the container's own mount
    /// fallback beneath this value, and a default here would shadow it,
    /// making the check measure the wrong filesystem.
    #[test]
    fn doctor_data_dir_stays_none_on_a_docker_stack_so_the_container_mount_can_answer() {
        assert_eq!(
            resolve_doctor_data_dir(None, None, None, Some(Stack::EthDocker)),
            None
        );
        assert_eq!(
            resolve_doctor_data_dir(None, None, None, Some(Stack::RocketPool)),
            None
        );
    }

    #[test]
    fn the_data_dir_flag_beats_the_env_and_the_config() {
        let got = resolve_doctor_data_dir(
            Some(PathBuf::from("/flag")),
            Some("/env".into()),
            Some(PathBuf::from("/cfg")),
            Some(Stack::BareMetal),
        );
        assert_eq!(got, Some(PathBuf::from("/flag")));
    }

    #[test]
    fn the_data_dir_env_beats_the_config() {
        let got = resolve_doctor_data_dir(
            None,
            Some("/env".into()),
            Some(PathBuf::from("/cfg")),
            Some(Stack::BareMetal),
        );
        assert_eq!(got, Some(PathBuf::from("/env")));
    }

    #[test]
    fn the_config_data_dir_beats_the_bare_metal_default() {
        let got = resolve_doctor_data_dir(
            None,
            None,
            Some(PathBuf::from("/cfg")),
            Some(Stack::BareMetal),
        );
        assert_eq!(got, Some(PathBuf::from("/cfg")));
    }

    /// On a Docker stack this stays None unless stated, so the container's own
    /// mount answers. A config value counts as stated.
    #[test]
    fn the_config_data_dir_applies_on_a_docker_stack_too() {
        let got = resolve_doctor_data_dir(
            None,
            None,
            Some(PathBuf::from("/cfg")),
            Some(Stack::EthDocker),
        );
        assert_eq!(got, Some(PathBuf::from("/cfg")));
    }

    #[test]
    fn an_unstated_data_dir_still_stays_none_on_a_docker_stack() {
        assert_eq!(
            resolve_doctor_data_dir(None, None, None, Some(Stack::EthDocker)),
            None
        );
    }

    #[test]
    fn an_unstated_data_dir_still_defaults_on_bare_metal() {
        assert_eq!(
            resolve_doctor_data_dir(None, None, None, Some(Stack::BareMetal)),
            Some(PathBuf::from(DEFAULT_BARE_METAL_DATA_DIR))
        );
    }

    /// Carried over from Task 1's review: `resolve_doctor_stack` must prefer a
    /// configured stack over one `docker ps` detected, consistent with
    /// `flag > env > config > detection`. `resolve_stack` folds the config
    /// rung into `stated`, which this prefers over the detected value.
    #[test]
    fn resolve_doctor_stack_prefers_the_config_over_detection() {
        let stated = resolve_stack(None, None, Some(Stack::RocketPool));
        assert_eq!(
            resolve_doctor_stack(stated, Some(Stack::EthDocker)),
            Some(Stack::RocketPool)
        );
    }

    /// Bare-metal, stated by flag, environment or config file, is the one
    /// answer that leaves `docker ps` nothing to contribute: no stack to
    /// detect and no container to name. Pinned on the extracted gate rather
    /// than on `doctor_probe_config`'s output, which cannot be tested without
    /// Docker.
    #[test]
    fn a_stated_bare_metal_stack_skips_the_docker_ps_spawn() {
        for stated in [
            resolve_stack(Some(Stack::BareMetal), None, None),
            resolve_stack(None, Some("bare-metal".into()), None),
            resolve_stack(None, None, Some(Stack::BareMetal)),
        ] {
            assert!(!doctor_needs_docker_ps(stated));
        }
    }

    /// The regression this guards against: a stated Docker stack used to skip
    /// the spawn, because the gate asked "is the stack still unknown". Doctor
    /// now inspects two containers, and neither name is knowable without
    /// asking Docker - both stacks let the operator rename the project prefix.
    #[test]
    fn a_stated_docker_stack_still_needs_docker_ps_for_the_container_names() {
        assert!(doctor_needs_docker_ps(Some(Stack::EthDocker)));
        assert!(doctor_needs_docker_ps(Some(Stack::RocketPool)));
    }

    #[test]
    fn doctor_needs_docker_ps_when_nothing_is_stated() {
        assert!(doctor_needs_docker_ps(None));
    }

    /// Two different endpoints, both answering.
    #[test]
    fn version_reports_a_separated_deployment_from_two_endpoints() {
        let mut bn_server = mockito::Server::new();
        let _bn = bn_server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(r#"beacon_teku_version_total{version="teku/v25.4.1"} 1"#)
            .create();
        let mut vc_server = mockito::Server::new();
        let _vc = vc_server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(r#"validator_teku_version_total{version="teku/v25.3.0"} 1"#)
            .create();

        let report = build_version_report(
            &format!("{}/metrics", bn_server.url()),
            &format!("{}/metrics", vc_server.url()),
        );
        assert_eq!(
            report.beacon_node.versions,
            vec!["teku/v25.4.1".to_string()]
        );
        assert_eq!(
            report.validator_client.versions,
            vec!["teku/v25.3.0".to_string()]
        );
    }

    /// Equal URLs mean one process serving both families. The endpoint is
    /// scraped once, and mockito's default expectation of exactly one hit per
    /// mock is what proves it.
    #[test]
    fn version_scrapes_once_when_both_urls_are_the_same() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(
                "beacon_teku_version_total{version=\"teku/v25.4.1\"} 1\n\
                 validator_teku_version_total{version=\"teku/v25.4.1\"} 1",
            )
            .expect(1)
            .create();

        let url = format!("{}/metrics", server.url());
        let report = build_version_report(&url, &url);
        assert_eq!(
            report.beacon_node.versions,
            vec!["teku/v25.4.1".to_string()]
        );
        assert_eq!(
            report.validator_client.versions,
            vec!["teku/v25.4.1".to_string()]
        );
        m.assert();
    }

    /// One side down is still a useful answer, so the row carries the reason
    /// and the command succeeds.
    #[test]
    fn version_reports_the_reachable_process_when_the_other_is_down() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(r#"beacon_teku_version_total{version="teku/v25.4.1"} 1"#)
            .create();

        let report = build_version_report(
            &format!("{}/metrics", server.url()),
            "http://127.0.0.1:1/metrics",
        );
        assert_eq!(
            report.beacon_node.versions,
            vec!["teku/v25.4.1".to_string()]
        );
        assert!(report.validator_client.versions.is_empty());
        assert!(report.validator_client.error.is_some());
    }

    /// The spec's exit contract for `version`: two absences is a failed
    /// command, not a report with two empty rows. Both endpoints here are
    /// unreachable, so this proves the non-zero exit path rather than the
    /// "reachable but no version metric" one covered elsewhere.
    #[test]
    fn version_fails_when_neither_endpoint_answers() {
        let err = metrics_version(
            "http://127.0.0.1:1/metrics",
            "http://127.0.0.1:1/metrics",
            false,
        )
        .expect_err("neither endpoint answered, so this must fail");
        assert!(
            err.to_string().contains("no Teku version metric found"),
            "{err}"
        );
    }

    /// Equal URLs collapse to one scrape (see `build_version_report`), so the
    /// failure message must name the single endpoint once rather than
    /// printing the same URL twice joined by "or".
    #[test]
    fn version_failure_message_names_the_endpoint_once_when_urls_are_equal() {
        let err = metrics_version(
            "http://127.0.0.1:1/metrics",
            "http://127.0.0.1:1/metrics",
            false,
        )
        .expect_err("neither endpoint answered, so this must fail");
        let message = err.to_string();
        assert_eq!(
            message.matches("127.0.0.1:1/metrics").count(),
            1,
            "endpoint named more than once: {message}"
        );
    }
}
