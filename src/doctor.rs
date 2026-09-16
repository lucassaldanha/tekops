//! `tekops doctor`: collect, then judge.
//!
//! `probe` performs every I/O and records each outcome into `Facts`, where
//! nothing is an early return. `evaluate` is a pure function over `Facts`
//! holding every threshold and every opinion, with no I/O at all.
//!
//! The split is what makes the diagnostic behaviour testable: "what does
//! doctor report for a Rocket Pool node whose consensus container is
//! restarting" is a unit test over a struct literal, on every stack, with
//! nothing installed.

use crate::beaconapi::{BeaconClient, FinalityCheckpoints, HealthState, PeerInfo, SyncingStatus};
use crate::docker::ContainerState;
use crate::host::{Disk, Load, Memory};
use crate::http::ApiError;
use crate::metrics::{DutiesMetrics, EndpointFamilies, MetricsClient, ValidatorMetrics};
use crate::output::plural;
use crate::stack::Stack;
use serde::Serialize;
use std::path::PathBuf;
use std::process::Command;

// Thresholds.
//
// Three checks below are not judgement calls: el_offline, an optimistic head,
// and a container that is not running each mean the node is not doing its job.
// finality-lag is anchored to the protocol, which finalizes exactly two epochs
// behind under normal operation.
//
// The constants here are the ones that ARE judgement calls. They were picked
// for a home-staker mainnet node and reviewed on the explicit understanding
// that they are tunable. They live here, named, precisely so tuning them is a
// one-line change and not an archaeology exercise inside `evaluate`.

/// Teku's own default lower bound is 64; well under a third of that is a real
/// connectivity problem rather than a quiet hour.
const PEERS_WARN_BELOW: usize = 20;

/// Finality is two epochs behind when healthy, so 3 absorbs a boundary.
const FINALITY_WARN_ABOVE: u64 = 3;
const FINALITY_FAIL_ABOVE: u64 = 6;

const SLOTS_PER_EPOCH: u64 = 32;

const GIB: u64 = 1024 * 1024 * 1024;
const DISK_WARN_BELOW: u64 = 50 * GIB;
const DISK_FAIL_BELOW: u64 = 20 * GIB;

/// A warning and never a failure: Teku's heap is preallocated, so low
/// available memory is the normal steady state of a correctly sized node.
const MEM_WARN_BELOW: u64 = GIB;

/// Restart count alone, with no uptime arithmetic. Turning Docker's RFC3339
/// StartedAt into a duration needs either a new dependency or a hand-rolled
/// calendar, and the restart count already carries the signal.
const RESTARTS_FAIL_AT: u64 = 5;

/// A load average at the core count means fully busy; twice that means work
/// is queueing faster than the box retires it.
const LOAD_WARN_ABOVE_PER_CPU: f64 = 1.0;
const LOAD_FAIL_ABOVE_PER_CPU: f64 = 2.0;

/// Bare-metal, or a stack that is not known at all: there was never a
/// container to look for. Distinct from `Probe::Failed`, which means Docker
/// was asked and did not answer, and from a Docker stack's own
/// `Probe::Ok(vec![])`, which means Docker was asked and matched nothing.
const NO_CONTAINER: &str = "no container for this stack";

/// The outcome of one piece of I/O.
///
/// `Skipped` is distinct from `Failed` on purpose. Doctor makes seven network
/// calls at a 10 second timeout each; against a dead node, attempting them all
/// is 70 seconds of silence on the command an operator reaches for *because*
/// the node is sick. So the first failure per endpoint short-circuits the rest,
/// and "skipped, the API is down" is both faster and more informative than
/// five identical timeouts.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "state", content = "value")]
pub enum Probe<T> {
    Ok(T),
    Failed(String),
    Skipped(&'static str),
}

impl<T> Probe<T> {
    pub fn from_result(r: Result<T, ApiError>) -> Self {
        match r {
            Ok(v) => Probe::Ok(v),
            // Sanitized here, once, at the boundary: this text came from the
            // node and ends up printed to the operator's terminal.
            Err(e) => Probe::Failed(crate::term::sanitize(&e.to_string())),
        }
    }

