use crate::http::{map_ureq_error, ApiError};
use serde::Serialize;
use std::collections::BTreeMap;

pub struct MetricsClient {
    url: String,
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

/// Parses a Prometheus text-exposition body (as served by Teku/Besu's own
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
            let close = open + line[open..].find('}')?;
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
    Some(Sample { name, labels, value })
}

fn parse_labels(raw: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut pairs = Vec::new();
    for (i, c) in raw.char_indices() {
        match c {
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
            labels.insert(key.trim().to_string(), value.trim().trim_matches('"').to_string());
        }
    }
    labels
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
                && matchers.iter().all(|(k, v)| s.labels.get(*k).map(|actual| actual == v).unwrap_or(false))
        })
        .map(|s| s.value)
        .sum()
}

#[derive(Debug, Serialize)]
pub struct DutiesMetrics {
    pub published_blocks: u64,
    pub published_attestations: u64,
    pub published_sync_committee_messages: u64,
    pub published_aggregates: u64,
}

const VALIDATOR_REQUESTS_METRIC: &str = "validator_beacon_node_requests_total";

impl MetricsClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    fn fetch(&self) -> Result<Vec<Sample>, ApiError> {
        let resp = ureq::get(&self.url).call().map_err(map_ureq_error)?;
        let body =
            resp.into_string().map_err(|e| ApiError::Malformed(format!("invalid response body: {e}")))?;
        Ok(parse_exposition(&body))
    }

    pub fn duties(&self) -> Result<DutiesMetrics, ApiError> {
        let samples = self.fetch()?;
        let published = |method: &str| {
            matching_value(&samples, VALIDATOR_REQUESTS_METRIC, &[("method", method), ("outcome", "success")])
                .round() as u64
        };
        Ok(DutiesMetrics {
            published_blocks: published("publish_block"),
            published_attestations: published("publish_attestation"),
            published_sync_committee_messages: published("send_sync_committee_messages"),
            published_aggregates: published("publish_aggregate_and_proofs"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_counter_line_with_labels() {
        let body = r#"validator_beacon_node_requests_total{method="publish_block",outcome="success"} 42"#;
        let samples = parse_exposition(body);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "validator_beacon_node_requests_total");
        assert_eq!(samples[0].labels.get("method"), Some(&"publish_block".to_string()));
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
        let value = matching_value(&samples, "requests_total", &[("method", "publish_block"), ("outcome", "success")]);
        assert_eq!(value, 10.0);
    }

    #[test]
    fn matching_value_is_zero_when_series_absent() {
        let samples = parse_exposition(r#"requests_total{method="other",outcome="success"} 5"#);
        let value = matching_value(&samples, "requests_total", &[("method", "publish_block"), ("outcome", "success")]);
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
            matching_value(&samples, VALIDATOR_REQUESTS_METRIC, &[("method", method), ("outcome", "success")]).round()
                as u64
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
        let _m = server.mock("GET", "/metrics").with_status(200).with_body(body).create();
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
}
