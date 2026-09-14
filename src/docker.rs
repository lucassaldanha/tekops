//! `docker inspect` for `tekops doctor`.
//!
//! Argv is returned as data and the JSON is parsed by a pure function, the
//! same split `logs::producer_argv` and `stack::detect_stack` use: the shape
//! of what we ask Docker and what we make of the answer are both testable with
//! no Docker installed.

// The `doctor` command that calls into this module lands in a later task;
// until then nothing outside this file's own tests uses these items. Same
// pattern as `host.rs` when it was first added ahead of its own wiring.
#![allow(dead_code)]

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ContainerState {
    pub name: String,
    pub running: bool,
    pub status: String,
    pub started_at: Option<String>,
    pub restart_count: u64,
    pub mounts: Vec<PathBuf>,
}

#[derive(Deserialize)]
struct InspectEntry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "RestartCount", default)]
    restart_count: u64,
    #[serde(rename = "State")]
    state: InspectState,
    #[serde(rename = "Mounts", default)]
    mounts: Vec<InspectMount>,
}

#[derive(Deserialize)]
struct InspectState {
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "Running", default)]
    running: bool,
    #[serde(rename = "StartedAt")]
    started_at: Option<String>,
}

#[derive(Deserialize)]
struct InspectMount {
    #[serde(rename = "Source", default)]
    source: String,
}

pub fn inspect_argv(name: &str) -> Vec<String> {
    vec![
        "docker".to_string(),
        "inspect".to_string(),
        name.to_string(),
    ]
}

/// Parses `docker inspect`'s output, which is always an array even for a
/// single container.
pub fn parse_inspect(json: &str) -> Option<ContainerState> {
    let entries: Vec<InspectEntry> = serde_json::from_str(json).ok()?;
    let e = entries.into_iter().next()?;
    Some(ContainerState {
        // Docker reports Name with a leading slash, which is not what the
        // operator typed and would read as wrong echoed back in a finding.
        name: e.name.trim_start_matches('/').to_string(),
        running: e.state.running,
        status: e.state.status,
        started_at: e.state.started_at,
        restart_count: e.restart_count,
        mounts: e
            .mounts
            .into_iter()
            .filter(|m| !m.source.is_empty())
            .map(|m| PathBuf::from(m.source))
            .collect(),
    })
}

/// The mount most likely to hold the chain data: the deepest source path.
///
/// Containers routinely bind trivia such as /etc/localtime alongside the real
/// data volume, so "the first mount" is the wrong answer and picking by
/// destination would need per-client knowledge that does not belong here.
pub fn data_mount(c: &ContainerState) -> Option<&Path> {
    c.mounts
        .iter()
        .max_by_key(|p| p.as_os_str().len())
        .map(|p| p.as_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape captured from a real `docker inspect` on an Eth Docker stack.
    /// `docker inspect` always returns an array, even for one container, and
    /// RestartCount is top-level rather than inside State.
    const ETH_DOCKER: &str = r#"[
      {
        "Name": "/eth-docker-consensus-1",
        "RestartCount": 0,
        "State": { "Status": "running", "Running": true,
                   "StartedAt": "2026-09-10T04:12:33.123456789Z" },
        "Mounts": [
          { "Type": "volume", "Source": "/var/lib/docker/volumes/eth-docker_consensus-data/_data",
            "Destination": "/var/lib/teku" },
          { "Type": "bind", "Source": "/etc/localtime", "Destination": "/etc/localtime" }
        ]
      }
    ]"#;

    const ROCKETPOOL_STOPPED: &str = r#"[
      {
        "Name": "/rocketpool_eth2",
        "RestartCount": 7,
        "State": { "Status": "exited", "Running": false,
                   "StartedAt": "2026-09-14T01:02:03.000000000Z" },
        "Mounts": [
          { "Type": "volume", "Source": "/var/lib/docker/volumes/rocketpool_eth2clientdata/_data",
            "Destination": "/ethclient" }
        ]
      }
    ]"#;

    #[test]
    fn inspect_argv_asks_docker_for_one_container() {
        assert_eq!(
            inspect_argv("eth-docker-consensus-1"),
            vec!["docker", "inspect", "eth-docker-consensus-1"]
        );
    }

    #[test]
    fn parses_a_running_eth_docker_container() {
        let got = parse_inspect(ETH_DOCKER).unwrap();
        // The leading slash Docker puts on Name is not part of the name an
        // operator typed, and it would look wrong echoed back in a finding.
        assert_eq!(got.name, "eth-docker-consensus-1");
        assert!(got.running);
        assert_eq!(got.status, "running");
        assert_eq!(got.restart_count, 0);
        assert_eq!(
            got.started_at.as_deref(),
            Some("2026-09-10T04:12:33.123456789Z")
        );
        assert_eq!(got.mounts.len(), 2);
    }

    #[test]
    fn parses_a_stopped_rocketpool_container_with_restarts() {
        let got = parse_inspect(ROCKETPOOL_STOPPED).unwrap();
        assert_eq!(got.name, "rocketpool_eth2");
        assert!(!got.running);
        assert_eq!(got.status, "exited");
        assert_eq!(got.restart_count, 7);
    }

    /// The data volume is the deepest mount, not the first one: containers
    /// routinely bind trivia like /etc/localtime alongside the real volume.
    #[test]
    fn data_mount_picks_the_most_specific_source_path() {
        let c = parse_inspect(ETH_DOCKER).unwrap();
        assert_eq!(
            data_mount(&c).unwrap(),
            Path::new("/var/lib/docker/volumes/eth-docker_consensus-data/_data")
        );
    }

    #[test]
    fn data_mount_is_none_when_there_are_no_mounts() {
        let c = parse_inspect(
            r#"[{"Name":"/x","RestartCount":0,
                 "State":{"Status":"running","Running":true,"StartedAt":"t"},
                 "Mounts":[]}]"#,
        )
        .unwrap();
        assert!(data_mount(&c).is_none());
    }

    #[test]
    fn parse_inspect_is_none_for_an_empty_array() {
        assert!(parse_inspect("[]").is_none());
    }

    #[test]
    fn parse_inspect_is_none_for_junk() {
        assert!(parse_inspect("not json at all").is_none());
    }
}
