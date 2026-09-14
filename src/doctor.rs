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

// `tekops doctor` itself, which calls `evaluate`, lands in a later task. Same
// pattern as `host.rs` and `docker.rs` when they were first added ahead of
// their own wiring.
#![allow(dead_code)]

use crate::beaconapi::{FinalityCheckpoints, HealthState, PeerInfo, SyncingStatus};
use crate::docker::ContainerState;
use crate::host::{Disk, Load, Memory};
use crate::http::ApiError;
use crate::metrics::{DutiesMetrics, ValidatorMetrics, VersionInfo};
use crate::stack::Stack;
use serde::Serialize;

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
    pub metric_url: String,
    pub os: &'static str,
    pub arch: &'static str,

    pub health: Probe<HealthState>,
    pub syncing: Probe<SyncingStatus>,
    pub finality: Probe<FinalityCheckpoints>,
    pub peers: Probe<Vec<PeerInfo>>,
    pub version: Probe<VersionInfo>,
    pub duties: Probe<DutiesMetrics>,
    pub validators: Probe<ValidatorMetrics>,

    /// Empty on bare-metal, by design, and rendered as no rows at all rather
    /// than as "n/a": a bare-metal operator has no restart count and should
    /// never be shown one.
    pub containers: Vec<ContainerState>,
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
    check_metrics_endpoint(f, &mut out);
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

fn check_metrics_endpoint(f: &Facts, out: &mut Vec<Finding>) {
    match &f.version {
        Probe::Ok(v) => push(
            out,
            "metrics endpoint",
            Status::Pass,
            format!("{} at {}", v.versions.join(", "), f.metric_url),
        ),
        Probe::Failed(e) if f.version.is_unreachable() => {
            push(out, "metrics endpoint", Status::Fail, e)
        }
        // Reachable but exporting nothing we recognize: almost always the
        // beacon node's port where the validator client's was meant.
        Probe::Failed(e) => push(out, "metrics endpoint", Status::Warn, e),
        Probe::Skipped(why) => push(out, "metrics endpoint", Status::Fail, *why),
    }
}

fn check_syncing(f: &Facts, out: &mut Vec<Finding>) {
    let Some(s) = f.syncing.ok() else {
        return;
    };
    if s.is_syncing {
        push(
            out,
            "sync status",
            Status::Warn,
            format!("syncing, {} slots behind", s.sync_distance),
        );
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
    let (Some(fin), Some(s)) = (f.finality.ok(), f.syncing.ok()) else {
        return;
    };
    let (Ok(head), Ok(finalized)) = (
        s.head_slot.parse::<u64>(),
        fin.finalized_epoch.parse::<u64>(),
    ) else {
        return;
    };
    let lag = (head / SLOTS_PER_EPOCH).saturating_sub(finalized);
    let status = if lag > FINALITY_FAIL_ABOVE {
        Status::Fail
    } else if lag > FINALITY_WARN_ABOVE {
        Status::Warn
    } else {
        Status::Pass
    };
    push(out, "finality lag", status, format!("{lag} epochs"));
}

fn check_peers(f: &Facts, out: &mut Vec<Finding>) {
    let Some(p) = f.peers.ok() else {
        return;
    };
    let n = p.len();
    let status = if n == 0 {
        Status::Fail
    } else if n < PEERS_WARN_BELOW {
        Status::Warn
    } else {
        Status::Pass
    };
    let detail = if status == Status::Pass {
        format!("{n} peers")
    } else {
        format!("{n} peers (want >= {PEERS_WARN_BELOW})")
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
                push(
                    out,
                    "validator keys",
                    Status::Pass,
                    format!("{active} active_ongoing, {:.2} ETH", v.total_eth),
                );
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
                        "{} attestations, {} blocks",
                        d.published_attestations, d.published_blocks
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
    // Omitted entirely on bare-metal, where the question does not apply.
    if f.containers.is_empty() {
        return;
    }

    let stopped: Vec<&str> = f
        .containers
        .iter()
        .filter(|c| !c.running)
        .map(|c| c.name.as_str())
        .collect();
    if stopped.is_empty() {
        let names: Vec<&str> = f.containers.iter().map(|c| c.name.as_str()).collect();
        push(
            out,
            "containers running",
            Status::Pass,
            format!("{} up", names.join(", ")),
        );
    } else {
        push(
            out,
            "containers running",
            Status::Fail,
            format!("not running: {}", stopped.join(", ")),
        );
    }

    let worst = f.containers.iter().max_by_key(|c| c.restart_count);
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
            format!("{} restarts on {}", c.restart_count, c.name)
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
        let status = if l.one > cpus_f * 2.0 {
            Status::Fail
        } else if l.one > cpus_f {
            Status::Warn
        } else {
            Status::Pass
        };
        push(
            out,
            "load average",
            status,
            format!("{:.2} ({cpus} cpus)", l.one),
        );
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::VersionInfo;
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
            metric_url: "http://localhost:8010/metrics".to_string(),
            os: "linux",
            arch: "x86_64",
            health: Probe::Ok(HealthState::Ready),
            syncing: Probe::Ok(syncing(false, "0")),
            finality: Probe::Ok(FinalityCheckpoints {
                previous_justified_epoch: "98".to_string(),
                current_justified_epoch: "99".to_string(),
                finalized_epoch: "98".to_string(),
            }),
            peers: Probe::Ok(vec![]),
            version: Probe::Ok(VersionInfo {
                versions: vec!["teku/v25.1.0".to_string()],
            }),
            duties: Probe::Ok(DutiesMetrics {
                published_blocks: 1,
                published_attestations: 10,
                published_sync_committee_messages: 0,
                published_aggregates: 0,
            }),
            validators: Probe::Ok(ValidatorMetrics {
                counts_by_status: BTreeMap::from([("active_ongoing".to_string(), 142)]),
                total_eth: 4544.0,
            }),
            containers: vec![],
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
    fn a_healthy_node_has_no_failures_and_no_warnings_except_peers() {
        let mut f = healthy();
        f.peers = Probe::Ok(peers(30));
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
    /// row about one.
    #[test]
    fn bare_metal_omits_both_container_checks() {
        let f = healthy();
        let got = evaluate(&f);
        assert!(status_of(&got, "containers running").is_none());
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
            f.containers = vec![container(name, false, 0)];
            let got = evaluate(&f);
            assert_eq!(
                status_of(&got, "containers running"),
                Some(Status::Fail),
                "{stack:?}"
            );
            assert!(got
                .iter()
                .find(|x| x.name == "containers running")
                .unwrap()
                .detail
                .contains(name));
        }
    }

    #[test]
    fn container_restart_boundaries() {
        for (n, want) in [(0u64, Status::Pass), (1, Status::Warn), (5, Status::Fail)] {
            let mut f = healthy();
            f.stack = Some(Stack::RocketPool);
            f.containers = vec![container("rocketpool_eth2", true, n)];
            assert_eq!(
                status_of(&evaluate(&f), "container restarts"),
                Some(want),
                "{n} restarts"
            );
        }
    }

    /// Findings are printed to a terminal, and container names and versions
    /// come from outside this binary.
    #[test]
    fn findings_are_sanitized() {
        let mut f = healthy();
        f.stack = Some(Stack::EthDocker);
        f.containers = vec![container("evil\u{1b}[2Jname", false, 0)];
        let got = evaluate(&f);
        assert!(got.iter().all(|x| !x.detail.contains('\u{1b}')));
    }
}
