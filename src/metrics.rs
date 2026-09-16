use crate::http::{agent, map_ureq_error, ApiError};
use serde::Serialize;
use std::collections::BTreeMap;

pub struct MetricsClient {
    url: String,
    agent: ureq::Agent,
}

/// One sample scraped from a Prometheus text-exposition page: a metric name,
/// its labels, and the instant value. No timestamp - Teku's own `/metrics`
/// endpoint reports current values, not a time series.
#[derive(Debug, Clone, PartialEq)]
struct Sample {
    name: String,
    labels: BTreeMap<String, String>,
    value: f64,
}

/// Parses a Prometheus text-exposition body (as served by Teku's own
/// `/metrics` endpoint) into samples, skipping HELP/TYPE comments and blank
/// lines. Not a general-purpose parser - just enough to read simple counter
/// and gauge lines like `metric_name{label="value"} 42`.
fn parse_exposition(body: &str) -> Vec<Sample> {
    body.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Sample> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let (name, labels, rest) = match line.find('{') {
        Some(open) => {
            let close = open + find_closing_brace(&line[open..])?;
            let name = line[..open].to_string();
            let labels = parse_labels(&line[open + 1..close]);
            (name, labels, line[close + 1..].trim())
        }
        None => {
            let (name, rest) = line.split_once(char::is_whitespace)?;
            (name.to_string(), BTreeMap::new(), rest.trim())
        }
    };

    // `rest` is "value" or "value timestamp" - only the value matters here.
    let value: f64 = rest.split_whitespace().next()?.parse().ok()?;
    // The exposition format permits `NaN`, `+Inf` and `-Inf`, and Rust parses
    // all three happily. Letting one through poisons every aggregate it lands
    // in - a single NaN balance turns an otherwise correct total into NaN,
    // which then serializes to `null`. Drop them at the door instead, so a
    // partially-populated scrape degrades to a smaller total rather than to
    // no answer at all.
    if !value.is_finite() {
        return None;
    }
    Some(Sample {
        name,
        labels,
        value,
    })
}

