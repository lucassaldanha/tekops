use crate::beaconapi::{
    AttesterDuty, BlockHeader, FinalityCheckpoints, HealthState, ProposerDuty, SyncingStatus,
    ValidatorInfo,
};
use crate::enr::Protocol;
use comfy_table::Table;
use serde::Serialize;

fn yes_no(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

pub fn format_health_summary(health: &HealthState, syncing: &SyncingStatus) -> String {
    let health_str = match health {
        HealthState::Ready => "ready",
        HealthState::Syncing => "syncing",
        HealthState::NotReady => "not ready",
    };
    format!(
        "health: {health_str} | syncing: {} | head slot: {} | sync distance: {} | optimistic: {}",
        yes_no(syncing.is_syncing),
        syncing.head_slot,
        syncing.sync_distance,
        yes_no(syncing.is_optimistic),
    )
}

pub fn format_head_summary(header: &BlockHeader, finality: &FinalityCheckpoints) -> String {
    format!(
        "head slot: {} | head root: {} | justified epoch: {} | finalized epoch: {}",
        header.slot, header.root, finality.current_justified_epoch, finality.finalized_epoch
    )
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
        Protocol::Unknown => "Unknown",
    }
}

pub fn format_peers_table(rows: &[PeerRow]) -> String {
    let mut table = Table::new();
    table.set_header(vec!["Direction", "State", "Protocol", "Peer ID"]);
    let mut sorted: Vec<&PeerRow> = rows.iter().collect();
    sorted.sort_by(|a, b| (&a.direction, &a.state).cmp(&(&b.direction, &b.state)));
    for row in sorted {
        table.add_row(vec![
            row.direction.clone(),
            row.state.clone(),
            protocol_label(row.protocol).to_string(),
            row.peer_id.clone(),
        ]);
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
    fn formats_health_summary() {
        let syncing = SyncingStatus {
            is_syncing: false,
            is_optimistic: false,
            head_slot: "123456".to_string(),
            sync_distance: "0".to_string(),
        };
        let out = format_health_summary(&HealthState::Ready, &syncing);
        assert_eq!(
            out,
            "health: ready | syncing: no | head slot: 123456 | sync distance: 0 | optimistic: no"
        );
    }

    #[test]
    fn formats_health_summary_while_syncing() {
        let syncing = SyncingStatus {
            is_syncing: true,
            is_optimistic: true,
            head_slot: "100".to_string(),
            sync_distance: "50".to_string(),
        };
        let out = format_health_summary(&HealthState::Syncing, &syncing);
        assert_eq!(
            out,
            "health: syncing | syncing: yes | head slot: 100 | sync distance: 50 | optimistic: yes"
        );
    }

    #[test]
    fn formats_peers_table() {
        use crate::enr::Protocol;
        let rows = vec![
            PeerRow { peer_id: "p1".to_string(), direction: "inbound".to_string(), state: "connected".to_string(), protocol: Protocol::Tcp },
            PeerRow { peer_id: "p2".to_string(), direction: "outbound".to_string(), state: "connected".to_string(), protocol: Protocol::Quic },
        ];
        let table = format_peers_table(&rows);
        assert!(table.contains("p1"));
        assert!(table.contains("inbound"));
        assert!(table.contains("TCP"));
        assert!(table.contains("p2"));
        assert!(table.contains("outbound"));
        assert!(table.contains("QUIC"));
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
    fn formats_head_summary() {
        use crate::beaconapi::{BlockHeader, FinalityCheckpoints};
        let header = BlockHeader { slot: "999".to_string(), root: "0xabc".to_string() };
        let finality = FinalityCheckpoints {
            previous_justified_epoch: "10".to_string(),
            current_justified_epoch: "11".to_string(),
            finalized_epoch: "9".to_string(),
        };
        let out = format_head_summary(&header, &finality);
        assert_eq!(
            out,
            "head slot: 999 | head root: 0xabc | justified epoch: 11 | finalized epoch: 9"
        );
    }
}
