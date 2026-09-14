use crate::http::{agent, map_ureq_error, ApiError};
use serde::{Deserialize, Serialize};

pub struct BeaconClient {
    base_url: String,
    agent: ureq::Agent,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
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
    /// Present in the Beacon API spec and populated by Teku, but defaulted
    /// rather than required: a client that omits it must not fail the whole
    /// decode, because every other field on this response is still useful.
    #[serde(default)]
    el_offline: bool,
}

#[derive(Debug, Serialize)]
pub struct SyncingStatus {
    pub is_syncing: bool,
    pub is_optimistic: bool,
    pub head_slot: String,
    pub sync_distance: String,
    pub el_offline: bool,
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

#[derive(Serialize)]
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

#[derive(Debug, Serialize)]
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
    last_seen_p2p_address: String,
    state: String,
    direction: String,
}

#[derive(Debug, Serialize)]
pub struct PeerInfo {
    pub peer_id: String,
    pub last_seen_p2p_address: String,
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

#[derive(Serialize)]
pub struct ValidatorInfo {
    pub index: String,
    pub pubkey: String,
    pub balance: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
struct AttesterDutiesResponse {
    data: Vec<AttesterDutyEntry>,
}
#[derive(Debug, Deserialize)]
struct AttesterDutyEntry {
    pubkey: String,
    validator_index: String,
    committee_index: String,
    slot: String,
}

#[derive(Serialize)]
pub struct AttesterDuty {
    pub pubkey: String,
    pub validator_index: String,
    pub committee_index: String,
    pub slot: String,
}

#[derive(Debug, Deserialize)]
struct ProposerDutiesResponse {
    data: Vec<ProposerDutyEntry>,
}
#[derive(Debug, Deserialize)]
struct ProposerDutyEntry {
    pubkey: String,
    validator_index: String,
    slot: String,
}

#[derive(Serialize)]
pub struct ProposerDuty {
    pub pubkey: String,
    pub validator_index: String,
    pub slot: String,
}

#[derive(Serialize)]
struct LogLevelRequest {
    level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_filter: Option<Vec<String>>,
}

/// Percent-encodes a single query-string value. Validator ids are indices or
/// hex pubkeys in practice, so the unreserved set covers every legitimate
/// input untouched and only malformed ones get escaped - which is the point:
/// they reach the server as one value instead of as extra parameters.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

impl BeaconClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            agent: agent(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn request_status(&self, path: &str) -> Result<u16, ApiError> {
        match self.agent.get(&self.url(path)).call() {
            Ok(resp) => Ok(resp.status()),
            Err(ureq::Error::Status(code, _)) => Ok(code),
            Err(e) => Err(map_ureq_error(e)),
        }
    }

    fn get_json<T: for<'de> serde::Deserialize<'de>>(&self, path: &str) -> Result<T, ApiError> {
        let resp = self
            .agent
            .get(&self.url(path))
            .call()
            .map_err(map_ureq_error)?;
        resp.into_json()
            .map_err(|e| ApiError::Malformed(format!("invalid JSON: {e}")))
    }

    fn post_json<T: for<'de> serde::Deserialize<'de>>(
        &self,
        path: &str,
        body: impl serde::Serialize,
    ) -> Result<T, ApiError> {
        let resp = self
            .agent
            .post(&self.url(path))
            .send_json(body)
            .map_err(map_ureq_error)?;
        resp.into_json()
            .map_err(|e| ApiError::Malformed(format!("invalid JSON: {e}")))
    }

    fn put_json(&self, path: &str, body: impl serde::Serialize) -> Result<(), ApiError> {
        self.agent
            .put(&self.url(path))
            .send_json(body)
            .map_err(map_ureq_error)?;
        Ok(())
    }

    pub fn health(&self) -> Result<HealthState, ApiError> {
        match self.request_status("/eth/v1/node/health")? {
            200 => Ok(HealthState::Ready),
            206 => Ok(HealthState::Syncing),
            503 => Ok(HealthState::NotReady),
            other => Err(ApiError::Status(
                other,
                "unexpected health status".to_string(),
            )),
        }
    }

    pub fn syncing(&self) -> Result<SyncingStatus, ApiError> {
        let parsed: SyncingResponse = self.get_json("/eth/v1/node/syncing")?;
        Ok(SyncingStatus {
            is_syncing: parsed.data.is_syncing,
            is_optimistic: parsed.data.is_optimistic,
            head_slot: parsed.data.head_slot,
            sync_distance: parsed.data.sync_distance,
            el_offline: parsed.data.el_offline,
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
            .map(|p| PeerInfo {
                peer_id: p.peer_id,
                last_seen_p2p_address: p.last_seen_p2p_address,
                state: p.state,
                direction: p.direction,
            })
            .collect())
    }

    pub fn validators(&self, ids: &[String]) -> Result<Vec<ValidatorInfo>, ApiError> {
        // Ids are percent-encoded individually, then joined on a literal comma
        // (which the Beacon API uses as the list separator, so it must not
        // itself be encoded). Interpolating them raw lets an id containing `&`
        // or `#` split into extra query parameters and silently query
        // something other than what was asked for.
        let query = ids
            .iter()
            .map(|id| percent_encode(id))
            .collect::<Vec<_>>()
            .join(",");
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

    pub fn duties_attester(
        &self,
        epoch: u64,
        indices: &[String],
    ) -> Result<Vec<AttesterDuty>, ApiError> {
        let path = format!("/eth/v1/validator/duties/attester/{epoch}");
        let parsed: AttesterDutiesResponse = self.post_json(&path, indices)?;
        Ok(parsed
            .data
            .into_iter()
            .map(|d| AttesterDuty {
                pubkey: d.pubkey,
                validator_index: d.validator_index,
                committee_index: d.committee_index,
                slot: d.slot,
            })
            .collect())
    }

    pub fn duties_proposer(&self, epoch: u64) -> Result<Vec<ProposerDuty>, ApiError> {
        let path = format!("/eth/v1/validator/duties/proposer/{epoch}");
        let parsed: ProposerDutiesResponse = self.get_json(&path)?;
        Ok(parsed
            .data
            .into_iter()
            .map(|d| ProposerDuty {
                pubkey: d.pubkey,
                validator_index: d.validator_index,
                slot: d.slot,
            })
            .collect())
    }

    /// Sets the node's runtime log level. `log_filter` scopes the change to
    /// specific logger names (e.g. `org.hyperledger.besu`); `None` changes
    /// the global level, and must be omitted from the request body entirely
    /// rather than sent as `null` or `[]`.
    pub fn set_log_level(
        &self,
        level: &str,
        log_filter: Option<Vec<String>>,
    ) -> Result<(), ApiError> {
        let body = LogLevelRequest {
            level: level.to_string(),
            log_filter,
        };
        self.put_json("/teku/v1/admin/log_level", body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_ready_on_200() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/eth/v1/node/health")
            .with_status(200)
            .create();
        let client = BeaconClient::new(server.url());
        assert!(matches!(client.health().unwrap(), HealthState::Ready));
    }

    #[test]
    fn health_syncing_on_206() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/eth/v1/node/health")
            .with_status(206)
            .create();
        let client = BeaconClient::new(server.url());
        assert!(matches!(client.health().unwrap(), HealthState::Syncing));
    }

    #[test]
    fn health_not_ready_on_503() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/eth/v1/node/health")
            .with_status(503)
            .create();
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
    fn syncing_reads_el_offline_when_present() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/eth/v1/node/syncing")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"data":{"is_syncing":false,"is_optimistic":false,
                    "el_offline":true,"head_slot":"100","sync_distance":"0"}}"#,
            )
            .create();

