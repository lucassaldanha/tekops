use crate::beaconapi::{
    AttesterDuty, BlockHeader, FinalityCheckpoints, HealthState, ProposerDuty, SyncingStatus,
    ValidatorInfo,
};
use crate::protocol::Protocol;
use comfy_table::Table;
use serde::Serialize;
use std::collections::BTreeMap;

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
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
    table.add_row(vec!["Syncing".to_string(), yes_no(syncing.is_syncing).to_string()]);
    table.add_row(vec!["Head Slot".to_string(), syncing.head_slot.clone()]);
    table.add_row(vec!["Sync Distance".to_string(), syncing.sync_distance.clone()]);
    table.add_row(vec!["Optimistic".to_string(), yes_no(syncing.is_optimistic).to_string()]);
    table.to_string()
}

pub fn format_head_table(header: &BlockHeader, finality: &FinalityCheckpoints) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Field", "Value"]);
    table.add_row(vec!["Head Slot".to_string(), header.slot.clone()]);
    table.add_row(vec!["Head Root".to_string(), header.root.clone()]);
    table.add_row(vec!["Justified Epoch".to_string(), finality.current_justified_epoch.clone()]);
    table.add_row(vec!["Finalized Epoch".to_string(), finality.finalized_epoch.clone()]);
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
        *counts.entry((row.direction.clone(), protocol_label(row.protocol))).or_insert(0) += 1;
    }
    let mut table = Table::new();
    table.set_header(vec!["Direction", "Protocol", "Count", "Total"]);
    for ((direction, protocol), count) in counts {
        table.add_row(vec![direction, protocol.to_string(), count.to_string(), total.to_string()]);
    }
    table.to_string()
}

pub fn format_validators_table(validators: &[ValidatorInfo]) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Index", "Pubkey", "Balance", "Status"]);
    for v in validators {
        table.add_row(vec![v.index.clone(), v.pubkey.clone(), v.balance.clone(), v.status.clone()]);
    }
    table.to_string()
}

pub fn format_attester_duties(duties: &[AttesterDuty]) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Slot", "Validator Index", "Committee Index", "Pubkey"]);
    for d in duties {
        table.add_row(vec![d.slot.clone(), d.validator_index.clone(), d.committee_index.clone(), d.pubkey.clone()]);
    }
    table.to_string()
}

pub fn format_proposer_duties(duties: &[ProposerDuty]) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Slot", "Validator Index", "Pubkey"]);
    for d in duties {
        table.add_row(vec![d.slot.clone(), d.validator_index.clone(), d.pubkey.clone()]);
    }
    table.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beaconapi::{HealthState, SyncingStatus};

    #[test]
    fn formats_health_table() {
        let syncing = SyncingStatus {
            is_syncing: false,
            is_optimistic: false,
            head_slot: "123456".to_string(),
            sync_distance: "0".to_string(),
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
            PeerRow { peer_id: "p1".to_string(), direction: "inbound".to_string(), state: "connected".to_string(), protocol: Protocol::Tcp },
            PeerRow { peer_id: "p2".to_string(), direction: "inbound".to_string(), state: "connected".to_string(), protocol: Protocol::Tcp },
            PeerRow { peer_id: "p3".to_string(), direction: "outbound".to_string(), state: "connected".to_string(), protocol: Protocol::Quic },
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
        let header = BlockHeader { slot: "999".to_string(), root: "0xabc".to_string() };
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
}
