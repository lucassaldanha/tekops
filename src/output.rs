use crate::beaconapi::{
    AttesterDuty, BlockHeader, FinalityCheckpoints, HealthState, ProposerDuty, SyncingStatus,
    ValidatorInfo,
};
use crate::doctor::{Facts, Finding, Status};
use crate::loglevel::LogLevelSpec;
use crate::metrics::{DutiesMetrics, ValidatorMetrics, VersionInfo};
use crate::protocol::Protocol;
use crate::term::sanitize;
use comfy_table::Table;
use serde::Serialize;
use std::collections::BTreeMap;

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

pub fn format_health_table(health: &HealthState, syncing: &SyncingStatus) -> String {
    let health_str = match health {
        HealthState::Ready => "ready",
        HealthState::Syncing => "syncing",
        HealthState::NotReady => "not ready",
    };
    let mut table = Table::new();
    table.set_header(vec!["Field", "Value"]);
    table.add_row(vec!["Health".to_string(), health_str.to_string()]);
    table.add_row(vec![
        "Syncing".to_string(),
        yes_no(syncing.is_syncing).to_string(),
    ]);
    table.add_row(vec!["Head Slot".to_string(), syncing.head_slot.clone()]);
    table.add_row(vec![
        "Sync Distance".to_string(),
        syncing.sync_distance.clone(),
    ]);
    table.add_row(vec![
        "Optimistic".to_string(),
        yes_no(syncing.is_optimistic).to_string(),
    ]);
    table.to_string()
}

pub fn format_head_table(header: &BlockHeader, finality: &FinalityCheckpoints) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Field", "Value"]);
    table.add_row(vec!["Head Slot".to_string(), header.slot.clone()]);
    table.add_row(vec!["Head Root".to_string(), header.root.clone()]);
    table.add_row(vec![
        "Justified Epoch".to_string(),
        finality.current_justified_epoch.clone(),
    ]);
    table.add_row(vec![
        "Finalized Epoch".to_string(),
        finality.finalized_epoch.clone(),
    ]);
    table.to_string()
}

pub fn format_duties_table(metrics: &DutiesMetrics) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Metric", "Value"]);
    table.add_row(vec![
        "Published Blocks".to_string(),
        metrics.published_blocks.to_string(),
    ]);
    table.add_row(vec![
        "Published Attestations".to_string(),
        metrics.published_attestations.to_string(),
    ]);
    table.add_row(vec![
        "Published Sync Committee Messages".to_string(),
        metrics.published_sync_committee_messages.to_string(),
    ]);
    table.add_row(vec![
        "Published Aggregates".to_string(),
        metrics.published_aggregates.to_string(),
    ]);
    table.to_string()
}

pub fn format_validator_metrics_table(metrics: &ValidatorMetrics) -> String {
    let mut counts_table = Table::new();
    counts_table.set_header(vec!["Status", "Count"]);
    for (status, count) in &metrics.counts_by_status {
        counts_table.add_row(vec![status.clone(), count.to_string()]);
    }

    let mut total_table = Table::new();
    total_table.set_header(vec!["Field", "Value"]);
    total_table.add_row(vec![
        "Total ETH".to_string(),
        match metrics.total_eth {
            Some(eth) => format!("{eth:.4}"),
            // Absent, not a measured zero: the scrape exports no balances
            // family at all, distinct from a validator client that legitimately
            // reports 0 ETH.
            None => "unknown (no balance metric exported)".to_string(),
        },
    ]);

    format!("{counts_table}\n\n{total_table}")
}

pub fn format_version_table(info: &VersionInfo) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Version"]);
    for version in &info.versions {
        table.add_row(vec![version.clone()]);
    }
    table.to_string()
}

#[derive(Serialize)]
pub struct PeerRow {
    pub peer_id: String,
    pub direction: String,
    pub state: String,
    pub protocol: Protocol,
}

fn protocol_label(p: Protocol) -> &'static str {
    match p {
        Protocol::Tcp => "TCP",
        Protocol::Quic => "QUIC",
    }
}