        let got = BeaconClient::new(server.url()).syncing().unwrap();
        assert!(got.el_offline);
    }

    /// Not every client ships the field. Its absence must not fail the whole
    /// response decode, which would turn a missing optional into an unusable
    /// `sync-status` check.
    #[test]
    fn syncing_defaults_el_offline_to_false_when_absent() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/eth/v1/node/syncing")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"data":{"is_syncing":false,"is_optimistic":false,
                    "head_slot":"100","sync_distance":"0"}}"#,
            )
            .create();

        let got = BeaconClient::new(server.url()).syncing().unwrap();
        assert!(!got.el_offline);
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
            {"peer_id":"p1","last_seen_p2p_address":"/ip4/1.2.3.4/udp/9001/quic","state":"connected","direction":"inbound"},
            {"peer_id":"p2","last_seen_p2p_address":"/ip4/5.6.7.8/tcp/9000","state":"connected","direction":"outbound"}
        ]}"#;
        let _m = server
            .mock("GET", "/eth/v1/node/peers")
            .with_status(200)
            .with_body(body)
            .create();
        let client = BeaconClient::new(server.url());
        let peers = client.peers().unwrap();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].peer_id, "p1");
        assert_eq!(peers[0].last_seen_p2p_address, "/ip4/1.2.3.4/udp/9001/quic");
        assert_eq!(peers[1].last_seen_p2p_address, "/ip4/5.6.7.8/tcp/9000");
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
    fn percent_encode_leaves_realistic_ids_untouched() {
        assert_eq!(percent_encode("123"), "123");
        assert_eq!(percent_encode("0xabcDEF0123"), "0xabcDEF0123");
    }

    #[test]
    fn percent_encode_escapes_query_delimiters() {
        assert_eq!(percent_encode("1&injected=yes"), "1%26injected%3Dyes");
        assert_eq!(percent_encode("a b"), "a%20b");
        assert_eq!(percent_encode("a#b"), "a%23b");
    }

    #[test]
    fn validators_does_not_let_an_id_forge_extra_query_parameters() {
        let mut server = mockito::Server::new();
        // Matching on the encoded form asserts the whole hostile id arrived as
        // a single `id` value rather than splitting into a second parameter.
        let _m = server
            .mock(
                "GET",
                "/eth/v1/beacon/states/head/validators?id=1%26injected%3Dyes",
            )
            .with_status(200)
            .with_body(r#"{"data":[]}"#)
            .create();
        let client = BeaconClient::new(server.url());
        client.validators(&["1&injected=yes".to_string()]).unwrap();
        _m.assert();
    }

    #[test]
    fn validators_joins_multiple_ids_on_an_unencoded_comma() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("GET", "/eth/v1/beacon/states/head/validators?id=1,2")
            .with_status(200)
            .with_body(r#"{"data":[]}"#)
            .create();
        let client = BeaconClient::new(server.url());
        client
            .validators(&["1".to_string(), "2".to_string()])
            .unwrap();
        _m.assert();
    }

    #[test]
    fn duties_attester_posts_indices_and_parses_response() {
        let mut server = mockito::Server::new();
        let body = r#"{"data":[{"pubkey":"0xabc","validator_index":"1","committee_index":"2","slot":"100"}]}"#;
        let _m = server
            .mock("POST", "/eth/v1/validator/duties/attester/5")
            .with_status(200)
            .with_body(body)
            .create();
        let client = BeaconClient::new(server.url());
        let duties = client.duties_attester(5, &["1".to_string()]).unwrap();
        assert_eq!(duties.len(), 1);
        assert_eq!(duties[0].pubkey, "0xabc");
        assert_eq!(duties[0].validator_index, "1");
        assert_eq!(duties[0].committee_index, "2");
        assert_eq!(duties[0].slot, "100");
    }

    #[test]
    fn duties_proposer_parses_response() {
        let mut server = mockito::Server::new();
        let body = r#"{"data":[{"pubkey":"0xdef","validator_index":"3","slot":"101"}]}"#;
        let _m = server
            .mock("GET", "/eth/v1/validator/duties/proposer/5")
            .with_status(200)
            .with_body(body)
            .create();
        let client = BeaconClient::new(server.url());
        let duties = client.duties_proposer(5).unwrap();
        assert_eq!(duties.len(), 1);
        assert_eq!(duties[0].pubkey, "0xdef");
        assert_eq!(duties[0].validator_index, "3");
        assert_eq!(duties[0].slot, "101");
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

    #[test]
    fn set_log_level_omits_log_filter_when_global() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("PUT", "/teku/v1/admin/log_level")
            .match_body(mockito::Matcher::Json(
                serde_json::json!({"level": "DEBUG"}),
            ))
            .with_status(200)
            .create();
        let client = BeaconClient::new(server.url());
        client.set_log_level("DEBUG", None).unwrap();
    }

    #[test]
    fn set_log_level_includes_log_filter_when_scoped() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("PUT", "/teku/v1/admin/log_level")
            .match_body(mockito::Matcher::Json(
                serde_json::json!({"level": "DEBUG", "log_filter": ["org.example"]}),
            ))
            .with_status(200)
            .create();
        let client = BeaconClient::new(server.url());
        client
            .set_log_level("DEBUG", Some(vec!["org.example".to_string()]))
            .unwrap();
    }

    /// Closes the seam between the two halves of the gist path: what
    /// `loglevel::parse_spec` makes of a prepared body, and what actually goes
    /// on the wire. Each side is tested on its own, so only a test spanning
    /// both catches them agreeing on different things.
    #[test]
    fn a_body_parsed_from_a_gist_reaches_the_wire_unchanged() {
        let spec = crate::loglevel::parse_spec(
            br#"{"level": "DEBUG", "log_filter": ["tech.pegasys.teku.sync"]}"#,
            "https://gist.github.com/someone/abc123",
        )
        .expect("the prepared body must parse");

        let mut server = mockito::Server::new();
        let _m = server
            .mock("PUT", "/teku/v1/admin/log_level")
            .match_body(mockito::Matcher::Json(serde_json::json!({
                "level": "DEBUG",
                "log_filter": ["tech.pegasys.teku.sync"],
            })))
            .with_status(200)
            .create();
        let client = BeaconClient::new(server.url());
        client.set_log_level(&spec.level, spec.log_filter).unwrap();
    }

    #[test]
    fn set_log_level_errors_on_non_2xx_status() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("PUT", "/teku/v1/admin/log_level")
            .with_status(400)
            .create();
        let client = BeaconClient::new(server.url());
        let err = client.set_log_level("NOT_A_LEVEL", None).unwrap_err();
        assert!(matches!(err, ApiError::Status(400, _)));
    }
}
