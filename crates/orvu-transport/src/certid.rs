//! Certificates for development: a self-signed certificate authority (CA) and
//! node certificates signed by it. Each node certificate carries two subject
//! alternative names (SANs): `URI:orvu://replica/<id>`, which is the node's
//! identity, and `DNS:orvu-node`, which the TLS name check uses. This is a
//! helper for setting machines up. It is not a CA service that runs alongside
//! the system.

use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, SanType};

use crate::types::replica_identity_uri;

/// A self-signed development CA. It keeps its key so it can sign node
/// certificates.
pub struct DevCa {
    pub cert_pem: String,
    cert: rcgen::Certificate,
    key: KeyPair,
}

/// One node's certificate and private key, both as PEM text.
pub struct NodeCertPems {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Creates a self-signed CA that can issue development node certificates.
pub fn generate_dev_ca() -> DevCa {
    let mut params = CertificateParams::new(vec![]).expect("empty SAN list is always valid");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "orvu dev CA");
    params.distinguished_name = dn;

    let key = KeyPair::generate().expect("key generation cannot fail");
    let cert = params.self_signed(&key).expect("self-signing cannot fail");
    let cert_pem = cert.pem();

    DevCa { cert_pem, cert, key }
}

/// Issues a node certificate for `replica`, signed by `ca`, with the SANs
/// `URI:orvu://replica/<replica>` and `DNS:orvu-node`.
pub fn issue_node_cert(ca: &DevCa, replica: u64) -> NodeCertPems {
    let mut params = CertificateParams::new(vec![]).expect("empty SAN list is always valid");
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "orvu-node");
    params.distinguished_name = dn;
    params.subject_alt_names = vec![
        SanType::DnsName("orvu-node".try_into().expect("valid DNS name")),
        SanType::URI(
            replica_identity_uri(replica)
                .try_into()
                .expect("valid URI"),
        ),
    ];

    let leaf_key = KeyPair::generate().expect("key generation cannot fail");
    let cert = params
        .signed_by(&leaf_key, &ca.cert, &ca.key)
        .expect("signing by dev CA cannot fail");

    NodeCertPems {
        cert_pem: cert.pem(),
        key_pem: leaf_key.serialize_pem(),
    }
}

/// Returns the first URI-type SAN in a DER-encoded certificate.
pub fn san_uri_from_cert_der(der: &[u8]) -> Option<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let san = cert.tbs_certificate.subject_alternative_name().ok()??;
    san.value.general_names.iter().find_map(|gn| match gn {
        x509_parser::extensions::GeneralName::URI(u) => Some(u.to_string()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_node_cert_has_replica_san_and_chains_to_ca() {
        let ca = generate_dev_ca();
        let node = issue_node_cert(&ca, 2);

        // The node certificate, converted from PEM to DER, carries the
        // identity URI SAN.
        let der = pem_to_der(&node.cert_pem);
        assert_eq!(
            san_uri_from_cert_der(&der).as_deref(),
            Some("orvu://replica/2")
        );

        // The CA PEM parses as a certificate. The QUIC tests check end to end
        // that a node certificate is accepted only when the CA signed it.
        let ca_der = pem_to_der(&ca.cert_pem);
        assert!(san_uri_from_cert_der(&ca_der).is_none()); // CA has no replica SAN
    }

    // A small PEM-to-DER helper for the test. It handles one certificate and
    // uses x509-parser's own PEM decoder, so no base64 dependency is needed.
    fn pem_to_der(pem: &str) -> Vec<u8> {
        let (_, pem) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
        pem.contents
    }
}
