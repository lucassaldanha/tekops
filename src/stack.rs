use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::fmt;

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
///
/// The `#[serde(rename = "...")]` on each variant is pinned by hand rather
/// than derived with a blanket `#[serde(rename_all = "kebab-case")]`: the
/// `--stack` values are hand-chosen flag names, not a mechanical transform of
/// the variant identifiers, and `RocketPool`'s flag value is deliberately the
/// unhyphenated `"rocketpool"` - kebab-case would produce `"rocket-pool"`,
/// which disagrees with both the flag and `Display`. Each rename must match
/// the variant's own `#[value(name = "...")]` exactly; see
/// `serde_matches_clap_values_for_every_variant` below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum Stack {
    /// Teku running directly on the host.
    #[value(name = "bare-metal")]
    #[serde(rename = "bare-metal")]
    BareMetal,
    /// https://ethdocker.com
    #[value(name = "eth-docker")]
    #[serde(rename = "eth-docker")]
    EthDocker,
    /// https://docs.rocketpool.net
    #[value(name = "rocketpool")]
    #[serde(rename = "rocketpool")]
    RocketPool,
}

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

    /// The beacon node's Prometheus scrape URL for this stack.
    ///
    /// Teku's own `--metrics-port` default is 8008, which both Docker stacks
    /// keep for the consensus client; Rocket Pool moves it to 9100
    /// (`defaultBnMetricsPort` in its `rocket-pool-config.go`).
    pub fn bn_metric_url(&self) -> &'static str {
        match self {
            Stack::BareMetal | Stack::EthDocker => "http://localhost:8008/metrics",
            Stack::RocketPool => "http://localhost:9100/metrics",
        }
    }

    /// The validator client's Prometheus scrape URL for this stack.
    ///
    /// This answers `duties` and `validators`, which read VC metric families
    /// exclusively.
    ///
    /// Note that 8010 for bare-metal is a tekops convention, not a Teku
    /// default: Teku defaults *both* processes to 8008, so a separated
    /// bare-metal node has necessarily repointed one of them by hand and will
    /// be configuring this anyway.
    pub fn vc_metric_url(&self) -> &'static str {
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

    /// The trailing part of this stack's validator-client container name.
    ///
    /// The counterpart to `container_suffix`, and the reason a separated
    /// deployment can be named at all: both stacks supervise the validator in
    /// a container of its own, and either stack will happily run that
    /// validator against a beacon node it did not start. Rocket Pool calls
    /// that External Consensus Client mode; on such a node `_eth2` never
    /// exists and only this suffix can tell tekops that Docker is involved.
    pub fn validator_container_suffix(&self) -> Option<&'static str> {
        match self {
            Stack::BareMetal => None,
            Stack::EthDocker => Some("-validator-1"),
            Stack::RocketPool => Some("_validator"),
        }
    }
}

/// Which of a deployment's two processes a detection is looking for.
///
/// Carried by `DetectError` so a failure names the container the caller
/// actually asked about, and so the escape hatch it suggests is one that
/// exists: `--container` points `logs` at a consensus container and has never
/// meant the validator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Consensus,
    Validator,
}

impl Role {
    fn noun(&self) -> &'static str {
        match self {
            Role::Consensus => "consensus container",
            Role::Validator => "validator container",
        }
    }
}

/// Renders exactly the value a user types for `--stack`, so doctor's report
/// header names a stack in the CLI's own vocabulary rather than inventing a
/// second spelling.
impl fmt::Display for Stack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stack::BareMetal => write!(f, "bare-metal"),
            Stack::EthDocker => write!(f, "eth-docker"),
            Stack::RocketPool => write!(f, "rocketpool"),
        }
    }
}

/// Why a detection could not name exactly one container.
#[derive(Debug, PartialEq, Eq)]
pub enum DetectError {
    NotFound(Role),
    Ambiguous(Role, Vec<String>),
}

