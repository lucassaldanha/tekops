use std::fmt;

#[derive(Debug)]
pub enum ApiError {
    Unreachable(String),
    Status(u16, String),
    Malformed(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Unreachable(msg) => write!(f, "could not reach endpoint: {msg}"),
            ApiError::Status(code, msg) => write!(f, "endpoint returned {code}: {msg}"),
            ApiError::Malformed(msg) => write!(f, "endpoint returned malformed data: {msg}"),
        }
    }
}

pub(crate) fn map_ureq_error(e: ureq::Error) -> ApiError {
    match e {
        ureq::Error::Status(code, resp) => {
            ApiError::Status(code, resp.into_string().unwrap_or_default())
        }
        ureq::Error::Transport(t) => ApiError::Unreachable(t.to_string()),
    }
}
