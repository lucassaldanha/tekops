use enr::{k256::ecdsa::SigningKey, Enr};
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tcp,
    Quic,
    Unknown,
}

pub fn classify_protocol(enr_str: &str) -> Protocol {
    let Ok(enr) = Enr::<SigningKey>::from_str(enr_str) else {
        return Protocol::Unknown;
    };
    if enr.get_raw_rlp("quic").is_some() || enr.get_raw_rlp("quic6").is_some() {
        Protocol::Quic
    } else if enr.get_raw_rlp("tcp").is_some() || enr.get_raw_rlp("tcp6").is_some() {
        Protocol::Tcp
    } else {
        Protocol::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use enr::Builder;
    use k256::ecdsa::SigningKey;
    use rand::rngs::OsRng;

    #[test]
    fn classifies_tcp_enr() {
        let key = SigningKey::random(&mut OsRng);
        let enr = Builder::<SigningKey>::default()
            .tcp4(9000)
            .build(&key)
            .unwrap();
        assert_eq!(classify_protocol(&enr.to_base64()), Protocol::Tcp);
    }

    #[test]
    fn classifies_quic_enr() {
        let key = SigningKey::random(&mut OsRng);
        let enr = Builder::<SigningKey>::default()
            .add_value("quic", &9001u16.to_be_bytes().as_slice())
            .build(&key)
            .unwrap();
        assert_eq!(classify_protocol(&enr.to_base64()), Protocol::Quic);
    }

    #[test]
    fn unknown_on_garbage_input() {
        assert_eq!(classify_protocol("not-an-enr"), Protocol::Unknown);
    }
}