impl fmt::Display for DetectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DetectError::NotFound(role) => write!(
                f,
                "no Eth Docker or Rocket Pool {} found; {}",
                role.noun(),
                match role {
                    Role::Consensus => "name one with --container or $TEKOPS_CONTAINER",
                    Role::Validator => "name the stack with --stack or $TEKOPS_STACK",
                }
            ),
            DetectError::Ambiguous(role, names) => write!(
                f,
                "found more than one {} ({}); {}",
                role.noun(),
                names.join(", "),
                match role {
                    Role::Consensus => "pick one with --container or narrow with --stack",
                    Role::Validator => "narrow with --stack",
                }
            ),
        }
    }
}

/// Identifies the stack from the output of `docker ps --format '{{.Names}}'`.
///
/// Takes the output as a parameter rather than running `docker` itself, so the
/// whole matching rule is unit-testable on a machine with no Docker installed -
/// the same shape, and for the same reason, as `logs::resolve_log_sources`.
///
/// `only` narrows the search to a single stack's naming, which is what
/// `--stack` contributes to detection. Note it contributes a *filter*, never a
/// container name: deriving one from the profile would hand an Eth Docker user
/// in a differently named directory `eth-docker-consensus-1`, a container that
/// does not exist on their machine, in preference to the correct name this
/// function was about to find.
///
/// Exactly one match is required. Zero is an error rather than a fallback, and
/// two is an error rather than a guess, because tailing the wrong node's logs
/// looks exactly like tailing the right one until it matters.
pub fn detect_stack(ps_output: &str, only: Option<Stack>) -> Result<(Stack, String), DetectError> {
    detect_role(ps_output, only, Role::Consensus)
}

/// The same rule as `detect_stack`, applied to the validator container.
///
/// Separate from `detect_stack` rather than a mode of it, because the two
/// answers are independent facts about one host: a Rocket Pool node in
/// External Consensus Client mode matches here and nowhere in `detect_stack`,
/// which is exactly the deployment that used to be reported as bare-metal.
/// Callers that only want a log target (`logs`, `dump-logs`) keep asking the
/// consensus question alone.
pub fn detect_validator_stack(
    ps_output: &str,
    only: Option<Stack>,
) -> Result<(Stack, String), DetectError> {
    detect_role(ps_output, only, Role::Validator)
}

