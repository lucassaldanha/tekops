use clap::ValueEnum;

/// Which deployment tekops is pointed at.
///
/// This exists because the two common Docker stacks disagree with a bare-metal
/// node, and with each other, about every port tekops talks to. Naming the
/// stack sets all of them at once instead of making the operator look up three
/// numbers.
///
/// It is a `ValueEnum` (unlike `logs`'s source and `update`'s target, which are
/// hand-matched because they double as a path and a subcommand name) because a
/// `--stack` value doubles as nothing: clap can reject a bad one at parse time
/// and list the valid values itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Stack {
    /// Teku and Besu running directly on the host.
    #[value(name = "bare-metal")]
    BareMetal,
    /// https://ethdocker.com
    #[value(name = "eth-docker")]
    EthDocker,
    /// https://docs.rocketpool.net
    #[value(name = "rocketpool")]
    RocketPool,
}

#[allow(dead_code)]
impl Stack {
    /// The Beacon API base URL to use when nothing more specific was given.
    ///
    /// Both Docker stacks put the REST API on 5052 rather than the 5051 a
    /// bare-metal node defaults to: see Eth Docker's `CL_REST_PORT` and Rocket
    /// Pool's `defaultBnApiPort`.
    pub fn api_url(&self) -> &'static str {
        match self {
            Stack::BareMetal => "http://localhost:5051",
            Stack::EthDocker | Stack::RocketPool => "http://localhost:5052",
        }
    }

    /// The Prometheus scrape URL to use when nothing more specific was given.
    ///
    /// This points at the **validator client**, not the beacon node. `duties`
    /// and `validators` read VC metric families, and `version` tries the beacon
    /// family then falls back to the validator one, so the VC endpoint answers
    /// all three while the beacon endpoint answers only one.
    pub fn metric_url(&self) -> &'static str {
        match self {
            Stack::BareMetal => "http://localhost:8010/metrics",
            Stack::EthDocker => "http://localhost:8009/metrics",
            Stack::RocketPool => "http://localhost:9101/metrics",
        }
    }

    /// The trailing part of this stack's consensus-client container name.
    ///
    /// A suffix rather than a whole name, because neither prefix is knowable
    /// from the stack alone: Eth Docker's is the Compose project (the directory
    /// it was cloned into, default `eth-docker`) and Rocket Pool's is its
    /// configurable `ProjectName` (default `rocketpool`). Matching on the
    /// suffix is what lets detection find a container in a directory named
    /// anything at all.
    pub fn container_suffix(&self) -> Option<&'static str> {
        match self {
            Stack::BareMetal => None,
            Stack::EthDocker => Some("-consensus-1"),
            Stack::RocketPool => Some("_eth2"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_metal_keeps_todays_defaults() {
        assert_eq!(Stack::BareMetal.api_url(), "http://localhost:5051");
        assert_eq!(
            Stack::BareMetal.metric_url(),
            "http://localhost:8010/metrics"
        );
        assert_eq!(Stack::BareMetal.container_suffix(), None);
    }

    #[test]
    fn both_docker_stacks_use_5052_for_the_beacon_api() {
        assert_eq!(Stack::EthDocker.api_url(), "http://localhost:5052");
        assert_eq!(Stack::RocketPool.api_url(), "http://localhost:5052");
    }

    /// `duties` and `validators` read validator-client metrics, and `version`
    /// falls back across the beacon and validator metric families, so the
    /// profile must point at the VC port. Pointing it at the beacon node's
    /// port (8008 for Eth Docker, 9100 for Rocket Pool) would leave two of the
    /// three metrics commands reporting a missing metric family.
    #[test]
    fn metric_url_points_at_the_validator_client_not_the_beacon_node() {
        assert_eq!(
            Stack::EthDocker.metric_url(),
            "http://localhost:8009/metrics"
        );
        assert_eq!(
            Stack::RocketPool.metric_url(),
            "http://localhost:9101/metrics"
        );
    }

    #[test]
    fn container_suffixes_match_each_stacks_service_naming() {
        assert_eq!(Stack::EthDocker.container_suffix(), Some("-consensus-1"));
        assert_eq!(Stack::RocketPool.container_suffix(), Some("_eth2"));
    }
}
