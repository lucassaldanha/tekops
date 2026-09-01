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

    pub fn health(&self) -> Result<HealthState, ApiError> {
        match self.request_status("/eth/v1/node/health")? {
            200 => Ok(HealthState::Ready),
            206 => Ok(HealthState::Syncing),
            503 => Ok(HealthState::NotReady),
            other => Err(ApiError::Status(other, "unexpected health status".to_string())),
        }
    }

    pub fn syncing(&self) -> Result<SyncingStatus, ApiError> {
        let resp = ureq::get(&self.url("/eth/v1/node/syncing"))
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => {
                    ApiError::Status(code, resp.into_string().unwrap_or_default())
                }
                ureq::Error::Transport(t) => ApiError::Unreachable(t.to_string()),
            })?;
        let parsed: SyncingResponse = resp
            .into_json()
            .map_err(|e| ApiError::Status(0, format!("invalid JSON: {e}")))?;
        Ok(SyncingStatus {
            is_syncing: parsed.data.is_syncing,
            is_optimistic: parsed.data.is_optimistic,
            head_slot: parsed.data.head_slot,
            sync_distance: parsed.data.sync_distance,
        })
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
}
