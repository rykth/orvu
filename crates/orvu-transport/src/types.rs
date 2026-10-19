//! Peer addresses and peer identity.

/// Where a peer can be reached. It is a plain string, and each transport
/// decides how to read it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeerAddr(pub String);

impl PeerAddr {
    pub fn new(addr: impl Into<String>) -> Self {
        Self(addr.into())
    }
}

/// Who the peer on a connection is, as shown during the TLS handshake.
///
/// `uri` is the peer's verified identity URI (`orvu://replica/<id>`). For
/// QUIC it is read from the subject alternative name (SAN) of the peer's
/// certificate, after the certificate has been verified. For `MemTransport`
/// the in-process network assigns it. `None` means no identity was
/// established.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    pub uri: Option<String>,
    pub fingerprint: Option<String>,
}

/// The standard identity URI for a replica: `orvu://replica/<id>`.
pub fn replica_identity_uri(replica: u64) -> String {
    format!("orvu://replica/{replica}")
}

/// Reads the replica id out of an `orvu://replica/<id>` identity URI.
/// Returns `None` for anything else, including a number not written in its
/// plain decimal form (a leading `+` or leading zeros).
pub fn parse_replica_identity_uri(uri: &str) -> Option<u64> {
    let rest = uri.strip_prefix("orvu://replica/")?;
    if rest.is_empty() || rest.contains('/') {
        return None;
    }
    let n = rest.parse::<u64>().ok()?;
    if rest != n.to_string() {
        return None;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replica_uri_round_trips() {
        assert_eq!(replica_identity_uri(7), "orvu://replica/7");
        assert_eq!(parse_replica_identity_uri("orvu://replica/7"), Some(7));
    }

    #[test]
    fn parse_rejects_malformed_uris() {
        assert_eq!(parse_replica_identity_uri("orvu://replica/"), None);
        assert_eq!(parse_replica_identity_uri("orvu://replica/x"), None);
        assert_eq!(parse_replica_identity_uri("http://replica/7"), None);
        assert_eq!(parse_replica_identity_uri("orvu://replica/7/extra"), None);
        assert_eq!(parse_replica_identity_uri("orvu://other/7"), None);
        assert_eq!(parse_replica_identity_uri("orvu://replica/+5"), None);
        assert_eq!(parse_replica_identity_uri("orvu://replica/007"), None);
        assert_eq!(parse_replica_identity_uri("orvu://replica/0"), Some(0));
        assert_eq!(parse_replica_identity_uri("orvu://replica/42"), Some(42));
    }
}