pub fn format_peers_table(rows: &[PeerRow]) -> String {
    let total = rows.len();
    let mut counts: BTreeMap<(String, &'static str), usize> = BTreeMap::new();
    for row in rows {
        *counts
            .entry((row.direction.clone(), protocol_label(row.protocol)))
            .or_insert(0) += 1;
    }
    let mut table = Table::new();
    table.set_header(vec!["Direction", "Protocol", "Count", "Total"]);
    for ((direction, protocol), count) in counts {
        table.add_row(vec![
            direction,
            protocol.to_string(),
            count.to_string(),
            total.to_string(),
        ]);
    }
    table.to_string()
}

pub fn format_validators_table(validators: &[ValidatorInfo]) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Index", "Pubkey", "Balance", "Status"]);
    for v in validators {
        table.add_row(vec![
            v.index.clone(),
            v.pubkey.clone(),
            v.balance.clone(),
            v.status.clone(),
        ]);
    }
    table.to_string()
}

pub fn format_attester_duties(duties: &[AttesterDuty]) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Slot", "Validator Index", "Committee Index", "Pubkey"]);
    for d in duties {
        table.add_row(vec![
            d.slot.clone(),
            d.validator_index.clone(),
            d.committee_index.clone(),
            d.pubkey.clone(),
        ]);
    }
    table.to_string()
}

pub fn format_proposer_duties(duties: &[ProposerDuty]) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Slot", "Validator Index", "Pubkey"]);
    for d in duties {
        table.add_row(vec![
            d.slot.clone(),
            d.validator_index.clone(),
            d.pubkey.clone(),
        ]);
    }
    table.to_string()
}

/// Not a table, unlike every other formatter here: this is the one command
/// whose output is prose rather than fetched data, so `comfy-table` would only
/// put a box around three lines a human reads once.
pub fn format_about() -> String {
    format!(
        "tekops {}\nMade with love by Lucas \u{2764}\u{fe0f}\nhttps://github.com/lucassaldanha/tekops",
        env!("CARGO_PKG_VERSION")
    )
}

/// Renders what a fetched body would apply, for the operator to read before it
/// is sent.
///
/// Plain aligned text rather than `comfy-table`, for a reason close to
/// `format_doctor_report`'s: this is the body of a prompt rather than the
/// command's output, and it is printed to stderr so `--json` leaves stdout
/// parseable.
///
/// Every value in it arrived over the network on its way to a terminal, so all
/// three go through `sanitize` - the same channel `logfmt::format_log_line` and
/// `map_ureq_error` guard. A forged line here would be read as tekops's own.
pub fn format_log_level_preview(url: &str, spec: &LogLevelSpec) -> String {
    let mut out = String::new();
    out.push_str(&format!("fetched from {}\n", sanitize(url)));
    out.push_str(&format!("  level:  {}\n", sanitize(&spec.level)));
    match &spec.log_filter {
        None => out.push_str("  filter: none, this is a global change\n"),
        Some(loggers) => {
            for (i, logger) in loggers.iter().enumerate() {
                let label = if i == 0 { "  filter:" } else { "         " };
                out.push_str(&format!("{label} {}\n", sanitize(logger)));
            }
        }
    }
    out
}

fn glyph(s: Status) -> char {
    match s {
        Status::Pass => '✔',
        Status::Warn => '⚠',
        Status::Fail => '✘',
    }
}

pub(crate) fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}

