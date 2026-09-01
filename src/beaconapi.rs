use serde::Deserialize;
use std::fmt;

pub struct BeaconClient {
    base_url: String,
}

#[derive(Debug)]
pub enum ApiError {
    Unreachable(String),
    Status(u16, String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unreachable(msg) => write!(f, "could not reach Beacon API: {msg}"),
            ApiError::Status(code, msg) => write!(f, "Beacon API returned {code}: {msg}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum HealthState {
    Ready,
    Syncing,
    NotReady,
}

#[derive(Debug, Deserialize)]
struct SyncingResponse {
    data: SyncingData,
}

#[derive(Debug, Deserialize)]
struct SyncingData {
    is_syncing: bool,
    is_optimistic: bool,
    head_slot: String,
    sync_distance: String,
}

pub struct SyncingStatus {
    pub is_syncing: bool,
    pub is_optimistic: bool,
    pub head_slot: String,
    pub sync_distance: String,
}

#[derive(Debug, Deserialize)]
struct HeaderResponse {
    data: HeaderData,
}
#[derive(Debug, Deserialize)]
struct HeaderData {
    root: String,
    header: HeaderMessageWrapper,
}
#[derive(Debug, Deserialize)]
struct HeaderMessageWrapper {
    message: HeaderMessage,
}
#[derive(Debug, Deserialize)]
struct HeaderMessage {
    slot: String,
}

pub struct BlockHeader {
    pub slot: String,
    pub root: String,
}

#[derive(Debug, Deserialize)]
struct FinalityResponse {
    data: FinalityData,
}
#[derive(Debug, Deserialize)]
struct FinalityData {
    previous_justified: Checkpoint,
    current_justified: Checkpoint,
    finalized: Checkpoint,
}
#[derive(Debug, Deserialize)]
struct Checkpoint {
    epoch: String,
}

pub struct FinalityCheckpoints {
    pub previous_justified_epoch: String,
    pub current_justified_epoch: String,
    pub finalized_epoch: String,
}

#[derive(Debug, Deserialize)]
struct PeersResponse {
    data: Vec<PeerData>,
}
#[derive(Debug, Deserialize)]
struct PeerData {
    peer_id: String,
    enr: Option<String>,
    state: String,
    direction: String,
}

pub struct PeerInfo {
    pub peer_id: String,
    pub enr: Option<String>,
    pub state: String,
    pub direction: String,
}

#[derive(Debug, Deserialize)]
struct ValidatorsResponse {
    data: Vec<ValidatorEntry>,
}
#[derive(Debug, Deserialize)]
struct ValidatorEntry {
    index: String,
    balance: String,
    status: String,
    validator: ValidatorDetail,
}
#[derive(Debug, Deserialize)]
struct ValidatorDetail {
    pubkey: String,
}

pub struct ValidatorInfo {
    pub index: String,
    pub pubkey: String,
    pub balance: String,
    pub status: String,
}

impl BeaconClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self { base_url: base_url.into() }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn request_status(&self, path: &str) -> Result<u16, ApiError> {
        match ureq::get(&self.url(path)).call() {
            Ok(resp) => Ok(resp.status()),
            Err(ureq::Error::Status(code, _)) => Ok(code),
            Err(ureq::Error::Transport(t)) => Err(ApiError::Unreachable(t.to_string())),
        }
    }

    fn get_json<T: for<'de> serde::Deserialize<'de>>(&self, path: &str) -> Result<T, ApiError> {
        let resp = ureq::get(&self.url(path)).call().map_err(|e| match e {
            ureq::Error::Status(code, resp) => {
                ApiError::Status(code, resp.into_string().unwrap_or_default())
            }
            ureq::Error::Transport(t) => ApiError::Unreachable(t.to_string()),
        })?;
        resp.into_json()
            .map_err(|e| ApiError::Status(0, format!("invalid JSON: {e}")))
    }

    pub fn health(&self) -> Result<HealthState, ApiError> {
        match self.request_status("/eth/v1/node/health")? {
            200 => Ok(HealthState::Ready),
            206 => Ok(HealthState::Syncing),
            503 => Ok(HealthState::NotReady),
            other => Err(ApiError::Status(other, "unexpected health status".to_string())),
        }
    }

    pub fn syncing(&self) -> Result<SyncingStatus, ApiError> {
        let parsed: SyncingResponse = self.get_json("/eth/v1/node/syncing")?;
        Ok(SyncingStatus {
            is_syncing: parsed.data.is_syncing,
            is_optimistic: parsed.data.is_optimistic,
            head_slot: parsed.data.head_slot,
            sync_distance: parsed.data.sync_distance,
        })
    }

    pub fn header_head(&self) -> Result<BlockHeader, ApiError> {
        let parsed: HeaderResponse = self.get_json("/eth/v1/beacon/headers/head")?;
        Ok(BlockHeader {
            slot: parsed.data.header.message.slot,
            root: parsed.data.root,
        })
    }

    pub fn finality_checkpoints(&self) -> Result<FinalityCheckpoints, ApiError> {
        let parsed: FinalityResponse =
            self.get_json("/eth/v1/beacon/states/head/finality_checkpoints")?;
        Ok(FinalityCheckpoints {
            previous_justified_epoch: parsed.data.previous_justified.epoch,
            current_justified_epoch: parsed.data.current_justified.epoch,
            finalized_epoch: parsed.data.finalized.epoch,
        })
    }

    pub fn peers(&self) -> Result<Vec<PeerInfo>, ApiError> {
        let parsed: PeersResponse = self.get_json("/eth/v1/node/peers")?;
        Ok(parsed
            .data
            .into_iter()
            .map(|p| PeerInfo { peer_id: p.peer_id, enr: p.enr, state: p.state, direction: p.direction })
            .collect())
    }

    pub fn validators(&self, ids: &[String]) -> Result<Vec<ValidatorInfo>, ApiError> {
        let query = ids.join(",");
        let path = format!("/eth/v1/beacon/states/head/validators?id={query}");
        let parsed: ValidatorsResponse = self.get_json(&path)?;
        Ok(parsed
            .data
            .into_iter()
            .map(|v| ValidatorInfo {
                index: v.index,
                pubkey: v.validator.pubkey,
                balance: v.balance,
                status: v.status,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_ready_on_200() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", "/eth/v1/node/health").with_status(200).create();
        let client = BeaconClient::new(server.url());
        assert!(matches!(client.health().unwrap(), HealthState::Ready));
    }

    #[test]
    fn health_syncing_on_206() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", "/eth/v1/node/health").with_status(206).create();
        let client = BeaconClient::new(server.url());
        assert!(matches!(client.health().unwrap(), HealthState::Syncing));
    }

    #[test]
    fn health_not_ready_on_503() {
        let mut server = mockito::Server::new();
        let _m = server.mock("GET", "/eth/v1/node/health").with_status(503).create();
        let client = BeaconClient::new(server.url());
        assert!(matches!(client.health().unwrap(), HealthState::NotReady));
    }

    #[test]
    fn health_errors_when_unreachable() {
        let client = BeaconClient::new("http://127.0.0.1:1");
        let err = client.health().unwrap_err();
        assert!(matches!(err, ApiError::Unreachable(_)));
    }

    #[test]
    fn syncing_parses_response_body() {
        let mut server = mockito::Server::new();
        let body = r#"{"data":{"is_syncing":true,"is_optimistic":false,"head_slot":"123","sync_distance":"4"}}"#;
        let _m = server
            .mock("GET", "/eth/v1/node/syncing")
            .with_status(200)
            .with_body(body)
            .create();
        let client = BeaconClient::new(server.url());
        let status = client.syncing().unwrap();
        assert!(status.is_syncing);
        assert!(!status.is_optimistic);
        assert_eq!(status.head_slot, "123");
        assert_eq!(status.sync_distance, "4");
    }

    #[test]
    fn header_head_parses_slot_and_root() {
        let mut server = mockito::Server::new();
        let body = r#"{"data":{"root":"0xabc","header":{"message":{"slot":"999"}}}}"#;
        let _m = server
            .mock("GET", "/eth/v1/beacon/headers/head")
            .with_status(200)
            .with_body(body)
            .create();
        let client = BeaconClient::new(server.url());
        let header = client.header_head().unwrap();
        assert_eq!(header.slot, "999");
        assert_eq!(header.root, "0xabc");
    }

    #[test]
    fn peers_parses_list() {
        let mut server = mockito::Server::new();
        let body = r#"{"data":[
            {"peer_id":"p1","enr":"enr:xyz","state":"connected","direction":"inbound"},
            {"peer_id":"p2","enr":null,"state":"connected","direction":"outbound"}
        ]}"#;
        let _m = server.mock("GET", "/eth/v1/node/peers").with_status(200).with_body(body).create();
        let client = BeaconClient::new(server.url());
        let peers = client.peers().unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].peer_id, "p1");
        assert_eq!(peers[0].enr.as_deref(), Some("enr:xyz"));
        assert_eq!(peers[1].enr, None);
    }

    #[test]
    fn validators_parses_list() {
        let mut server = mockito::Server::new();
        let body = r#"{"data":[
            {"index":"1","balance":"32000000000","status":"active_ongoing","validator":{"pubkey":"0xabc"}}
        ]}"#;
        let _m = server
            .mock("GET", "/eth/v1/beacon/states/head/validators?id=1")
            .with_status(200)
            .with_body(body)
            .create();
        let client = BeaconClient::new(server.url());
        let validators = client.validators(&["1".to_string()]).unwrap();
        assert_eq!(validators.len(), 1);
        assert_eq!(validators[0].index, "1");
        assert_eq!(validators[0].pubkey, "0xabc");
        assert_eq!(validators[0].balance, "32000000000");
        assert_eq!(validators[0].status, "active_ongoing");
    }

    #[test]
    fn finality_checkpoints_parses_epochs() {
        let mut server = mockito::Server::new();
        let body = r#"{"data":{"previous_justified":{"epoch":"10","root":"0x1"},"current_justified":{"epoch":"11","root":"0x2"},"finalized":{"epoch":"9","root":"0x3"}}}"#;
        let _m = server
            .mock("GET", "/eth/v1/beacon/states/head/finality_checkpoints")
            .with_status(200)
            .with_body(body)
            .create();
        let client = BeaconClient::new(server.url());
        let fc = client.finality_checkpoints().unwrap();
        assert_eq!(fc.previous_justified_epoch, "10");
        assert_eq!(fc.current_justified_epoch, "11");
        assert_eq!(fc.finalized_epoch, "9");
    }
}
