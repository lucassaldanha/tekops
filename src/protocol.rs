use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Tcp,
    Quic,
}

/// Classifies a peer's transport from its `last_seen_p2p_address` multiaddr.
/// Mirrors the working `jq` query: quic if the address advertises a `/quic`
/// component, tcp otherwise.
pub fn classify_protocol(address: &str) -> Protocol {
    if address.contains("/quic") {
        Protocol::Quic
    } else {
        Protocol::Tcp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_quic_multiaddr() {
        assert_eq!(classify_protocol("/ip4/1.2.3.4/udp/9001/quic"), Protocol::Quic);
    }

    #[test]
    fn classifies_tcp_multiaddr() {
        assert_eq!(classify_protocol("/ip4/1.2.3.4/tcp/9000"), Protocol::Tcp);
    }
}