/// The doctor report.
///
/// Deliberately not a `comfy-table`: pasting this into a chat when asking for
/// help is a stated goal of the command, and box-drawing characters are noise
/// there. No ANSI is emitted either, for the same reason.
pub fn format_doctor_report(f: &Facts, findings: &[Finding]) -> String {
    let mut s = String::new();

    let stack = f
        .stack
        .map(|st| st.to_string())
        .unwrap_or_else(|| "unknown stack".to_string());
    let version = match &f.version {
        crate::doctor::Probe::Ok(v) if !v.versions.is_empty() => v.versions.join(", "),
        _ => "version unknown".to_string(),
    };
    s.push_str(&format!(
        "\n  {} · {} · {} {}\n\n",
        stack,
        crate::term::sanitize(&version),
        f.os,
        f.arch
    ));

    let width = findings.iter().map(|x| x.name.len()).max().unwrap_or(0);
    for x in findings {
        s.push_str(&format!(
            "  {}  {:width$}  {}\n",
            glyph(x.status),
            x.name,
            x.detail,
            width = width
        ));
    }

    let fails = findings.iter().filter(|x| x.status == Status::Fail).count();
    let warns = findings.iter().filter(|x| x.status == Status::Warn).count();
    s.push('\n');
    if fails == 0 && warns == 0 {
        s.push_str("no problems found.\n");
    } else {
        s.push_str(&format!(
            "{}, {}.\n",
            plural(fails, "failure"),
            plural(warns, "warning")
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beaconapi::{HealthState, SyncingStatus};

    /// The version has to come from `CARGO_PKG_VERSION` rather than a literal,
    /// so a release bump can't leave `about` reporting a build that no longer
    /// exists. Hardcoding it back is exactly what this catches.
    #[test]
    fn about_reports_the_build_version_the_credit_and_the_source_url() {
        let about = format_about();
        assert!(
            about.contains(&format!("tekops {}", env!("CARGO_PKG_VERSION"))),
            "{about}"
        );
        assert!(
            about.contains("Made with love by Lucas \u{2764}\u{fe0f}"),
            "{about}"
        );
        assert!(
            about.contains("https://github.com/lucassaldanha/tekops"),
            "{about}"
        );
    }

    #[test]
    fn formats_health_table() {
        let syncing = SyncingStatus {
            is_syncing: false,
            is_optimistic: false,
            head_slot: "123456".to_string(),
            sync_distance: "0".to_string(),
            el_offline: false,
        };
        let table = format_health_table(&HealthState::Ready, &syncing);
        assert!(table.contains("ready"));
        assert!(table.contains("123456"));
        assert!(table.contains("no"));
    }

    #[test]
    fn formats_health_table_while_syncing() {
        let syncing = SyncingStatus {
            is_syncing: true,
            is_optimistic: true,
            head_slot: "100".to_string(),
            sync_distance: "50".to_string(),
            el_offline: false,
        };
        let table = format_health_table(&HealthState::Syncing, &syncing);
        assert!(table.contains("syncing"));
        assert!(table.contains("100"));
        assert!(table.contains("50"));
        assert!(table.contains("yes"));
    }

    #[test]
    fn formats_peers_table_grouped_by_direction_and_protocol() {
        use crate::protocol::Protocol;
        let rows = vec![
            PeerRow {
                peer_id: "p1".to_string(),
                direction: "inbound".to_string(),
                state: "connected".to_string(),
                protocol: Protocol::Tcp,
            },
            PeerRow {
                peer_id: "p2".to_string(),
                direction: "inbound".to_string(),
                state: "connected".to_string(),
                protocol: Protocol::Tcp,
            },
            PeerRow {
                peer_id: "p3".to_string(),
                direction: "outbound".to_string(),
                state: "connected".to_string(),
                protocol: Protocol::Quic,
            },
        ];
        let table = format_peers_table(&rows);
        assert!(table.contains("inbound"));
        assert!(table.contains("TCP"));
        assert!(table.contains("outbound"));
        assert!(table.contains("QUIC"));
        // 2 inbound/tcp peers, and a total of 3 across all rows
        assert!(table.contains('2'));
        assert!(table.contains('3'));
        assert!(!table.contains("p1"));
        assert!(!table.contains("connected"));
    }

    #[test]
    fn formats_validators_table() {
        use crate::beaconapi::ValidatorInfo;
        let validators = vec![ValidatorInfo {
            index: "1".to_string(),
            pubkey: "0xabc".to_string(),
            balance: "32000000000".to_string(),
            status: "active_ongoing".to_string(),
        }];
        let table = format_validators_table(&validators);
        assert!(table.contains("1"));
        assert!(table.contains("0xabc"));
        assert!(table.contains("32000000000"));
        assert!(table.contains("active_ongoing"));
    }

    #[test]
    fn formats_attester_duties() {
        use crate::beaconapi::AttesterDuty;
        let duties = vec![AttesterDuty {
            pubkey: "0xabc".to_string(),
            validator_index: "1".to_string(),
            committee_index: "2".to_string(),
            slot: "100".to_string(),
        }];
        let table = format_attester_duties(&duties);
        assert!(table.contains("0xabc"));
        assert!(table.contains("100"));
    }

    #[test]
    fn formats_proposer_duties() {
        use crate::beaconapi::ProposerDuty;
        let duties = vec![ProposerDuty {
            pubkey: "0xdef".to_string(),
            validator_index: "3".to_string(),
            slot: "101".to_string(),
        }];
        let table = format_proposer_duties(&duties);
        assert!(table.contains("0xdef"));
        assert!(table.contains("101"));
    }

    #[test]
    fn formats_head_table() {
        use crate::beaconapi::{BlockHeader, FinalityCheckpoints};
        let header = BlockHeader {
            slot: "999".to_string(),
            root: "0xabc".to_string(),
        };
        let finality = FinalityCheckpoints {
            previous_justified_epoch: "10".to_string(),
            current_justified_epoch: "11".to_string(),
            finalized_epoch: "9".to_string(),
        };
        let table = format_head_table(&header, &finality);
        assert!(table.contains("999"));
        assert!(table.contains("0xabc"));
        assert!(table.contains("11"));
        assert!(table.contains('9'));
    }

    #[test]
    fn formats_duties_table() {
        let metrics = DutiesMetrics {
            published_blocks: 1,
            published_attestations: 2,
            published_sync_committee_messages: 3,
            published_aggregates: 4,
        };
        let table = format_duties_table(&metrics);
        assert!(table.contains("Published Blocks"));
        assert!(table.contains('1'));
        assert!(table.contains("Published Attestations"));
        assert!(table.contains('2'));
        assert!(table.contains("Published Sync Committee Messages"));
        assert!(table.contains('3'));
        assert!(table.contains("Published Aggregates"));
        assert!(table.contains('4'));
    }

    #[test]
    fn formats_validator_metrics_table() {
        let mut counts_by_status = BTreeMap::new();
        counts_by_status.insert("active_ongoing".to_string(), 100);
        counts_by_status.insert("pending_queued".to_string(), 3);
        let metrics = ValidatorMetrics {
            counts_by_status,
            total_eth: Some(63.5),
        };
        let table = format_validator_metrics_table(&metrics);
        assert!(table.contains("active_ongoing"));
        assert!(table.contains("100"));
        assert!(table.contains("pending_queued"));
        assert!(table.contains("Total ETH"));
        assert!(table.contains("63.5000"));
    }

    /// I3: an absent balances family must render as absent, not as a
    /// measured `0.0000` sitting beside a real count.
    #[test]
    fn formats_validator_metrics_table_without_a_total_eth_figure_when_absent() {
        let mut counts_by_status = BTreeMap::new();
        counts_by_status.insert("active_ongoing".to_string(), 142);
        let metrics = ValidatorMetrics {
            counts_by_status,
            total_eth: None,
        };
        let table = format_validator_metrics_table(&metrics);
        assert!(table.contains("Total ETH"));
        assert!(
            !table.contains("0.0000"),
            "absent must not read as zero: {table}"
        );
    }

    #[test]
    fn formats_version_table() {
        let info = VersionInfo {
            versions: vec!["teku/v24.9.0".to_string()],
        };
        let table = format_version_table(&info);
        assert!(table.contains("teku/v24.9.0"));
    }

    #[test]
    fn formats_version_table_with_multiple_versions() {
        let info = VersionInfo {
            versions: vec!["teku/v24.10.0".to_string(), "teku/v24.9.0".to_string()],
        };
        let table = format_version_table(&info);
        assert!(table.contains("teku/v24.10.0"));
        assert!(table.contains("teku/v24.9.0"));
    }

    /// A node built the same way Task 6's `healthy()` builds one, except
    /// pinned to a Docker stack and a `linux` host, since the header tests
    /// below need both named.
    fn facts_for_output() -> crate::doctor::Facts {
        use crate::doctor::Probe;
        use crate::host::{Disk, Load, Memory};
        use crate::stack::Stack;
        use std::collections::BTreeMap;

        crate::doctor::Facts {
            stack: Some(Stack::EthDocker),
            api_url: "http://localhost:5052".to_string(),
            metric_url: "http://localhost:8009/metrics".to_string(),
            os: "linux",
            arch: "x86_64",
            health: Probe::Ok(HealthState::Ready),
            syncing: Probe::Ok(SyncingStatus {
                is_syncing: false,
                is_optimistic: false,
                el_offline: false,
                head_slot: "3200".to_string(),
                sync_distance: "0".to_string(),
            }),
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
                total_eth: Some(4544.0),
            }),
            containers: Probe::Skipped("no container for this stack"),
            disk: Some(Disk {
                available_bytes: 400 * 1024 * 1024 * 1024,
                mount_point: "/var/lib/teku".to_string(),
            }),
            memory: Some(Memory {
                total_bytes: 32 * 1024 * 1024 * 1024,
                available_bytes: 8 * 1024 * 1024 * 1024,
            }),
            load: Some(Load {
                one: 1.0,
                five: 1.0,
                fifteen: 1.0,
            }),
            cpus: Some(8),
        }
    }

    #[test]
    fn doctor_report_lists_every_finding_with_a_glyph_and_a_summary() {
        let findings = vec![
            Finding::new("beacon api", Status::Pass, "ready at http://x"),
            Finding::new("peer count", Status::Warn, "12 peers (want >= 20)"),
            Finding::new("optimistic head", Status::Fail, "head unverified"),
        ];
        let out = format_doctor_report(&facts_for_output(), &findings);

        assert!(out.contains("beacon api"));
        assert!(out.contains("12 peers"));
        assert!(out.contains('✔') && out.contains('⚠') && out.contains('✘'));
        assert!(out.contains("1 failure, 1 warning"), "summary was: {out}");
    }

    /// The Discord-paste case is a stated goal, and escape sequences would
    /// both corrupt a paste and reintroduce the surface `term::sanitize`
    /// exists to close.
    #[test]
    fn doctor_report_emits_no_ansi_escape_sequences() {
        let findings = vec![Finding::new("beacon api", Status::Fail, "down")];
        let out = format_doctor_report(&facts_for_output(), &findings);
        assert!(!out.contains('\u{1b}'));
    }

    #[test]
    fn doctor_report_says_so_when_everything_passes() {
        let findings = vec![Finding::new("beacon api", Status::Pass, "ready")];
        let out = format_doctor_report(&facts_for_output(), &findings);
        assert!(out.contains("no problems found"), "summary was: {out}");
    }

    #[test]
    fn doctor_report_header_names_the_stack_and_host() {
        let out = format_doctor_report(&facts_for_output(), &[]);
        assert!(out.contains("eth-docker"));
        assert!(out.contains("linux"));
    }

    #[test]
    fn a_preview_names_the_url_the_level_and_every_filter() {
        let preview = format_log_level_preview(
            "https://gist.github.com/someone/abc123",
            &LogLevelSpec {
                level: "DEBUG".to_string(),
                log_filter: Some(vec![
                    "tech.pegasys.teku.sync".to_string(),
                    "tech.pegasys.teku.networking".to_string(),
                ]),
            },
        );
        assert!(preview.contains("https://gist.github.com/someone/abc123"));
        assert!(preview.contains("DEBUG"));
        assert!(preview.contains("tech.pegasys.teku.sync"));
        assert!(
            preview.contains("tech.pegasys.teku.networking"),
            "every filter must be shown, not just the first: {preview}"
        );
    }

    /// A global change and a change scoped to loggers the operator cannot see
    /// must not look the same on screen.
    #[test]
    fn a_preview_says_so_when_the_change_is_global() {
        let preview = format_log_level_preview(
            "https://example.com/x",
            &LogLevelSpec {
                level: "INFO".to_string(),
                log_filter: None,
            },
        );
        assert!(preview.to_lowercase().contains("global"), "got {preview}");
    }

    /// The level and the logger names came off the network, and this text goes
    /// straight to a terminal - the channel `term::sanitize` exists for.
    #[test]
    fn a_preview_cannot_carry_escape_sequences_to_the_terminal() {
        let preview = format_log_level_preview(
            "https://example.com/\u{1b}[2Jx",
            &LogLevelSpec {
                level: "DEBUG\u{1b}[31m".to_string(),
                log_filter: Some(vec!["org.a\u{1b}[2J".to_string()]),
            },
        );
        assert!(!preview.contains('\u{1b}'), "escape survived: {preview:?}");
    }
}