/// The matching rule both detections share, so the two cannot drift on
/// whitespace handling, `only` narrowing, or the refusal to guess.
fn detect_role(
    ps_output: &str,
    only: Option<Stack>,
    role: Role,
) -> Result<(Stack, String), DetectError> {
    let candidates = [Stack::EthDocker, Stack::RocketPool];
    let mut matches: Vec<(Stack, String)> = Vec::new();

    for line in ps_output.lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        for stack in candidates {
            if only.is_some_and(|s| s != stack) {
                continue;
            }
            let suffix = match role {
                Role::Consensus => stack.container_suffix(),
                Role::Validator => stack.validator_container_suffix(),
            };
            if let Some(suffix) = suffix {
                if name.ends_with(suffix) {
                    matches.push((stack, name.to_string()));
                }
            }
        }
    }

    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(DetectError::NotFound(role)),
        _ => {
            let mut names: Vec<String> = matches.into_iter().map(|(_, n)| n).collect();
            names.sort();
            Err(DetectError::Ambiguous(role, names))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Must match the `--stack` flag's own spelling: a doctor report that
    /// prints a stack name the CLI does not accept would hand the operator a
    /// value they cannot paste back in.
    #[test]
    fn display_matches_the_flag_values() {
        assert_eq!(Stack::BareMetal.to_string(), "bare-metal");
        assert_eq!(Stack::EthDocker.to_string(), "eth-docker");
        assert_eq!(Stack::RocketPool.to_string(), "rocketpool");
    }

    /// The bug this guards against: a blanket `#[serde(rename_all =
    /// "kebab-case")]` coincidentally matched the clap value for `BareMetal`
    /// and `EthDocker` but silently produced `"rocket-pool"` for `RocketPool`,
    /// whose clap value is the unhyphenated `"rocketpool"` - so `--json`
    /// output disagreed with both the flag vocabulary and `Display`.
    ///
    /// Driven off `Stack::value_variants()` and `to_possible_value()` (both
    /// from `clap::ValueEnum`) rather than a hand-written list, so a variant
    /// added later is covered automatically instead of silently skipped.
    #[test]
    fn serde_matches_clap_values_for_every_variant() {
        for stack in Stack::value_variants() {
            let clap_name = stack.to_possible_value().unwrap().get_name().to_string();

            let serialized = serde_json::to_value(stack).unwrap();
            let serde_name = serialized.as_str().unwrap();

            assert_eq!(
                serde_name, clap_name,
                "serde rename for {stack:?} disagrees with its clap value"
            );
            assert_eq!(
                stack.to_string(),
                clap_name,
                "Display for {stack:?} disagrees with its clap value"
            );
        }
    }

    #[test]
    fn bare_metal_keeps_todays_defaults() {
        assert_eq!(Stack::BareMetal.api_url(), "http://localhost:5051");
        assert_eq!(
            Stack::BareMetal.vc_metric_url(),
            "http://localhost:8010/metrics"
        );
        assert_eq!(Stack::BareMetal.container_suffix(), None);
    }

    #[test]
    fn both_docker_stacks_use_5052_for_the_beacon_api() {
        assert_eq!(Stack::EthDocker.api_url(), "http://localhost:5052");
        assert_eq!(Stack::RocketPool.api_url(), "http://localhost:5052");
    }

    /// The values `metric_url` used to return, kept byte-for-byte. Operators
    /// already point at these ports; the rename must not move them.
    #[test]
    fn vc_metric_urls_keep_todays_values() {
        assert_eq!(
            Stack::BareMetal.vc_metric_url(),
            "http://localhost:8010/metrics"
        );
        assert_eq!(
            Stack::EthDocker.vc_metric_url(),
            "http://localhost:8009/metrics"
        );
        assert_eq!(
            Stack::RocketPool.vc_metric_url(),
            "http://localhost:9101/metrics"
        );
    }

    /// Verified upstream: Teku's own `--metrics-port` default is 8008 for both
    /// the main command and the `validator-client` subcommand; eth-docker's
    /// `teku.yml` puts the CL on 8008; Rocket Pool's `rocket-pool-config.go`
    /// declares `defaultBnMetricsPort = 9100`.
    #[test]
    fn bn_metric_urls_match_each_stacks_beacon_node_port() {
        assert_eq!(
            Stack::BareMetal.bn_metric_url(),
            "http://localhost:8008/metrics"
        );
        assert_eq!(
            Stack::EthDocker.bn_metric_url(),
            "http://localhost:8008/metrics"
        );
        assert_eq!(
            Stack::RocketPool.bn_metric_url(),
            "http://localhost:9100/metrics"
        );
    }

    /// The whole point of the split. If a stack ever returned one URL for both
    /// processes, `doctor` would report the same endpoint twice under two
    /// names and `version` would claim the same process is both.
    ///
    /// Driven off `value_variants()` so a stack added later is covered
    /// automatically, the same shape as
    /// `serde_matches_clap_values_for_every_variant`.
    #[test]
    fn bn_and_vc_metric_urls_differ_for_every_stack() {
        for stack in Stack::value_variants() {
            assert_ne!(
                stack.bn_metric_url(),
                stack.vc_metric_url(),
                "{stack} points both processes at one endpoint"
            );
        }
    }

    #[test]
    fn container_suffixes_match_each_stacks_service_naming() {
        assert_eq!(Stack::EthDocker.container_suffix(), Some("-consensus-1"));
        assert_eq!(Stack::RocketPool.container_suffix(), Some("_eth2"));
    }

    #[test]
    fn detects_rocket_pool_from_a_default_install() {
        let ps = "rocketpool_node\nrocketpool_eth1\nrocketpool_eth2\nrocketpool_validator\n";
        let (stack, name) = detect_stack(ps, None).unwrap();
        assert_eq!(stack, Stack::RocketPool);
        assert_eq!(name, "rocketpool_eth2");
    }

    #[test]
    fn detects_eth_docker_from_a_default_install() {
        let ps = "eth-docker-execution-1\neth-docker-consensus-1\neth-docker-validator-1\n";
        let (stack, name) = detect_stack(ps, None).unwrap();
        assert_eq!(stack, Stack::EthDocker);
        assert_eq!(name, "eth-docker-consensus-1");
    }

    /// The whole reason detection matches on a suffix: both stacks let the
    /// operator rename the prefix, and Eth Docker's is just whatever directory
    /// the repo was cloned into.
    #[test]
    fn detects_both_stacks_under_non_default_project_names() {
        let (stack, name) = detect_stack("my-node-consensus-1\n", None).unwrap();
        assert_eq!(stack, Stack::EthDocker);
        assert_eq!(name, "my-node-consensus-1");

        let (stack, name) = detect_stack("hoodi_eth2\n", None).unwrap();
        assert_eq!(stack, Stack::RocketPool);
        assert_eq!(name, "hoodi_eth2");
    }

    #[test]
    fn reports_not_found_when_nothing_matches() {
        let err = detect_stack("postgres\nredis\n", None).unwrap_err();
        assert!(matches!(err, DetectError::NotFound(Role::Consensus)));
    }

    #[test]
    fn refuses_to_guess_between_two_matching_stacks() {
        let ps = "eth-docker-consensus-1\nrocketpool_eth2\n";
        let err = detect_stack(ps, None).unwrap_err();
        let DetectError::Ambiguous(_, names) = err else {
            panic!("expected Ambiguous, got {err:?}");
        };
        assert_eq!(names, vec!["eth-docker-consensus-1", "rocketpool_eth2"]);
    }

    /// The message has to name the candidates, because "ambiguous" without
    /// them leaves the operator no way to pick one short of running `docker ps`
    /// themselves.
    #[test]
    fn ambiguous_error_message_lists_the_candidates() {
        let err = detect_stack("eth-docker-consensus-1\nrocketpool_eth2\n", None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("eth-docker-consensus-1"), "got: {msg}");
        assert!(msg.contains("rocketpool_eth2"), "got: {msg}");
        assert!(
            msg.contains("--container"),
            "should name the escape hatch: {msg}"
        );
    }

    /// `--stack bare-metal` is the escape hatch for a host that runs Docker
    /// alongside a bare-metal node: narrowing to a stack with no
    /// `container_suffix()` can never match anything, so detection turns off
    /// and `resolve_log_sources` falls through to its bn slot's file-path
    /// default. This already worked - `container_suffix()` is `None` for
    /// `BareMetal` and the loop skips a `None` suffix - but nothing pinned it
    /// before this test.
    #[test]
    fn narrowing_to_bare_metal_matches_nothing() {
        let ps = "eth-docker-consensus-1\nrocketpool_eth2\n";
        let err = detect_stack(ps, Some(Stack::BareMetal)).unwrap_err();
        assert_eq!(err, DetectError::NotFound(Role::Consensus));
    }

    #[test]
    fn narrowing_to_one_stack_ignores_the_others_containers() {
        let ps = "eth-docker-consensus-1\nrocketpool_eth2\n";
        let (stack, name) = detect_stack(ps, Some(Stack::RocketPool)).unwrap();
        assert_eq!(stack, Stack::RocketPool);
        assert_eq!(name, "rocketpool_eth2");
    }

    #[test]
    fn ignores_blank_lines_and_surrounding_whitespace() {
        let ps = "\n  rocketpool_eth2  \n\n";
        let (_, name) = detect_stack(ps, None).unwrap();
        assert_eq!(name, "rocketpool_eth2");
    }

    /// Rocket Pool's execution container is `_eth1`, one character from the
    /// consensus one. Matching it would point `tekops logs` at the execution
    /// client while claiming to show Teku.
    #[test]
    fn does_not_match_rocket_pools_execution_container() {
        let err = detect_stack("rocketpool_eth1\n", None).unwrap_err();
        assert!(matches!(err, DetectError::NotFound(Role::Consensus)));
    }

    // --- validator detection ---

    /// The reported bug, at the layer it starts: Rocket Pool in External
    /// Consensus Client mode supervises the validator and nothing else, so
    /// there is no `_eth2` container, `detect_stack` finds nothing, and
    /// doctor's ladder used to terminate in a confident "bare-metal" for a
    /// node that is plainly running Rocket Pool under Docker.
    #[test]
    fn rocket_pool_running_only_a_validator_is_not_bare_metal() {
        let ps = "rocketpool_api\nrocketpool_node\nrocketpool_watchtower\nrocketpool_validator\n";

        assert!(matches!(
            detect_stack(ps, None),
            Err(DetectError::NotFound(Role::Consensus))
        ));

        let (stack, name) = detect_validator_stack(ps, None).unwrap();
        assert_eq!(stack, Stack::RocketPool);
        assert_eq!(name, "rocketpool_validator");
    }

    #[test]
    fn detects_eth_dockers_validator_container() {
        let ps = "eth-docker-execution-1\neth-docker-consensus-1\neth-docker-validator-1\n";
        let (stack, name) = detect_validator_stack(ps, None).unwrap();
        assert_eq!(stack, Stack::EthDocker);
        assert_eq!(name, "eth-docker-validator-1");
    }

    /// Validator detection matches on a suffix for the same reason the
    /// consensus one does: both stacks let the operator rename the prefix.
    #[test]
    fn detects_a_validator_under_a_non_default_project_name() {
        let (stack, name) = detect_validator_stack("my-node-validator-1\n", None).unwrap();
        assert_eq!(stack, Stack::EthDocker);
        assert_eq!(name, "my-node-validator-1");

        let (stack, name) = detect_validator_stack("hoodi_validator\n", None).unwrap();
        assert_eq!(stack, Stack::RocketPool);
        assert_eq!(name, "hoodi_validator");
    }

    /// Rocket Pool's supporting containers sit one underscore away from the
    /// validator. Matching `rocketpool_node` would report a stack from a
    /// container that has never run a validator.
    #[test]
    fn does_not_match_rocket_pools_supporting_containers() {
        for ps in ["rocketpool_node\n", "rocketpool_api\n", "rocketpool_eth2\n"] {
            assert!(
                matches!(
                    detect_validator_stack(ps, None),
                    Err(DetectError::NotFound(Role::Validator))
                ),
                "matched a validator in: {ps}"
            );
        }
    }

    #[test]
    fn refuses_to_guess_between_two_matching_validators() {
        let ps = "eth-docker-validator-1\nrocketpool_validator\n";
        let err = detect_validator_stack(ps, None).unwrap_err();
        let DetectError::Ambiguous(role, names) = err else {
            panic!("expected Ambiguous, got {err:?}");
        };
        assert_eq!(role, Role::Validator);
        assert_eq!(
            names,
            vec!["eth-docker-validator-1", "rocketpool_validator"]
        );
    }

    /// `--container` names a log target and has never meant the validator, so
    /// suggesting it here would send the operator after a flag that cannot fix
    /// what they are looking at.
    #[test]
    fn validator_failures_suggest_only_flags_that_exist() {
        let not_found = detect_validator_stack("postgres\n", None)
            .unwrap_err()
            .to_string();
        assert!(not_found.contains("validator container"), "{not_found}");
        assert!(!not_found.contains("--container"), "{not_found}");

        let ambiguous = detect_validator_stack("a-validator-1\nb_validator\n", None)
            .unwrap_err()
            .to_string();
        assert!(ambiguous.contains("--stack"), "{ambiguous}");
        assert!(!ambiguous.contains("--container"), "{ambiguous}");
    }

    /// `--stack bare-metal` turns *both* detections off, not just the
    /// consensus one - the escape hatch for a host that runs a bare-metal node
    /// alongside unrelated Docker containers has to stay total.
    #[test]
    fn narrowing_to_bare_metal_matches_no_validator_either() {
        let ps = "eth-docker-validator-1\nrocketpool_validator\n";
        let err = detect_validator_stack(ps, Some(Stack::BareMetal)).unwrap_err();
        assert_eq!(err, DetectError::NotFound(Role::Validator));
    }

    #[test]
    fn every_stack_with_a_consensus_container_has_a_validator_one() {
        for stack in Stack::value_variants() {
            assert_eq!(
                stack.container_suffix().is_some(),
                stack.validator_container_suffix().is_some(),
                "{stack} names one container but not the other"
            );
        }
    }
}