/// Finds the `}` that closes the label set, ignoring any that appear inside a
/// quoted label value. A naive `find('}')` truncates the label set early on a
/// value like `version="teku/v25.1.0 {dev}"`, which then fails to parse and
/// silently drops the whole sample.
fn find_closing_brace(s: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            '}' if !in_quotes => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_labels(raw: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;
    let mut pairs = Vec::new();
    for (i, c) in raw.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_quotes => escaped = true,
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                pairs.push(&raw[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = raw[start..].trim();
    if !last.is_empty() {
        pairs.push(last);
    }
    for pair in pairs {
        if let Some((key, value)) = pair.trim().split_once('=') {
            labels.insert(key.trim().to_string(), unquote(value.trim()));
        }
    }
    labels
}

/// Strips the surrounding quotes from a label value and resolves the escape
/// sequences the exposition format defines (`\\`, `\"`, `\n`). Only the outer
/// pair of quotes is removed, so a value that legitimately starts or ends with
/// one survives intact.
fn unquote(value: &str) -> String {
    let inner = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Sums the value of every sample matching `name` and all `matchers`
/// (label, value) pairs - mirroring a PromQL `sum(metric{k="v",...})`, but
/// evaluated locally since a raw scrape endpoint has no query engine behind
/// it. In practice each of these counters resolves to a single series once
/// filtered down to one `method`/`outcome` pair, so the sum is just that
/// series' instant value.
fn matching_value(samples: &[Sample], name: &str, matchers: &[(&str, &str)]) -> f64 {
    samples
        .iter()
        .filter(|s| {
            s.name == name
                && matchers
                    .iter()
                    .all(|(k, v)| s.labels.get(*k).map(|actual| actual == v).unwrap_or(false))
        })
        .map(|s| s.value)
        .sum()
}

/// Sums every sample matching `name`, grouped by the value of `group_label` -
/// mirroring a PromQL `sum by (label) (metric{...})`. Samples missing the
/// label are dropped rather than grouped under an empty key.
fn sum_by_label(samples: &[Sample], name: &str, group_label: &str) -> BTreeMap<String, f64> {
    let mut totals: BTreeMap<String, f64> = BTreeMap::new();
    for s in samples.iter().filter(|s| s.name == name) {
        if let Some(value) = s.labels.get(group_label) {
            *totals.entry(value.clone()).or_insert(0.0) += s.value;
        }
    }
    totals
}

/// Sums every sample matching `name`, ignoring labels entirely - mirroring a
/// plain PromQL `sum(metric{...})` with no `by`/label matchers.
fn sum_all(samples: &[Sample], name: &str) -> f64 {
    samples
        .iter()
        .filter(|s| s.name == name)
        .map(|s| s.value)
        .sum()
}

/// Whether the scrape carries any sample for `name` at all, regardless of
/// labels or value. Absence means the metric family isn't exported by whatever
/// process is behind this endpoint - a different question entirely from the
/// metric being present and reading zero, and one the aggregation helpers
/// above can't answer since both cases sum to 0.
fn has_metric(samples: &[Sample], name: &str) -> bool {
    samples.iter().any(|s| s.name == name)
}

/// Every distinct value of `label` across samples matching `name`, sorted.
/// Used for "info" style metrics (e.g. a version metric whose value is
/// always 1 and whose label carries the actual data) where there's normally
/// exactly one matching series, but more than one shouldn't be silently
/// dropped (e.g. mid-upgrade, briefly both the old and new version report).
fn distinct_label_values(samples: &[Sample], name: &str, label: &str) -> Vec<String> {
    let mut values: Vec<String> = samples
        .iter()
        .filter(|s| s.name == name)
        .filter_map(|s| s.labels.get(label).cloned())
        .collect();
    values.sort();
    values.dedup();
    values
}

#[derive(Debug, Serialize)]
pub struct DutiesMetrics {
    pub published_blocks: u64,
    pub published_attestations: u64,
    pub published_sync_committee_messages: u64,
    pub published_aggregates: u64,
}

#[derive(Debug, Serialize)]
pub struct ValidatorMetrics {
    pub counts_by_status: BTreeMap<String, u64>,
    /// `None` when the scrape exports no balances family at all - a
    /// validator client that reports key counts but not balances is still
    /// working, so this is absent rather than a confident `0.0`. See
    /// `require_metric`'s doc comment for the bug class this avoids.
    pub total_eth: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct VersionInfo {
    pub versions: Vec<String>,
}

/// Everything `version` and `doctor` need to know about one scrape endpoint,
/// from a single fetch.
///
/// Both callers need more than one fact about the same endpoint, and the
/// per-question methods on this client each re-fetch. Asking once and
/// returning the answers together keeps `doctor` from scraping the same URL
/// three times to decide what it is.
///
/// Emptiness is reported, never treated as an error. Which absent family
/// matters is the caller's question: `version` tolerates one missing process,
/// while `doctor` reads the exact combination to tell an all-in-one deployment
/// apart from a misconfigured one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[allow(dead_code)]
pub struct EndpointFamilies {
    /// Versions from `beacon_teku_version_total`, empty on a validator client.
    pub beacon_versions: Vec<String>,
    /// Versions from `validator_teku_version_total`, empty on a beacon node.
    pub validator_versions: Vec<String>,
    /// Whether this endpoint exports `validator_local_validator_counts`, the
    /// family `validators` needs.
    ///
    /// Carried alongside the versions because it is what distinguishes a
    /// process serving both roles from a validator client sitting where a
    /// beacon node was expected: the first exports beacon versions too, the
    /// second does not.
    pub has_validator_families: bool,
}

const VALIDATOR_REQUESTS_METRIC: &str = "validator_beacon_node_requests_total";
const VALIDATOR_COUNTS_METRIC: &str = "validator_local_validator_counts";
const VALIDATOR_BALANCES_METRIC: &str = "validator_local_validator_balances";
const GWEI_PER_ETH: f64 = 1_000_000_000.0;
const BEACON_VERSION_METRIC: &str = "beacon_teku_version_total";
const VALIDATOR_VERSION_METRIC: &str = "validator_teku_version_total";

impl MetricsClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            agent: agent(),
        }
    }

    fn fetch(&self) -> Result<Vec<Sample>, ApiError> {
        let resp = self.agent.get(&self.url).call().map_err(map_ureq_error)?;
        let body = resp
            .into_string()
            .map_err(|e| ApiError::Malformed(format!("invalid response body: {e}")))?;
        Ok(parse_exposition(&body))
    }

    pub fn duties(&self) -> Result<DutiesMetrics, ApiError> {
        let samples = self.fetch()?;
        self.require_metric(&samples, VALIDATOR_REQUESTS_METRIC)?;
        let published = |method: &str| {
            matching_value(
                &samples,
                VALIDATOR_REQUESTS_METRIC,
                &[("method", method), ("outcome", "success")],
            )
            .round() as u64
        };
        Ok(DutiesMetrics {
            published_blocks: published("publish_block"),
            published_attestations: published("publish_attestation"),
            published_sync_committee_messages: published("send_sync_committee_messages"),
            published_aggregates: published("publish_aggregate_and_proofs"),
        })
    }

    pub fn validators(&self) -> Result<ValidatorMetrics, ApiError> {
        let samples = self.fetch()?;
        self.require_metric(&samples, VALIDATOR_COUNTS_METRIC)?;
        let counts_by_status = sum_by_label(&samples, VALIDATOR_COUNTS_METRIC, "status")
            .into_iter()
            .map(|(status, value)| (status, value.round() as u64))
            .collect();
        // Guarded like `require_metric`, but not through it: an absent
        // balances family should not fail the whole call, since the key
        // counts above are still a real, useful answer. `sum_all` alone
        // can't tell "absent" from "present and zero" (both sum to 0.0),
        // which is exactly the I3 bug class - a scrape exporting
        // `validator_local_validator_counts` but not `..._balances` must not
        // render as a measured `0.00 ETH`.
        let total_eth = has_metric(&samples, VALIDATOR_BALANCES_METRIC)
            .then(|| sum_all(&samples, VALIDATOR_BALANCES_METRIC) / GWEI_PER_ETH);
        Ok(ValidatorMetrics {
            counts_by_status,
            total_eth,
        })
    }

    /// Scrapes once and reports every family tekops can identify.
    /// Wired in by Tasks 5 and 6.
    #[allow(dead_code)]
    pub fn families(&self) -> Result<EndpointFamilies, ApiError> {
        let samples = self.fetch()?;
        Ok(EndpointFamilies {
            beacon_versions: distinct_label_values(&samples, BEACON_VERSION_METRIC, "version"),
            validator_versions: distinct_label_values(
                &samples,
                VALIDATOR_VERSION_METRIC,
                "version",
            ),
            has_validator_families: has_metric(&samples, VALIDATOR_COUNTS_METRIC),
        })
    }

    /// Reads the running Teku version from whichever of the beacon-node or
    /// validator-client version metric is present on this scrape - only one
    /// exists at a time, depending on which process's `/metrics` endpoint
    /// this client points at.
    pub fn version(&self) -> Result<VersionInfo, ApiError> {
        let samples = self.fetch()?;
        let mut versions = distinct_label_values(&samples, BEACON_VERSION_METRIC, "version");
        if versions.is_empty() {
            versions = distinct_label_values(&samples, VALIDATOR_VERSION_METRIC, "version");
        }
        if versions.is_empty() {
            return Err(ApiError::Malformed(format!(
                "no version metric found at {} (looked for {BEACON_VERSION_METRIC} and \
                 {VALIDATOR_VERSION_METRIC}); is this a Teku metrics endpoint?",
                self.url
            )));
        }
        Ok(VersionInfo { versions })
    }

    /// Fails when `name` is absent from the scrape entirely. Without this the
    /// aggregation helpers report a confident zero for a metric that was never
    /// exported, so pointing at the wrong process (the beacon node's port
    /// instead of the validator client's, say) renders as "this validator
    /// published nothing" - the alarm reading, from a healthy node.
    fn require_metric(&self, samples: &[Sample], name: &str) -> Result<(), ApiError> {
        if has_metric(samples, name) {
            return Ok(());
        }
        Err(ApiError::Malformed(format!(
            "metric {name} not found at {} - the endpoint responded but exports no such metric; \
             check that this is the right process's metrics port. If one process serves both \
             roles (an all-in-one deployment), point `vc_metric_url` at it.",
            self.url
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_counter_line_with_labels() {
        let body =
            r#"validator_beacon_node_requests_total{method="publish_block",outcome="success"} 42"#;
        let samples = parse_exposition(body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "validator_beacon_node_requests_total");
        assert_eq!(
            samples[0].labels.get("method"),
            Some(&"publish_block".to_string())
        );
        assert_eq!(samples[0].value, 42.0);
    }

    #[test]
    fn skips_help_type_and_blank_lines() {
        let body = "# HELP foo bar\n# TYPE foo counter\n\nfoo 1\n";
        let samples = parse_exposition(body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "foo");
    }

    #[test]
    fn parses_line_without_labels() {
        let samples = parse_exposition("jvm_threads_current 12");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "jvm_threads_current");
        assert!(samples[0].labels.is_empty());
        assert_eq!(samples[0].value, 12.0);
    }

    #[test]
    fn ignores_trailing_timestamp() {
        let samples = parse_exposition("foo 1 1700000000000");
        assert_eq!(samples[0].value, 1.0);
    }

    #[test]
    fn matching_value_sums_only_matching_series() {
        let body = r#"
requests_total{method="publish_block",outcome="success"} 10
requests_total{method="publish_block",outcome="failure"} 3
requests_total{method="publish_attestation",outcome="success"} 99
"#;
        let samples = parse_exposition(body);
        let value = matching_value(
            &samples,
            "requests_total",
            &[("method", "publish_block"), ("outcome", "success")],
        );
        assert_eq!(value, 10.0);
    }

    #[test]
    fn matching_value_is_zero_when_series_absent() {
        let samples = parse_exposition(r#"requests_total{method="other",outcome="success"} 5"#);
        let value = matching_value(
            &samples,
            "requests_total",
            &[("method", "publish_block"), ("outcome", "success")],
        );
        assert_eq!(value, 0.0);
    }

    #[test]
    fn duties_reads_all_four_metrics_from_a_scrape() {
        let body = r#"
validator_beacon_node_requests_total{method="publish_block",outcome="success"} 1
validator_beacon_node_requests_total{method="publish_attestation",outcome="success"} 2
validator_beacon_node_requests_total{method="send_sync_committee_messages",outcome="success"} 3
validator_beacon_node_requests_total{method="publish_aggregate_and_proofs",outcome="success"} 4
"#;
        let samples = parse_exposition(body);
        let published = |method: &str| {
            matching_value(
                &samples,
                VALIDATOR_REQUESTS_METRIC,
                &[("method", method), ("outcome", "success")],
            )
            .round() as u64
        };
        assert_eq!(published("publish_block"), 1);
        assert_eq!(published("publish_attestation"), 2);
        assert_eq!(published("send_sync_committee_messages"), 3);
        assert_eq!(published("publish_aggregate_and_proofs"), 4);
    }

    #[test]
    fn duties_fetches_and_parses_a_live_scrape() {
        let mut server = mockito::Server::new();
        let body = r#"
validator_beacon_node_requests_total{method="publish_block",outcome="success"} 5
validator_beacon_node_requests_total{method="publish_attestation",outcome="success"} 6
validator_beacon_node_requests_total{method="send_sync_committee_messages",outcome="success"} 7
validator_beacon_node_requests_total{method="publish_aggregate_and_proofs",outcome="success"} 8
"#;
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();
        let client = MetricsClient::new(format!("{}/metrics", server.url()));
        let duties = client.duties().unwrap();
        assert_eq!(duties.published_blocks, 5);
        assert_eq!(duties.published_attestations, 6);
        assert_eq!(duties.published_sync_committee_messages, 7);
        assert_eq!(duties.published_aggregates, 8);
    }

    #[test]
    fn duties_errors_when_unreachable() {
        let client = MetricsClient::new("http://127.0.0.1:1/metrics");
        let err = client.duties().unwrap_err();
        assert!(matches!(err, ApiError::Unreachable(_)));
    }

    #[test]
    fn sum_by_label_groups_and_sums_matching_series() {
        let body = r#"
validator_local_validator_counts{status="active_ongoing"} 100
validator_local_validator_counts{status="active_ongoing"} 20
validator_local_validator_counts{status="pending_queued"} 3
"#;
        let samples = parse_exposition(body);
        let totals = sum_by_label(&samples, "validator_local_validator_counts", "status");
        assert_eq!(totals.get("active_ongoing"), Some(&120.0));
        assert_eq!(totals.get("pending_queued"), Some(&3.0));
        assert_eq!(totals.len(), 2);
    }

    #[test]
    fn sum_by_label_drops_samples_missing_the_group_label() {
        let samples = parse_exposition("validator_local_validator_counts 42");
        let totals = sum_by_label(&samples, "validator_local_validator_counts", "status");
        assert!(totals.is_empty());
    }

    #[test]
    fn sum_all_ignores_labels() {
        let body = r#"
validator_local_validator_balances{pubkey="0x1"} 32000000000
validator_local_validator_balances{pubkey="0x2"} 31900000000
"#;
        let samples = parse_exposition(body);
        assert_eq!(
            sum_all(&samples, "validator_local_validator_balances"),
            63900000000.0
        );
    }

    #[test]
    fn validators_reads_counts_and_converts_balance_gwei_to_eth() {
        let mut server = mockito::Server::new();
        let body = r#"
validator_local_validator_counts{status="active_ongoing"} 2
validator_local_validator_counts{status="pending_queued"} 1
validator_local_validator_balances{pubkey="0x1"} 32000000000
validator_local_validator_balances{pubkey="0x2"} 31500000000
"#;
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();
        let client = MetricsClient::new(format!("{}/metrics", server.url()));
        let metrics = client.validators().unwrap();
        assert_eq!(metrics.counts_by_status.get("active_ongoing"), Some(&2));
        assert_eq!(metrics.counts_by_status.get("pending_queued"), Some(&1));
        assert_eq!(metrics.total_eth, Some(63.5));
    }

    /// I3: a scrape that exports the counts family but not the balances one
    /// must report `total_eth: None`, not a confident `Some(0.0)` - the
    /// counts are still a real answer, but absent-as-zero is the bug class
    /// this repo has hit before (see `require_metric`'s doc comment).
    #[test]
    fn validators_reports_no_total_eth_when_the_balances_family_is_absent() {
        let mut server = mockito::Server::new();
        let body = r#"validator_local_validator_counts{status="active_ongoing"} 142"#;
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();
        let client = MetricsClient::new(format!("{}/metrics", server.url()));
        let metrics = client.validators().unwrap();
        assert_eq!(metrics.counts_by_status.get("active_ongoing"), Some(&142));
        assert_eq!(
            metrics.total_eth, None,
            "an absent balances family must not read as 0.0 ETH"
        );
    }

    #[test]
    fn validators_errors_when_unreachable() {
        let client = MetricsClient::new("http://127.0.0.1:1/metrics");
        let err = client.validators().unwrap_err();
        assert!(matches!(err, ApiError::Unreachable(_)));
    }

    #[test]
    fn distinct_label_values_dedupes_and_sorts() {
        let body = r#"
beacon_teku_version_total{version="teku/v24.9.0"} 1
beacon_teku_version_total{version="teku/v24.9.0"} 1
beacon_teku_version_total{version="teku/v24.10.0"} 1
"#;
        let samples = parse_exposition(body);
        let values = distinct_label_values(&samples, "beacon_teku_version_total", "version");
        assert_eq!(
            values,
            vec!["teku/v24.10.0".to_string(), "teku/v24.9.0".to_string()]
        );
    }

    #[test]
    fn version_reads_beacon_metric_when_present() {
        let mut server = mockito::Server::new();
        let body = r#"beacon_teku_version_total{version="teku/v24.9.0"} 1"#;
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();
        let client = MetricsClient::new(format!("{}/metrics", server.url()));
        let info = client.version().unwrap();
        assert_eq!(info.versions, vec!["teku/v24.9.0".to_string()]);
    }

    #[test]
    fn version_falls_back_to_validator_metric_when_beacon_metric_absent() {
        let mut server = mockito::Server::new();
        let body = r#"validator_teku_version_total{version="teku/v24.9.0"} 1"#;
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();
        let client = MetricsClient::new(format!("{}/metrics", server.url()));
        let info = client.version().unwrap();
        assert_eq!(info.versions, vec!["teku/v24.9.0".to_string()]);
    }

    #[test]
    fn version_errors_when_neither_metric_present() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body("jvm_threads_current 1")
            .create();
        let client = MetricsClient::new(format!("{}/metrics", server.url()));
        let err = client.version().unwrap_err();
        assert!(matches!(err, ApiError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn find_closing_brace_ignores_braces_inside_quoted_values() {
        let body = r#"beacon_teku_version_total{version="teku/v25.1.0 {dev}"} 1"#;
        let samples = parse_exposition(body);
        assert_eq!(samples.len(), 1, "sample was dropped: {samples:?}");
        assert_eq!(
            samples[0].labels.get("version"),
            Some(&"teku/v25.1.0 {dev}".to_string())
        );
        assert_eq!(samples[0].value, 1.0);
    }

    #[test]
    fn parses_labels_containing_escaped_quotes() {
        let body = r#"m{a="x\"y",b="2"} 5"#;
        let samples = parse_exposition(body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].labels.get("a"), Some(&"x\"y".to_string()));
        assert_eq!(samples[0].labels.get("b"), Some(&"2".to_string()));
    }

    #[test]
    fn parses_label_value_containing_a_comma() {
        let samples = parse_exposition(r#"m{a="x,y",b="z"} 1"#);
        assert_eq!(samples[0].labels.get("a"), Some(&"x,y".to_string()));
        assert_eq!(samples[0].labels.get("b"), Some(&"z".to_string()));
    }

    #[test]
    fn drops_non_finite_sample_values() {
        for body in ["m NaN", "m +Inf", "m -Inf", "m inf"] {
            assert!(
                parse_exposition(body).is_empty(),
                "{body} should have been dropped"
            );
        }
    }

    #[test]
    fn a_nan_balance_does_not_poison_the_total() {
        let mut server = mockito::Server::new();
        let body = r#"
validator_local_validator_counts{status="active_ongoing"} 2
validator_local_validator_balances{pubkey="0x1"} NaN
validator_local_validator_balances{pubkey="0x2"} 32000000000
"#;
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();
        let client = MetricsClient::new(format!("{}/metrics", server.url()));
        let metrics = client.validators().unwrap();
        assert_eq!(
            metrics.total_eth,
            Some(32.0),
            "the real balance must survive the NaN"
        );
    }

    #[test]
    fn duties_errors_when_the_metric_family_is_absent() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body("jvm_threads_current 1")
            .create();
        let client = MetricsClient::new(format!("{}/metrics", server.url()));
        let err = client.duties().unwrap_err();
        assert!(matches!(err, ApiError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn duties_still_reports_a_genuine_zero_when_the_metric_is_present() {
        let mut server = mockito::Server::new();
        let body =
            r#"validator_beacon_node_requests_total{method="publish_block",outcome="failure"} 3"#;
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();
        let client = MetricsClient::new(format!("{}/metrics", server.url()));
        let duties = client.duties().unwrap();
        assert_eq!(
            duties.published_blocks, 0,
            "present-but-zero must not be an error"
        );
    }

    #[test]
    fn validators_errors_when_the_metric_family_is_absent() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body("jvm_threads_current 1")
            .create();
        let client = MetricsClient::new(format!("{}/metrics", server.url()));
        let err = client.validators().unwrap_err();
        assert!(matches!(err, ApiError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn version_errors_when_unreachable() {
        let client = MetricsClient::new("http://127.0.0.1:1/metrics");
        let err = client.version().unwrap_err();
        assert!(matches!(err, ApiError::Unreachable(_)));
    }

    #[test]
    fn families_reads_a_beacon_node_endpoint() {
        let mut server = mockito::Server::new();
        let body = r#"beacon_teku_version_total{version="teku/v25.4.1"} 1"#;
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();
        let f = MetricsClient::new(format!("{}/metrics", server.url()))
            .families()
            .unwrap();
        assert_eq!(f.beacon_versions, vec!["teku/v25.4.1".to_string()]);
        assert!(f.validator_versions.is_empty());
        assert!(!f.has_validator_families);
    }

    #[test]
    fn families_reads_a_validator_client_endpoint() {
        let mut server = mockito::Server::new();
        let body = "validator_teku_version_total{version=\"teku/v25.4.1\"} 1\n\
                    validator_local_validator_counts{status=\"active_ongoing\"} 12";
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();
        let f = MetricsClient::new(format!("{}/metrics", server.url()))
            .families()
            .unwrap();
        assert!(f.beacon_versions.is_empty());
        assert_eq!(f.validator_versions, vec!["teku/v25.4.1".to_string()]);
        assert!(f.has_validator_families);
    }

    /// An all-in-one deployment: one process exporting both families on one
    /// port. This is what eth-docker's `teku-allin1.yml` produces, and telling
    /// it apart from a validator endpoint is the whole reason
    /// `has_validator_families` is reported alongside the versions.
    #[test]
    fn families_reads_an_all_in_one_endpoint_exporting_both() {
        let mut server = mockito::Server::new();
        let body = "beacon_teku_version_total{version=\"teku/v25.4.1\"} 1\n\
                    validator_teku_version_total{version=\"teku/v25.4.1\"} 1\n\
                    validator_local_validator_counts{status=\"active_ongoing\"} 12";
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(body)
            .create();
        let f = MetricsClient::new(format!("{}/metrics", server.url()))
            .families()
            .unwrap();
        assert_eq!(f.beacon_versions, vec!["teku/v25.4.1".to_string()]);
        assert_eq!(f.validator_versions, vec!["teku/v25.4.1".to_string()]);
        assert!(f.has_validator_families);
    }

    /// An endpoint that answers but is not Teku's. `families` reports the
    /// emptiness rather than failing: only the caller knows whether an empty
    /// family is fatal for what it was asked to do.
    #[test]
    fn families_reports_emptiness_rather_than_failing_on_a_foreign_endpoint() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body("go_goroutines 42")
            .create();
        let f = MetricsClient::new(format!("{}/metrics", server.url()))
            .families()
            .unwrap();
        assert!(f.beacon_versions.is_empty());
        assert!(f.validator_versions.is_empty());
        assert!(!f.has_validator_families);
    }

    #[test]
    fn families_fails_when_the_endpoint_is_unreachable() {
        let client = MetricsClient::new("http://127.0.0.1:1/metrics".to_string());
        assert!(client.families().is_err());
    }

    /// `duties` and `validators` deliberately do not fall back to the beacon
    /// node's port when the validator families are missing: a silent repoint
    /// is the same mistake `require_metric` exists to prevent. What they owe
    /// the operator instead is the likely cause, since an all-in-one
    /// deployment is the common way to land here.
    #[test]
    fn a_missing_validator_family_names_the_all_in_one_case() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/metrics")
            .with_status(200)
            .with_body(r#"beacon_teku_version_total{version="teku/v25.4.1"} 1"#)
            .create();
        let err = MetricsClient::new(format!("{}/metrics", server.url()))
            .validators()
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("vc_metric_url"), "must name the fix: {msg}");
        assert!(msg.contains("all-in-one"), "{msg}");
    }
}
