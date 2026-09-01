use crate::beaconapi::{HealthState, SyncingStatus};

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
}
