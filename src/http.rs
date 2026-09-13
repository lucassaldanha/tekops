use crate::term::sanitize;
use std::fmt;
use std::time::Duration;

/// How long any single request may spend connecting, reading, or writing
/// before it's abandoned. `ureq`'s own defaults leave reads and writes
/// unbounded, which lets a node that accepts a connection but never answers
/// (an overloaded or wedged Teku - exactly when these commands get run) hang
/// the CLI forever with no output and no error. Every command here is a
/// one-shot query against a local process, so seconds is the right scale.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Builds the `ureq` agent used for every request this binary makes. Both
/// `BeaconClient` and `MetricsClient` hold one of these rather than calling
/// `ureq`'s free functions, so that timeouts (and any future transport
/// policy) are configured in exactly one place instead of implicitly
/// defaulting per call site.
pub(crate) fn agent() -> ureq::Agent {
    agent_with_timeout(REQUEST_TIMEOUT)
}

fn agent_with_timeout(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .timeout_write(timeout)
        .build()
}

pub(crate) fn map_ureq_error(e: ureq::Error) -> ApiError {
    match e {
        ureq::Error::Status(code, resp) => {
            ApiError::Status(code, sanitize(&resp.into_string().unwrap_or_default()))
        }
        ureq::Error::Transport(t) => ApiError::Unreachable(t.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Instant;

    /// A socket that accepts connections and then never answers. Without a
    /// read timeout this hangs the caller forever; the point of the test is
    /// that it now fails fast instead. Uses a short injected timeout so the
    /// suite doesn't pay the production value in wall clock.
    #[test]
    fn request_to_a_silent_endpoint_times_out_rather_than_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        // Hold the accepted connection open (and unanswered) for the duration.
        std::thread::spawn(move || {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept() {
                held.push(stream);
            }
        });

        let timeout = Duration::from_millis(250);
        let started = Instant::now();
        let result = agent_with_timeout(timeout)
            .get(&format!("http://{addr}/eth/v1/node/health"))
            .call();
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected a timeout error, got a response");
        assert!(
            elapsed < timeout * 20,
            "request should have timed out promptly, took {elapsed:?}"
        );
    }

    /// Guards the wiring the test above deliberately bypasses: the agent the
    /// binary actually uses must carry a bounded timeout, not ureq's unbounded
    /// default.
    #[test]
    fn production_agent_has_a_bounded_timeout() {
        assert!(REQUEST_TIMEOUT > Duration::ZERO);
        assert!(REQUEST_TIMEOUT <= Duration::from_secs(60));
        assert!(format!("{:?}", agent()).contains("timeout_read: Some"));
    }

    #[test]
    fn agent_still_completes_a_normal_request() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi")
                .unwrap();
        });

        let resp = agent().get(&format!("http://{addr}/")).call().unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.into_string().unwrap(), "hi");
    }

    #[test]
    fn status_error_body_is_sanitized() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let body = "\u{1b}[2Jfire";
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .unwrap();
        });

        let err = map_ureq_error(agent().get(&format!("http://{addr}/")).call().unwrap_err());
        match err {
            ApiError::Status(500, body) => {
                assert!(!body.contains('\u{1b}'), "ESC survived: {body:?}")
            }
            other => panic!("expected a 500 status error, got {other:?}"),
        }
    }
}