    pub fn ok(&self) -> Option<&T> {
        match self {
            Probe::Ok(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_unreachable(&self) -> bool {
        matches!(self, Probe::Failed(m) if m.contains("could not reach endpoint"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub name: &'static str,
    pub status: Status,
    /// Already sanitized. Everything interpolated here came from the node,
    /// from Docker, or from the filesystem, and none of it is trusted.
    pub detail: String,
}

impl Finding {
    pub fn new(name: &'static str, status: Status, detail: impl AsRef<str>) -> Self {
        Finding {
            name,
            status,
            detail: crate::term::sanitize(detail.as_ref()),
        }
    }
}

/// Everything doctor managed to learn. Every field is a recorded outcome and
/// never an early return: a doctor command that aborts on its first failure is
/// useless at exactly the moment it is needed.
#[derive(Debug, Serialize)]
pub struct Facts {
    pub stack: Option<Stack>,
    pub api_url: String,
    pub bn_metric_url: String,
    pub vc_metric_url: String,
    pub os: &'static str,
    pub arch: &'static str,

    pub health: Probe<HealthState>,
    pub syncing: Probe<SyncingStatus>,
    pub finality: Probe<FinalityCheckpoints>,
    pub peers: Probe<Vec<PeerInfo>>,
    pub bn_families: Probe<EndpointFamilies>,
    pub vc_families: Probe<EndpointFamilies>,
    pub duties: Probe<DutiesMetrics>,
    pub validators: Probe<ValidatorMetrics>,

    /// `Probe::Skipped(NO_CONTAINER)` on bare-metal, where there is no
    /// container to inspect and the question does not apply. On a Docker
    /// stack, `Probe::Ok` with an empty `Vec` is itself a finding (Docker was
    /// asked and matched no consensus container), distinct from
    /// `Probe::Failed` (Docker was asked about a named container and did not
    /// answer).
    pub containers: Probe<Vec<ContainerState>>,
    pub disk: Option<Disk>,
    pub memory: Option<Memory>,
    pub load: Option<Load>,
    pub cpus: Option<usize>,
}

/// Every opinion doctor holds, as a pure function of what it managed to learn.
///
/// A check whose input is missing is omitted from the result rather than
/// rendered as a placeholder row. That is deliberate on two axes: a host fact
/// that could not be read says nothing, and a container check on a bare-metal
/// node is a question that does not apply.
pub fn evaluate(f: &Facts) -> Vec<Finding> {
    let mut out = Vec::new();
    check_beacon_api(f, &mut out);
    check_bn_metrics(f, &mut out);
    check_vc_metrics(f, &mut out);
    check_metrics_layout(f, &mut out);
    check_syncing(f, &mut out);
    check_finality(f, &mut out);
    check_peers(f, &mut out);
    check_validators(f, &mut out);
    check_duties(f, &mut out);
    check_containers(f, &mut out);
    check_host(f, &mut out);
    out
}

fn push(out: &mut Vec<Finding>, name: &'static str, status: Status, detail: impl AsRef<str>) {
    out.push(Finding::new(name, status, detail));
}

fn check_beacon_api(f: &Facts, out: &mut Vec<Finding>) {
    match &f.health {
        Probe::Ok(HealthState::Ready) => push(
            out,
            "beacon api",
            Status::Pass,
            format!("ready at {}", f.api_url),
        ),
        Probe::Ok(HealthState::Syncing) => push(
            out,
            "beacon api",
            Status::Warn,
            format!("syncing at {}", f.api_url),
        ),
        Probe::Ok(HealthState::NotReady) => push(
            out,
            "beacon api",
            Status::Fail,
            format!("not ready at {}", f.api_url),
        ),
        Probe::Failed(e) => push(out, "beacon api", Status::Fail, e),
        Probe::Skipped(why) => push(out, "beacon api", Status::Fail, *why),
    }
}

fn check_bn_metrics(f: &Facts, out: &mut Vec<Finding>) {
    check_one_metrics_endpoint(
        "beacon node metrics",
        &f.bn_families,
        &f.bn_metric_url,
        |e| &e.beacon_versions,
        out,
    );
}

fn check_vc_metrics(f: &Facts, out: &mut Vec<Finding>) {
    check_one_metrics_endpoint(
        "validator metrics",
        &f.vc_families,
        &f.vc_metric_url,
        |e| &e.validator_versions,
        out,
    );
}

/// One endpoint's check. Reachable-but-wrong is a Warn rather than a Fail: the
/// node may be perfectly healthy and only tekops pointed somewhere odd, and
/// `check_metrics_layout` below often has a specific explanation for it.
fn check_one_metrics_endpoint(
    name: &'static str,
    probe: &Probe<EndpointFamilies>,
    url: &str,
    pick: fn(&EndpointFamilies) -> &Vec<String>,
    out: &mut Vec<Finding>,
) {
    match probe {
        Probe::Ok(e) if !pick(e).is_empty() => push(
            out,
            name,
            Status::Pass,
            format!("{} at {url}", pick(e).join(", ")),
        ),
        Probe::Ok(_) => push(
            out,
            name,
            Status::Warn,
            format!("{url} responded but exports no matching Teku version metric"),
        ),
        Probe::Failed(e) => push(out, name, Status::Fail, e),
        Probe::Skipped(why) => push(out, name, Status::Fail, *why),
    }
}

/// The two ways a metrics endpoint pair can be wrong that tekops can name.
///
/// Both read which families each endpoint actually exports, and that is what
/// tells them apart: a process serving both roles exports the beacon families
/// *and* the validator ones, while a validator client exports only the
/// validator ones. So the pair is mutually exclusive by construction, and
/// `the_two_layout_diagnostics_are_mutually_exclusive` pins that.
///
/// Nothing is repointed automatically. A heuristic that silently moved an
/// operator's endpoint would be the same class of mistake `require_metric`
/// exists to prevent, so this only ever advises.
fn check_metrics_layout(f: &Facts, out: &mut Vec<Finding>) {
    let Probe::Ok(bn) = &f.bn_families else {
        return;
    };
    // `has_validator_families` alone is too narrow a gate: a validator client
    // with no keys loaded exports no children of `validator_local_validator_counts`
    // at all - this repo's own absent-is-not-zero premise, already applied to
    // the sibling balances family in `metrics.rs`. `validator_versions` is a
    // second, independent signal of the same family that survives that case,
    // so either one is enough to proceed.
    if !bn.has_validator_families && bn.validator_versions.is_empty() {
        return;
    }

    if bn.beacon_versions.is_empty() {
        push(
            out,
            "metrics layout",
            Status::Warn,
            format!(
                "{} is a validator client endpoint, not a beacon node one. \
                 The unprefixed `metric_url` now means the beacon node; move \
                 this value to `vc_metric_url` (or $TEKOPS_VC_METRIC_URL, or \
                 --vc-metric-url).",
                f.bn_metric_url
            ),
        );
    } else if f.vc_families.is_unreachable() {
        push(
            out,
            "metrics layout",
            Status::Warn,
            format!(
                "{} exports both beacon and validator metrics and {} is \
                 unreachable: this looks like an all-in-one deployment. Set \
                 `vc_metric_url` to {}.",
                f.bn_metric_url, f.vc_metric_url, f.bn_metric_url
            ),
        );
    }
}

fn check_syncing(f: &Facts, out: &mut Vec<Finding>) {
    // A probe that was attempted and errored is a diagnosis in itself, not an
    // absence: unlike a host fact that was never measured, silence here would
    // hide that the beacon API answered `health` but not `syncing`. The
    // outage is already `beacon api`'s to report as a Fail, so this warns
    // rather than failing a second time for one cause.
    let s = match &f.syncing {
        Probe::Ok(s) => s,
        Probe::Failed(e) => {
            push(out, "sync status", Status::Warn, e);
            push(out, "execution layer", Status::Warn, e);
            push(out, "optimistic head", Status::Warn, e);
            return;
        }
        Probe::Skipped(why) => {
            push(out, "sync status", Status::Warn, *why);
            push(out, "execution layer", Status::Warn, *why);
            push(out, "optimistic head", Status::Warn, *why);
            return;
        }
    };
    if s.is_syncing {
        // sync_distance arrives as a Beacon API string, not a number - parsed
        // here only to pick a/the word, with the original string kept as a
        // fallback so an unparseable value still renders rather than vanishing.
        let detail = match s.sync_distance.parse::<usize>() {
            Ok(n) => format!("syncing, {} behind", plural(n, "slot")),
            Err(_) => format!("syncing, {} slots behind", s.sync_distance),
        };
        push(out, "sync status", Status::Warn, detail);
    } else {
        push(
            out,
            "sync status",
            Status::Pass,
            format!("synced, head {}", s.head_slot),
        );
    }

    // Binary facts, not judgement calls: either of these means the node is not
    // doing its job.
    if s.el_offline {
        push(
            out,
            "execution layer",
            Status::Fail,
            "the beacon node reports its execution client is offline",
        );
    } else {
        push(out, "execution layer", Status::Pass, "online");
    }

    if s.is_optimistic {
        push(
            out,
            "optimistic head",
            Status::Fail,
            "head unverified by the execution client; duties will not be performed",
        );
    } else {
        push(out, "optimistic head", Status::Pass, "head is verified");
    }
}

fn check_finality(f: &Facts, out: &mut Vec<Finding>) {
    // Same reasoning as `check_syncing`: a probe that errored is a finding
    // (Warn - the outage is already reported elsewhere), not silence.
    let s = match &f.syncing {
        Probe::Ok(s) => s,
        Probe::Failed(e) => return push(out, "finality lag", Status::Warn, e),
        Probe::Skipped(why) => return push(out, "finality lag", Status::Warn, *why),
    };
    let fin = match &f.finality {
        Probe::Ok(fin) => fin,
        Probe::Failed(e) => return push(out, "finality lag", Status::Warn, e),
        Probe::Skipped(why) => return push(out, "finality lag", Status::Warn, *why),
    };
    // Unlike an absent input, an unparseable one is not silence: it is the
    // one branch in `evaluate` where a check could otherwise vanish for a
    // reason other than "the input was absent" (see the module's rule at the
    // top of `evaluate`). A non-numeric head slot or finalized epoch is not a
    // real Beacon API failure mode, but naming it beats disappearing.
    let head = match s.head_slot.parse::<u64>() {
        Ok(v) => v,
        Err(_) => {
            return push(
                out,
                "finality lag",
                Status::Warn,
                format!("head slot {:?} is not a number", s.head_slot),
            )
        }
    };
    let finalized = match fin.finalized_epoch.parse::<u64>() {
        Ok(v) => v,
        Err(_) => {
            return push(
                out,
                "finality lag",
                Status::Warn,
                format!("finalized epoch {:?} is not a number", fin.finalized_epoch),
            )
        }
    };
    let lag = (head / SLOTS_PER_EPOCH).saturating_sub(finalized);
    let status = if lag > FINALITY_FAIL_ABOVE {
        Status::Fail
    } else if lag > FINALITY_WARN_ABOVE {
        Status::Warn
    } else {
        Status::Pass
    };
    push(out, "finality lag", status, plural(lag as usize, "epoch"));
}

fn check_peers(f: &Facts, out: &mut Vec<Finding>) {
    // Same reasoning as `check_syncing`: a probe that errored is a finding
    // (Warn - the outage is already reported elsewhere), not silence.
    let p = match &f.peers {
        Probe::Ok(p) => p,
        Probe::Failed(e) => return push(out, "peer count", Status::Warn, e),
        Probe::Skipped(why) => return push(out, "peer count", Status::Warn, *why),
    };
    let n = p.len();
    let status = if n == 0 {
        Status::Fail
    } else if n < PEERS_WARN_BELOW {
        Status::Warn
    } else {
        Status::Pass
    };
    let word = plural(n, "peer");
    let detail = if status == Status::Pass {
        word
    } else {
        format!("{word} (want >= {PEERS_WARN_BELOW})")
    };
    push(out, "peer count", status, detail);
}

fn check_validators(f: &Facts, out: &mut Vec<Finding>) {
    match &f.validators {
        Probe::Ok(v) => {
            let active = v
                .counts_by_status
                .get("active_ongoing")
                .copied()
                .unwrap_or(0);
            if active > 0 {
                // Absent, not zero: the ETH clause is omitted entirely rather
                // than printing `0.00 ETH` when the scrape has no balances
                // family, so a working validator client that just doesn't
                // export balances is not misread as holding nothing.
                let detail = match v.total_eth {
                    Some(eth) => format!("{active} active_ongoing, {eth:.2} ETH"),
                    None => format!("{active} active_ongoing"),
                };
                push(out, "validator keys", Status::Pass, detail);
            } else {
                let breakdown: Vec<String> = v
                    .counts_by_status
                    .iter()
                    .map(|(k, n)| format!("{k}={n}"))
                    .collect();
                let detail = if breakdown.is_empty() {
                    "no validator keys loaded".to_string()
                } else {
                    format!("no active_ongoing keys ({})", breakdown.join(", "))
                };
                push(out, "validator keys", Status::Warn, detail);
            }
        }
        // Absent is not zero. `require_metric`'s message already names the URL
        // and says the endpoint exports no such metric, which is the whole
        // point: this is a wrong-port diagnosis, not a claim about duties.
        Probe::Failed(e) => push(out, "validator keys", Status::Warn, e),
        Probe::Skipped(why) => push(out, "validator keys", Status::Warn, *why),
    }
}

fn check_duties(f: &Facts, out: &mut Vec<Finding>) {
    match &f.duties {
        Probe::Ok(d) => {
            let total = d.published_blocks
                + d.published_attestations
                + d.published_sync_committee_messages
                + d.published_aggregates;
            if total > 0 {
                push(
                    out,
                    "duties published",
                    Status::Pass,
                    format!(
                        "{}, {}",
                        plural(d.published_attestations as usize, "attestation"),
                        plural(d.published_blocks as usize, "block")
                    ),
                );
            } else {
                // Never a failure: a validator client restarted thirty seconds
                // ago legitimately reports zero.
                push(
                    out,
                    "duties published",
                    Status::Warn,
                    "none yet (normal if the validator client restarted recently)",
                );
            }
        }
        Probe::Failed(e) => push(out, "duties published", Status::Warn, e),
        Probe::Skipped(why) => push(out, "duties published", Status::Warn, *why),
    }
}

fn check_containers(f: &Facts, out: &mut Vec<Finding>) {
    // The question does not apply on bare-metal (or when the stack itself is
    // unknown): there is no container to inspect.
    match f.stack {
        Some(Stack::BareMetal) | None => return,
        Some(Stack::EthDocker) | Some(Stack::RocketPool) => {}
    }

    // Unlike bare-metal, an empty listing on a Docker stack is itself the
    // finding: `probe` asked Docker for a consensus container and matched
    // none, which is exactly the failure this command exists to catch, not
    // silence that could be misread as "containers weren't the problem".
    let containers = match &f.containers {
        Probe::Ok(v) if v.is_empty() => {
            return push(
                out,
                "consensus container",
                Status::Fail,
                "no consensus container found; is the stack running?",
            );
        }
        Probe::Ok(v) => v,
        Probe::Failed(e) => return push(out, "consensus container", Status::Fail, e),
        // Unreachable in practice: the bare-metal/unknown-stack match above
        // already returned before this point, and a Docker stack's `probe`
        // never produces `Skipped` (see `probe`'s `containers` match). Kept
        // only for exhaustiveness, the same reasoning as the dead `Skipped`
        // arms in `check_beacon_api`/`check_metrics_endpoint`.
        Probe::Skipped(why) => return push(out, "consensus container", Status::Fail, *why),
    };

    let stopped: Vec<&str> = containers
        .iter()
        .filter(|c| !c.running)
        .map(|c| c.name.as_str())
        .collect();
    if stopped.is_empty() {
        let names: Vec<&str> = containers.iter().map(|c| c.name.as_str()).collect();
        push(
            out,
            "consensus container",
            Status::Pass,
            format!("{} up", names.join(", ")),
        );
    } else {
        push(
            out,
            "consensus container",
            Status::Fail,
            format!("not running: {}", stopped.join(", ")),
        );
    }

    let worst = containers.iter().max_by_key(|c| c.restart_count);
    if let Some(c) = worst {
        let status = if c.restart_count >= RESTARTS_FAIL_AT {
            Status::Fail
        } else if c.restart_count > 0 {
            Status::Warn
        } else {
            Status::Pass
        };
        let detail = if c.restart_count == 0 {
            "0".to_string()
        } else {
            format!(
                "{} on {}",
                plural(c.restart_count as usize, "restart"),
                c.name
            )
        };
        push(out, "container restarts", status, detail);
    }
}

fn check_host(f: &Facts, out: &mut Vec<Finding>) {
    if let Some(d) = &f.disk {
        let status = if d.available_bytes < DISK_FAIL_BELOW {
            Status::Fail
        } else if d.available_bytes < DISK_WARN_BELOW {
            Status::Warn
        } else {
            Status::Pass
        };
        push(
            out,
            "disk free",
            status,
            format!("{} on {}", gib(d.available_bytes), d.mount_point),
        );
    }

    if let Some(m) = &f.memory {
        // Warn, never fail: Teku preallocates its heap.
        let status = if m.available_bytes < MEM_WARN_BELOW {
            Status::Warn
        } else {
            Status::Pass
        };
        push(
            out,
            "memory available",
            status,
            format!("{} of {}", gib(m.available_bytes), gib(m.total_bytes)),
        );
    }

    if let (Some(l), Some(cpus)) = (&f.load, f.cpus) {
        let cpus_f = cpus as f64;
        let status = if l.one > cpus_f * LOAD_FAIL_ABOVE_PER_CPU {
            Status::Fail
        } else if l.one > cpus_f * LOAD_WARN_ABOVE_PER_CPU {
            Status::Warn
        } else {
            Status::Pass
        };
        push(
            out,
            "load average",
            status,
            format!("{:.2} ({})", l.one, plural(cpus, "cpu")),
        );
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

pub struct ProbeConfig {
    pub stack: Option<Stack>,
    pub api_url: String,
    pub bn_metric_url: String,
    pub vc_metric_url: String,
    /// The consensus container to inspect, when there is one.
    pub container: Option<String>,
    /// Bare-metal disk target. On a Docker stack the container's own mounts
    /// answer this, and a value here overrides them (flags beat detection).
    pub data_dir: Option<PathBuf>,
}

const API_DOWN: &str = "skipped: the beacon api is unreachable";
const METRICS_DOWN: &str = "skipped: the metrics endpoint is unreachable";

/// Every I/O doctor performs, and nothing else.
///
/// Nothing here returns early. The short-circuits are per endpoint and are a
/// latency bound, not a control-flow shortcut: seven calls at a 10 second
/// timeout is 70 seconds of silence against a dead node.
pub fn probe(cfg: &ProbeConfig) -> Facts {
    let bn = BeaconClient::new(cfg.api_url.clone());
    let health = Probe::from_result(bn.health());

    let (syncing, finality, peers) = if health.is_unreachable() {
        (
            Probe::Skipped(API_DOWN),
            Probe::Skipped(API_DOWN),
            Probe::Skipped(API_DOWN),
        )
    } else {
        (
            Probe::from_result(bn.syncing()),
            Probe::from_result(bn.finality_checkpoints()),
            Probe::from_result(bn.peers()),
        )
    };

    let bn_families = Probe::from_result(MetricsClient::new(cfg.bn_metric_url.clone()).families());

    // Equal URLs mean one process serving both families. Scrape once and hand
    // the same answer to both slots rather than asking the same endpoint
    // twice: `EndpointFamilies` is `Clone` precisely for this.
    let vc_mc = MetricsClient::new(cfg.vc_metric_url.clone());
    let vc_families = if cfg.bn_metric_url == cfg.vc_metric_url {
        match &bn_families {
            Probe::Ok(f) => Probe::Ok(f.clone()),
            Probe::Failed(e) => Probe::Failed(e.clone()),
            Probe::Skipped(s) => Probe::Skipped(s),
        }
    } else {
        Probe::from_result(vc_mc.families())
    };

    let (duties, validators) = if vc_families.is_unreachable() {
        (Probe::Skipped(METRICS_DOWN), Probe::Skipped(METRICS_DOWN))
    } else {
        (
            Probe::from_result(vc_mc.duties()),
            Probe::from_result(vc_mc.validators()),
        )
    };

    // `Probe`, not a bare Vec: an empty list on a Docker stack means "docker
    // was asked and matched nothing", which is a reportable failure, and it
    // must stay distinguishable from bare-metal's "there was never anything
    // to list" - the two are told apart by `cfg.stack`, not by
    // `cfg.container` alone, since a Docker stack whose detection found
    // nothing also arrives with `cfg.container: None`.
    let containers: Probe<Vec<ContainerState>> = match (cfg.stack, cfg.container.as_deref()) {
        (Some(Stack::BareMetal), _) | (None, _) => Probe::Skipped(NO_CONTAINER),
        (_, None) => Probe::Ok(vec![]),
        (_, Some(name)) => match inspect_container(name) {
            Some(c) => Probe::Ok(vec![c]),
            None => Probe::Failed(crate::term::sanitize(&format!(
                "docker inspect {name} failed or returned nothing"
            ))),
        },
    };

    // Flags beat detection: an explicit --data-dir wins even on a stack whose
    // container could have answered.
    let data_dir: Option<PathBuf> = cfg.data_dir.clone().or_else(|| {
        containers
            .ok()
            .and_then(|v| v.first())
            .and_then(crate::docker::data_mount)
            .map(PathBuf::from)
    });
    let host = crate::host::collect(data_dir.as_deref());

    Facts {
        stack: cfg.stack,
        api_url: cfg.api_url.clone(),
        bn_metric_url: cfg.bn_metric_url.clone(),
        vc_metric_url: cfg.vc_metric_url.clone(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        health,
        syncing,
        finality,
        peers,
        bn_families,
        vc_families,
        duties,
        validators,
        containers,
        disk: host.disk,
        memory: host.memory,
        load: host.load,
        cpus: host.cpus,
    }
}

fn inspect_container(name: &str) -> Option<ContainerState> {
    let argv = crate::docker::inspect_argv(name);
    let out = Command::new(&argv[0]).args(&argv[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    crate::docker::parse_inspect(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn syncing(is_syncing: bool, distance: &str) -> SyncingStatus {
        SyncingStatus {
            is_syncing,
            is_optimistic: false,
            el_offline: false,
            head_slot: "3200".to_string(),
            sync_distance: distance.to_string(),
        }
    }

    /// A node where every check passes, so each test can change one thing.
    fn healthy() -> Facts {
        Facts {
            stack: Some(Stack::BareMetal),
            api_url: "http://localhost:5051".to_string(),
            bn_metric_url: "http://localhost:8008/metrics".to_string(),
            vc_metric_url: "http://localhost:8010/metrics".to_string(),
            os: "linux",
            arch: "x86_64",
            health: Probe::Ok(HealthState::Ready),
            syncing: Probe::Ok(syncing(false, "0")),
            finality: Probe::Ok(FinalityCheckpoints {
                previous_justified_epoch: "98".to_string(),
                current_justified_epoch: "99".to_string(),
                finalized_epoch: "98".to_string(),
            }),
            peers: Probe::Ok(peers(30)),
            bn_families: Probe::Ok(EndpointFamilies {
                beacon_versions: vec!["teku/v25.1.0".to_string()],
                validator_versions: vec![],
                has_validator_families: false,
            }),
            vc_families: Probe::Ok(EndpointFamilies {
                beacon_versions: vec![],
                validator_versions: vec!["teku/v25.1.0".to_string()],
                has_validator_families: true,
            }),
            duties: Probe::Ok(DutiesMetrics {
                published_blocks: 1,
                published_attestations: 10,
                published_sync_committee_messages: 0,
                published_aggregates: 0,
            }),
            validators: Probe::Ok(ValidatorMetrics {
                counts_by_status: BTreeMap::from([("active_ongoing".to_string(), 142)]),
                total_eth: Some(4544.0),
            }),
            containers: Probe::Skipped(NO_CONTAINER),
            disk: Some(Disk {
                available_bytes: 400 * GIB,
                mount_point: "/var/lib/teku".to_string(),
            }),
            memory: Some(Memory {
                total_bytes: 32 * GIB,
                available_bytes: 8 * GIB,
            }),
            load: Some(Load {
                one: 1.0,
                five: 1.0,
                fifteen: 1.0,
            }),
            cpus: Some(8),
        }
    }

    fn status_of(fs: &[Finding], name: &str) -> Option<Status> {
        fs.iter().find(|f| f.name == name).map(|f| f.status)
    }

    fn peers(n: usize) -> Vec<PeerInfo> {
        (0..n)
            .map(|i| PeerInfo {
                peer_id: format!("p{i}"),
                last_seen_p2p_address: "/ip4/1.2.3.4/tcp/9000".to_string(),
                state: "connected".to_string(),
                direction: "outbound".to_string(),
            })
            .collect()
    }

    #[test]
    fn a_healthy_node_has_no_failures_or_warnings() {
        let f = healthy();
        let got = evaluate(&f);
        assert!(
            got.iter().all(|x| x.status == Status::Pass),
            "unexpected non-pass: {:?}",
            got.iter()
                .filter(|x| x.status != Status::Pass)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn beacon_api_fails_when_unreachable() {
        let mut f = healthy();
        f.health = Probe::Failed("could not reach endpoint: refused".to_string());
        assert_eq!(status_of(&evaluate(&f), "beacon api"), Some(Status::Fail));
    }

    #[test]
    fn beacon_api_warns_while_syncing() {
        let mut f = healthy();
        f.health = Probe::Ok(HealthState::Syncing);
        assert_eq!(status_of(&evaluate(&f), "beacon api"), Some(Status::Warn));
    }

    #[test]
    fn execution_layer_fails_when_el_offline() {
        let mut f = healthy();
        let mut s = syncing(false, "0");
        s.el_offline = true;
        f.syncing = Probe::Ok(s);
        assert_eq!(
            status_of(&evaluate(&f), "execution layer"),
            Some(Status::Fail)
        );
    }

    #[test]
    fn optimistic_head_fails_when_the_head_is_unverified() {
        let mut f = healthy();
        let mut s = syncing(false, "0");
        s.is_optimistic = true;
        f.syncing = Probe::Ok(s);
        assert_eq!(
            status_of(&evaluate(&f), "optimistic head"),
            Some(Status::Fail)
        );
    }

    #[test]
    fn sync_status_warns_while_syncing_and_shows_the_distance() {
        let mut f = healthy();
        f.syncing = Probe::Ok(syncing(true, "412"));
        let got = evaluate(&f);
        assert_eq!(status_of(&got, "sync status"), Some(Status::Warn));
        let detail = &got.iter().find(|x| x.name == "sync status").unwrap().detail;
        assert!(detail.contains("412"), "detail was {detail:?}");
    }

    /// "1 slots behind" reads as broken English. `sync_distance` arrives as a
    /// Beacon API string, not a number, which is exactly the kind of value
    /// that's easy to interpolate straight into a unit word without noticing
    /// it can be singular.
    #[test]
    fn sync_status_detail_is_singular_for_one_slot_behind() {
        let mut f = healthy();
        f.syncing = Probe::Ok(syncing(true, "1"));
        let got = evaluate(&f);
        let detail = &got.iter().find(|x| x.name == "sync status").unwrap().detail;
        assert_eq!(detail, "syncing, 1 slot behind");
    }

    /// Finality runs exactly two epochs behind when healthy, so the boundary
    /// between "normal" and "worth mentioning" has to be tested on both sides.
    #[test]
    fn finality_lag_boundaries() {
        // head 3200 -> epoch 100.
        let cases = [
            (97u64, Status::Pass),
            (96, Status::Warn),
            (93, Status::Fail),
        ];
        for (finalized, want) in cases {
            let mut f = healthy();
            f.finality = Probe::Ok(FinalityCheckpoints {
                previous_justified_epoch: "99".to_string(),
                current_justified_epoch: "99".to_string(),
                finalized_epoch: finalized.to_string(),
            });
            assert_eq!(
                status_of(&evaluate(&f), "finality lag"),
                Some(want),
                "finalized epoch {finalized}"
            );
        }
    }

    /// "1 epochs" reads as broken English. Reuses `output::plural`, which the
    /// summary line ("1 failure, 3 warnings") already relies on getting right.
    #[test]
    fn finality_lag_detail_is_singular_for_one_epoch() {
        let mut f = healthy();
        // head 3200 -> epoch 100; finalized 99 -> lag 1.
        f.finality = Probe::Ok(FinalityCheckpoints {
            previous_justified_epoch: "99".to_string(),
            current_justified_epoch: "99".to_string(),
            finalized_epoch: "99".to_string(),
        });
        let got = evaluate(&f);
        let detail = &got
            .iter()
            .find(|x| x.name == "finality lag")
            .unwrap()
            .detail;
        assert_eq!(detail, "1 epoch");
    }

    /// An unparseable value is not the same as an absent one: it must warn
    /// and name the value, not silently vanish the row the way a genuinely
    /// missing probe result does.
    #[test]
    fn finality_lag_warns_and_names_the_value_when_head_slot_does_not_parse() {
        let mut f = healthy();
        f.syncing = Probe::Ok(SyncingStatus {
            head_slot: "not-a-number".to_string(),
            ..syncing(false, "0")
        });
        let got = evaluate(&f);
        assert_eq!(status_of(&got, "finality lag"), Some(Status::Warn));
        let detail = &got
            .iter()
            .find(|x| x.name == "finality lag")
            .unwrap()
            .detail;
        assert!(detail.contains("not-a-number"), "got: {detail:?}");
    }

    #[test]
    fn finality_lag_warns_and_names_the_value_when_finalized_epoch_does_not_parse() {
        let mut f = healthy();
        f.finality = Probe::Ok(FinalityCheckpoints {
            previous_justified_epoch: "99".to_string(),
            current_justified_epoch: "99".to_string(),
            finalized_epoch: "not-a-number".to_string(),
        });
        let got = evaluate(&f);
        assert_eq!(status_of(&got, "finality lag"), Some(Status::Warn));
        let detail = &got
            .iter()
            .find(|x| x.name == "finality lag")
            .unwrap()
            .detail;
        assert!(detail.contains("not-a-number"), "got: {detail:?}");
    }

    #[test]
    fn peer_count_boundaries() {
        for (n, want) in [
            (20usize, Status::Pass),
            (19, Status::Warn),
            (0, Status::Fail),
        ] {
            let mut f = healthy();
            f.peers = Probe::Ok(peers(n));
            assert_eq!(
                status_of(&evaluate(&f), "peer count"),
                Some(want),
                "{n} peers"
            );
        }
    }

    #[test]
    fn peer_count_detail_is_singular_for_one_peer() {
        let mut f = healthy();
        f.peers = Probe::Ok(peers(1));
        let got = evaluate(&f);
        let detail = &got.iter().find(|x| x.name == "peer count").unwrap().detail;
        assert_eq!(detail, "1 peer (want >= 20)");
    }

    /// The documented past bug this must not reintroduce: absent metric
    /// families mean the scrape is pointed at the wrong process, NOT that the
    /// validator published nothing.
    #[test]
    fn absent_validator_metrics_warn_about_the_url_not_about_the_validator() {
        let mut f = healthy();
        f.validators = Probe::Failed(
            "metric validator_local_validator_counts not found at \
             http://localhost:8010/metrics - the endpoint responded but exports no such metric"
                .to_string(),
        );
        let got = evaluate(&f);
        assert_eq!(status_of(&got, "validator keys"), Some(Status::Warn));
        let detail = &got
            .iter()
            .find(|x| x.name == "validator keys")
            .unwrap()
            .detail;
        assert!(
            detail.contains("8010"),
            "detail must name the scraped url: {detail:?}"
        );
        assert!(
            !detail.contains("published nothing") && !detail.contains("0 "),
            "absent must not read as zero: {detail:?}"
        );
    }

    /// I3, at the render layer: an absent balances family must drop the ETH
    /// clause entirely rather than print a measured `0.00 ETH` behind a Pass.
    #[test]
    fn validator_keys_omits_the_eth_clause_when_the_balances_family_is_absent() {
        let mut f = healthy();
        f.validators = Probe::Ok(ValidatorMetrics {
            counts_by_status: BTreeMap::from([("active_ongoing".to_string(), 142)]),
            total_eth: None,
        });
        let got = evaluate(&f);
        assert_eq!(status_of(&got, "validator keys"), Some(Status::Pass));
        let detail = &got
            .iter()
            .find(|x| x.name == "validator keys")
            .unwrap()
            .detail;
        assert_eq!(detail, "142 active_ongoing");
    }

    #[test]
    fn duties_published_warns_at_zero_and_says_a_restart_explains_it() {
        let mut f = healthy();
        f.duties = Probe::Ok(DutiesMetrics {
            published_blocks: 0,
            published_attestations: 0,
            published_sync_committee_messages: 0,
            published_aggregates: 0,
        });
        let got = evaluate(&f);
        assert_eq!(status_of(&got, "duties published"), Some(Status::Warn));
        assert!(got
            .iter()
            .find(|x| x.name == "duties published")
            .unwrap()
            .detail
            .contains("restart"));
    }

    #[test]
    fn duties_published_detail_is_singular_for_one_of_each() {
        let mut f = healthy();
        f.duties = Probe::Ok(DutiesMetrics {
            published_blocks: 1,
            published_attestations: 1,
            published_sync_committee_messages: 0,
            published_aggregates: 0,
        });
        let got = evaluate(&f);
        let detail = &got
            .iter()
            .find(|x| x.name == "duties published")
            .unwrap()
            .detail;
        assert_eq!(detail, "1 attestation, 1 block");
    }

    #[test]
    fn disk_boundaries() {
        for (gib, want) in [
            (50u64, Status::Pass),
            (49, Status::Warn),
            (19, Status::Fail),
        ] {
            let mut f = healthy();
            f.disk = Some(Disk {
                available_bytes: gib * GIB,
                mount_point: "/var/lib/teku".to_string(),
            });
            assert_eq!(
                status_of(&evaluate(&f), "disk free"),
                Some(want),
                "{gib} GiB"
            );
        }
    }

    /// A warning and never a failure: Teku preallocates its heap, so a
    /// correctly sized node sits here normally.
    #[test]
    fn low_memory_warns_but_never_fails() {
        let mut f = healthy();
        f.memory = Some(Memory {
            total_bytes: 32 * GIB,
            available_bytes: GIB / 2,
        });
        assert_eq!(
            status_of(&evaluate(&f), "memory available"),
            Some(Status::Warn)
        );
    }

    #[test]
    fn load_average_boundaries() {
        for (one, want) in [
            (8.0, Status::Pass),
            (8.1, Status::Warn),
            (16.1, Status::Fail),
        ] {
            let mut f = healthy();
            f.load = Some(Load {
                one,
                five: 1.0,
                fifteen: 1.0,
            });
            assert_eq!(
                status_of(&evaluate(&f), "load average"),
                Some(want),
                "load {one}"
            );
        }
    }

    #[test]
    fn load_average_detail_is_singular_for_one_cpu() {
        let mut f = healthy();
        f.load = Some(Load {
            one: 1.0,
            five: 1.0,
            fifteen: 1.0,
        });
        f.cpus = Some(1);
        let got = evaluate(&f);
        let detail = &got
            .iter()
            .find(|x| x.name == "load average")
            .unwrap()
            .detail;
        assert_eq!(detail, "1.00 (1 cpu)");
    }

    #[test]
    fn a_missing_host_fact_omits_its_check_rather_than_guessing() {
        let mut f = healthy();
        f.memory = None;
        f.load = None;
        f.disk = None;
        let got = evaluate(&f);
        assert!(status_of(&got, "memory available").is_none());
        assert!(status_of(&got, "load average").is_none());
        assert!(status_of(&got, "disk free").is_none());
    }

    // --- cross-stack ---

    fn container(name: &str, running: bool, restarts: u64) -> ContainerState {
        ContainerState {
            name: name.to_string(),
            running,
            status: if running { "running" } else { "exited" }.to_string(),
            started_at: Some("2026-09-10T04:12:33Z".to_string()),
            restart_count: restarts,
            mounts: vec![],
        }
    }

    /// A bare-metal operator has no restart count and must never be shown a
    /// row about one - proven here by giving it a non-empty, healthy
    /// container list and asserting the rows are absent anyway. If this
    /// passed only because the list happened to be empty, it would not be
    /// testing the stack branch at all.
    #[test]
    fn bare_metal_omits_both_container_checks() {
        let mut f = healthy();
        f.containers = Probe::Ok(vec![container("some-unrelated-container", true, 0)]);
        let got = evaluate(&f);
        assert!(status_of(&got, "consensus container").is_none());
        assert!(status_of(&got, "container restarts").is_none());
    }

    #[test]
    fn docker_stacks_fail_when_a_container_is_not_running() {
        for (stack, name) in [
            (Stack::EthDocker, "eth-docker-consensus-1"),
            (Stack::RocketPool, "rocketpool_eth2"),
        ] {
            let mut f = healthy();
            f.stack = Some(stack);
            f.containers = Probe::Ok(vec![container(name, false, 0)]);
            let got = evaluate(&f);
            assert_eq!(
                status_of(&got, "consensus container"),
                Some(Status::Fail),
                "{stack:?}"
            );
            assert!(got
                .iter()
                .find(|x| x.name == "consensus container")
                .unwrap()
                .detail
                .contains(name));
        }
    }

    /// An empty listing on a Docker stack is itself the failure - no
    /// consensus container was found at all - and must not read as silence.
    /// This is the state `probe` actually produces once `cfg.container` is
    /// `None` on a Docker stack (I1): the wording asserted here is what an
    /// operator sees in practice, not an unreachable arm.
    #[test]
    fn docker_stack_with_no_container_found_fails() {
        for stack in [Stack::EthDocker, Stack::RocketPool] {
            let mut f = healthy();
            f.stack = Some(stack);
            f.containers = Probe::Ok(vec![]);
            let got = evaluate(&f);
            assert_eq!(
                status_of(&got, "consensus container"),
                Some(Status::Fail),
                "{stack:?}"
            );
            let detail = &got
                .iter()
                .find(|x| x.name == "consensus container")
                .unwrap()
                .detail;
            assert!(
                detail.contains("running"),
                "detail should ask whether the stack is running: {detail:?}"
            );
        }
    }

    /// Docker inspect failing outright (the container disappeared between
    /// `docker ps` and `docker inspect`, say) is a distinct outcome from
    /// finding nothing at all, and must still surface as a failure rather
    /// than vanish.
    #[test]
    fn docker_stack_fails_when_the_inspect_probe_itself_failed() {
        let mut f = healthy();
        f.stack = Some(Stack::EthDocker);
        f.containers = Probe::Failed("docker inspect eth-docker-consensus-1 failed".to_string());
        assert_eq!(
            status_of(&evaluate(&f), "consensus container"),
            Some(Status::Fail)
        );
    }

    #[test]
    fn container_restart_boundaries() {
        for (n, want) in [(0u64, Status::Pass), (1, Status::Warn), (5, Status::Fail)] {
            let mut f = healthy();
            f.stack = Some(Stack::RocketPool);
            f.containers = Probe::Ok(vec![container("rocketpool_eth2", true, n)]);
            assert_eq!(
                status_of(&evaluate(&f), "container restarts"),
                Some(want),
                "{n} restarts"
            );
        }
    }

    #[test]
    fn container_restarts_detail_is_singular_for_one_restart() {
        let mut f = healthy();
        f.stack = Some(Stack::RocketPool);
        f.containers = Probe::Ok(vec![container("rocketpool_eth2", true, 1)]);
        let got = evaluate(&f);
        let detail = &got
            .iter()
            .find(|x| x.name == "container restarts")
            .unwrap()
            .detail;
        assert_eq!(detail, "1 restart on rocketpool_eth2");
    }

    /// Findings are printed to a terminal, and container names and versions
    /// come from outside this binary.
    #[test]
    fn findings_are_sanitized() {
        let mut f = healthy();
        f.stack = Some(Stack::EthDocker);
        f.containers = Probe::Ok(vec![container("evil\u{1b}[2Jname", false, 0)]);
        let got = evaluate(&f);
        assert!(got.iter().all(|x| !x.detail.contains('\u{1b}')));
    }

    // --- Probe::Skipped and Probe::Failed on the dependent checks ---
    //
    // A probe that was attempted and errored is a diagnosis in itself, and
    // must render as a Warn row naming the reason, not vanish the way an
    // unmeasured host fact correctly does.

    #[test]
    fn sync_status_execution_layer_and_optimistic_head_warn_when_syncing_is_skipped() {
        let mut f = healthy();
        f.syncing = Probe::Skipped("beacon api unreachable");
        let got = evaluate(&f);
        assert_eq!(status_of(&got, "sync status"), Some(Status::Warn));
        assert_eq!(status_of(&got, "execution layer"), Some(Status::Warn));
        assert_eq!(status_of(&got, "optimistic head"), Some(Status::Warn));
    }

    #[test]
    fn sync_status_warns_rather_than_vanishing_when_syncing_failed() {
        let mut f = healthy();
        f.syncing = Probe::Failed("could not reach endpoint: refused".to_string());
        assert_eq!(status_of(&evaluate(&f), "sync status"), Some(Status::Warn));
    }

    #[test]
    fn finality_lag_warns_when_syncing_is_skipped() {
        let mut f = healthy();
        f.syncing = Probe::Skipped("beacon api unreachable");
        assert_eq!(status_of(&evaluate(&f), "finality lag"), Some(Status::Warn));
    }

    #[test]
    fn finality_lag_warns_when_the_finality_probe_itself_is_skipped() {
        let mut f = healthy();
        f.finality = Probe::Skipped("beacon api unreachable");
        assert_eq!(status_of(&evaluate(&f), "finality lag"), Some(Status::Warn));
    }

    #[test]
    fn peer_count_warns_when_skipped() {
        let mut f = healthy();
        f.peers = Probe::Skipped("beacon api unreachable");
        assert_eq!(status_of(&evaluate(&f), "peer count"), Some(Status::Warn));
    }

    #[test]
    fn peer_count_warns_rather_than_vanishing_when_peers_failed() {
        let mut f = healthy();
        f.peers = Probe::Failed("could not reach endpoint: refused".to_string());
        assert_eq!(status_of(&evaluate(&f), "peer count"), Some(Status::Warn));
    }

    // --- bn/vc metrics ---
    //
    // The only genuinely non-obvious status logic in the module: unreachable
    // is Fail, but reachable-and-erroring is Warn, since that almost always
    // means the scrape is pointed at the wrong process - a misconfiguration,
    // not an outage. `check_metrics_layout` below often names the reason.

    fn families(beacon: &[&str], validator: &[&str], has_vc: bool) -> EndpointFamilies {
        EndpointFamilies {
            beacon_versions: beacon.iter().map(|s| s.to_string()).collect(),
            validator_versions: validator.iter().map(|s| s.to_string()).collect(),
            has_validator_families: has_vc,
        }
    }

    fn finding<'a>(findings: &'a [Finding], name: &str) -> Option<&'a Finding> {
        findings.iter().find(|f| f.name == name)
    }

    #[test]
    fn bn_metrics_passes_when_a_version_is_reported() {
        let f = healthy();
        assert_eq!(
            status_of(&evaluate(&f), "beacon node metrics"),
            Some(Status::Pass)
        );
    }

    #[test]
    fn bn_metrics_fails_when_unreachable() {
        let mut f = healthy();
        f.bn_families = Probe::Failed("could not reach endpoint: refused".to_string());
        assert_eq!(
            status_of(&evaluate(&f), "beacon node metrics"),
            Some(Status::Fail)
        );
    }

    #[test]
    fn bn_metrics_warns_when_reachable_but_missing_the_metric() {
        let mut f = healthy();
        f.bn_families = Probe::Ok(families(&[], &[], false));
        assert_eq!(
            status_of(&evaluate(&f), "beacon node metrics"),
            Some(Status::Warn)
        );
    }

    #[test]
    fn bn_metrics_fails_when_skipped() {
        let mut f = healthy();
        f.bn_families = Probe::Skipped("beacon api unreachable");
        assert_eq!(
            status_of(&evaluate(&f), "beacon node metrics"),
            Some(Status::Fail)
        );
    }

    #[test]
    fn both_metrics_endpoints_are_checked_separately() {
        let findings = evaluate(&healthy());
        assert!(finding(&findings, "beacon node metrics").is_some());
        assert!(finding(&findings, "validator metrics").is_some());
    }

    #[test]
    fn a_dead_validator_endpoint_does_not_fail_the_beacon_one() {
        let mut f = healthy();
        f.vc_families = Probe::Failed("could not reach endpoint".to_string());
        let findings = evaluate(&f);
        assert_eq!(
            finding(&findings, "beacon node metrics").unwrap().status,
            Status::Pass
        );
        assert_eq!(
            finding(&findings, "validator metrics").unwrap().status,
            Status::Fail
        );
    }

    /// One process exporting both families, with nothing on the validator
    /// port. eth-docker's `teku-allin1.yml` produces exactly this, and its
    /// defaults (8008 and 8009) are not equal, so nothing else catches it.
    #[test]
    fn an_all_in_one_deployment_is_recognised_and_named() {
        let mut f = healthy();
        f.bn_families = Probe::Ok(families(&["teku/v25.4.1"], &["teku/v25.4.1"], true));
        f.vc_families = Probe::Failed("could not reach endpoint".to_string());
        let findings = evaluate(&f);
        let layout = finding(&findings, "metrics layout").expect("expected a layout finding");
        assert_eq!(layout.status, Status::Warn);
        assert!(layout.detail.contains("all-in-one"), "{}", layout.detail);
        assert!(
            layout.detail.contains("vc_metric_url"),
            "must name the fix: {}",
            layout.detail
        );
    }

    /// A validator endpoint sitting in the beacon node's slot. No correct
    /// configuration produces this, so it is almost always a config written
    /// when `metric_url` still meant the validator client.
    #[test]
    fn a_validator_endpoint_in_the_beacon_slot_is_recognised_and_named() {
        let mut f = healthy();
        f.bn_families = Probe::Ok(families(&[], &["teku/v25.4.1"], true));
        let findings = evaluate(&f);
        let layout = finding(&findings, "metrics layout").expect("expected a layout finding");
        assert_eq!(layout.status, Status::Warn);
        assert!(
            layout.detail.contains("vc_metric_url"),
            "must name the fix: {}",
            layout.detail
        );
        assert!(
            !layout.detail.contains("all-in-one"),
            "must not be confused with an all-in-one deployment: {}",
            layout.detail
        );
    }

    /// A validator client with no keys loaded exports none of
    /// `validator_local_validator_counts`'s children, so `has_validator_families`
    /// is false even though the process is unmistakably a validator client:
    /// `beacon_versions` is empty and `validator_versions` is populated. The
    /// old `!bn.has_validator_families` gate returned before looking at
    /// either, which is exactly the absent-is-not-zero bug this repo already
    /// avoids for the sibling balances family.
    #[test]
    fn a_keyless_validator_endpoint_in_the_beacon_slot_is_still_recognised() {
        let mut f = healthy();
        f.bn_families = Probe::Ok(families(&[], &["teku/v25.4.1"], false));
        let findings = evaluate(&f);
        let layout = finding(&findings, "metrics layout").expect("expected a layout finding");
        assert_eq!(layout.status, Status::Warn);
        assert!(
            layout.detail.contains("vc_metric_url"),
            "must name the fix: {}",
            layout.detail
        );
    }

    /// The property that makes two diagnostics worth having rather than one:
    /// an all-in-one process exports the beacon families too, and a validator
    /// client does not.
    #[test]
    fn the_two_layout_diagnostics_are_mutually_exclusive() {
        let mut all_in_one = healthy();
        all_in_one.bn_families = Probe::Ok(families(&["teku/v25.4.1"], &["teku/v25.4.1"], true));
        all_in_one.vc_families = Probe::Failed("could not reach endpoint".to_string());

        let mut misdirected = healthy();
        misdirected.bn_families = Probe::Ok(families(&[], &["teku/v25.4.1"], true));

        let all_in_one_findings = evaluate(&all_in_one);
        let misdirected_findings = evaluate(&misdirected);

        // `finding()` returns the first match by name, so on its own it can't
        // tell "exactly one fired" from "both fired and this is the first" -
        // asserting the count closes that gap.
        assert_eq!(
            all_in_one_findings
                .iter()
                .filter(|f| f.name == "metrics layout")
                .count(),
            1
        );
        assert_eq!(
            misdirected_findings
                .iter()
                .filter(|f| f.name == "metrics layout")
                .count(),
            1
        );

        let a = finding(&all_in_one_findings, "metrics layout")
            .unwrap()
            .detail
            .clone();
        let b = finding(&misdirected_findings, "metrics layout")
            .unwrap()
            .detail
            .clone();
        assert_ne!(a, b, "the two shapes must produce different advice");
    }

    /// A correctly configured separated deployment says nothing about layout.
    #[test]
    fn a_healthy_separated_deployment_produces_no_layout_finding() {
        assert!(finding(&evaluate(&healthy()), "metrics layout").is_none());
    }

    // --- probe ---

    /// What actually proves the short-circuit here is the five
    /// `Probe::Skipped` matches below, not the elapsed-time bound: a
    /// connection to `127.0.0.1:1` is refused instantly, so the `< 30s`
    /// assertion would pass even if `probe` made all seven calls with no
    /// short-circuit at all. The timing check is a loose backstop against a
    /// regression that reintroduces a real per-call wait, not the mechanism
    /// this test relies on to catch a broken guard.
    #[test]
    fn a_dead_beacon_api_skips_the_rest_instead_of_timing_out_once_per_call() {
        use std::time::Instant;

        // A port nothing listens on: the connection is refused immediately,
        // but the point is that only ONE call is attempted per endpoint.
        let cfg = ProbeConfig {
            stack: Some(Stack::BareMetal),
            api_url: "http://127.0.0.1:1".to_string(),
            bn_metric_url: "http://127.0.0.1:1/metrics".to_string(),
            vc_metric_url: "http://127.0.0.1:1/metrics".to_string(),
            container: None,
            data_dir: None,
        };

        let started = Instant::now();
        let f = probe(&cfg);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "probe took {:?}; it must short-circuit rather than time out per call",
            started.elapsed()
        );

        assert!(matches!(f.health, Probe::Failed(_)));
        assert!(matches!(f.syncing, Probe::Skipped(_)));
        assert!(matches!(f.finality, Probe::Skipped(_)));
        assert!(matches!(f.peers, Probe::Skipped(_)));
        assert!(matches!(f.bn_families, Probe::Failed(_)));
        assert!(matches!(f.duties, Probe::Skipped(_)));
        assert!(matches!(f.validators, Probe::Skipped(_)));
    }

    #[test]
    fn probe_records_the_urls_it_used_so_findings_can_name_them() {
        let cfg = ProbeConfig {
            stack: Some(Stack::EthDocker),
            api_url: "http://127.0.0.1:1".to_string(),
            bn_metric_url: "http://127.0.0.1:1/metrics".to_string(),
            vc_metric_url: "http://127.0.0.1:1/metrics".to_string(),
            container: None,
            data_dir: None,
        };
        let f = probe(&cfg);
        assert_eq!(f.api_url, "http://127.0.0.1:1");
        assert_eq!(f.stack, Some(Stack::EthDocker));
    }

    /// I1: on a Docker stack, `cfg.container: None` means detection ran and
    /// matched nothing - Docker was asked - which is a reportable failure
    /// (`Probe::Ok(vec![])`), not the same silence bare-metal reports.
    #[test]
    fn probe_reports_an_empty_container_list_rather_than_skipping_on_a_docker_stack() {
        for stack in [Stack::EthDocker, Stack::RocketPool] {
            let cfg = ProbeConfig {
                stack: Some(stack),
                api_url: "http://127.0.0.1:1".to_string(),
                bn_metric_url: "http://127.0.0.1:1/metrics".to_string(),
                vc_metric_url: "http://127.0.0.1:1/metrics".to_string(),
                container: None,
                data_dir: None,
            };
            let f = probe(&cfg);
            assert!(
                matches!(&f.containers, Probe::Ok(v) if v.is_empty()),
                "{stack:?}: got {:?}",
                f.containers
            );
        }
    }

    /// Bare-metal (and an unknown stack) never had a container to look for in
    /// the first place, which stays `Probe::Skipped` rather than the "asked
    /// and found nothing" `Probe::Ok(vec![])` a Docker stack reports.
    #[test]
    fn probe_skips_the_container_check_on_bare_metal_and_when_the_stack_is_unknown() {
        for stack in [Some(Stack::BareMetal), None] {
            let cfg = ProbeConfig {
                stack,
                api_url: "http://127.0.0.1:1".to_string(),
                bn_metric_url: "http://127.0.0.1:1/metrics".to_string(),
                vc_metric_url: "http://127.0.0.1:1/metrics".to_string(),
                container: None,
                data_dir: None,
            };
            let f = probe(&cfg);
            assert!(
                matches!(f.containers, Probe::Skipped(_)),
                "{stack:?}: got {:?}",
                f.containers
            );
        }
    }

    /// Only unreachability short-circuits the Beacon API probes. An endpoint
    /// that answers with an error (a 500, here) may still answer the other
    /// calls, and skipping them on anything less specific than
    /// `is_unreachable()` (e.g. `health.ok().is_none()`) would discard real
    /// diagnostic data at exactly the moment an operator needs it - and would
    /// pass every other test in this module unchanged, since none of them
    /// distinguish "failed but reachable" from "unreachable".
    #[test]
    fn a_beacon_api_error_that_is_not_unreachable_does_not_skip_the_rest() {
        let mut server = mockito::Server::new();
        let _health = server
            .mock("GET", "/eth/v1/node/health")
            .with_status(500)
            .create();
        let _syncing = server
            .mock("GET", "/eth/v1/node/syncing")
            .with_status(200)
            .with_body(
                r#"{"data":{"is_syncing":false,"is_optimistic":false,
                    "head_slot":"100","sync_distance":"0"}}"#,
            )
            .create();
        let _finality = server
            .mock("GET", "/eth/v1/beacon/states/head/finality_checkpoints")
            .with_status(200)
            .with_body(
                r#"{"data":{"previous_justified":{"epoch":"10","root":"0x1"},
                    "current_justified":{"epoch":"11","root":"0x2"},
                    "finalized":{"epoch":"9","root":"0x3"}}}"#,
            )
            .create();
        let _peers = server
            .mock("GET", "/eth/v1/node/peers")
            .with_status(200)
            .with_body(r#"{"data":[]}"#)
            .create();

        let cfg = ProbeConfig {
            stack: Some(Stack::BareMetal),
            api_url: server.url(),
            bn_metric_url: "http://127.0.0.1:1/metrics".to_string(),
            vc_metric_url: "http://127.0.0.1:1/metrics".to_string(),
            container: None,
            data_dir: None,
        };
        let f = probe(&cfg);

        assert!(matches!(f.health, Probe::Failed(_)));
        assert!(
            !matches!(f.syncing, Probe::Skipped(_)),
            "syncing was skipped on a non-unreachable health failure: {:?}",
            f.syncing
        );
        assert!(
            !matches!(f.finality, Probe::Skipped(_)),
            "finality was skipped on a non-unreachable health failure: {:?}",
            f.finality
        );
        assert!(
            !matches!(f.peers, Probe::Skipped(_)),
            "peers was skipped on a non-unreachable health failure: {:?}",
            f.peers
        );
    }

    /// Same reasoning as `a_beacon_api_error_that_is_not_unreachable_does_not_skip_the_rest`,
    /// for the metrics side, adapted to `families`: unlike the old `version`,
    /// a reachable scrape exporting none of the families tekops looks for is
    /// `Probe::Ok` with empty vecs rather than a failure (that is the point of
    /// `families` reporting emptiness instead of erroring - see
    /// `families_reports_emptiness_rather_than_failing_on_a_foreign_endpoint`
    /// in `metrics.rs`). So the short-circuit here only has real unreachability
    /// to key off, and the body below deliberately includes the metrics
    /// `duties`/`validators` need, so a wrongly-skipped call would be visible
    /// as a missing `Probe::Ok`.
    #[test]
    fn a_reachable_but_unrecognised_metrics_endpoint_does_not_skip_the_rest() {
        let mut server = mockito::Server::new();
        let body = r#"
validator_beacon_node_requests_total{method="publish_block",outcome="success"} 1
validator_local_validator_counts{status="active_ongoing"} 1
validator_local_validator_balances{pubkey="0x1"} 32000000000
"#;
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();

        let cfg = ProbeConfig {
            stack: Some(Stack::BareMetal),
            api_url: "http://127.0.0.1:1".to_string(),
            bn_metric_url: "http://127.0.0.1:1/metrics".to_string(),
            vc_metric_url: format!("{}/metrics", server.url()),
            container: None,
            data_dir: None,
        };
        let f = probe(&cfg);

        assert!(matches!(f.vc_families, Probe::Ok(_)));
        assert!(
            !matches!(f.duties, Probe::Skipped(_)),
            "duties was skipped on a reachable, merely unrecognised, endpoint: {:?}",
            f.duties
        );
        assert!(
            !matches!(f.validators, Probe::Skipped(_)),
            "validators was skipped on a reachable, merely unrecognised, endpoint: {:?}",
            f.validators
        );
    }
}
