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

// `evaluate`, which uses every type here, lands in the next task. Same
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
