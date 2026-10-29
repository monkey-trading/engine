//! TLS-ALPN-01 on the API's own listener, and a CA certificate taking over from the self-signed one
//! without a restart.

use super::*;

use rcgen::{CertificateParams, DnType, IsCa, Issuer, KeyPair};
use rustls::RootCertStore;
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::GeneralName;
use x509_parser::prelude::FromDer;

use crate::tls::ACME_TLS_ALPN;

const DOMAIN: &str = "api.engine.test";
/// A key authorization's SHA-256, as the CA would derive it.
const DIGEST: [u8; 32] = [7; 32];
/// id-pe-acmeIdentifier (RFC 8737 §6.1).
const ACME_IDENTIFIER: &str = "1.3.6.1.5.5.7.1.31";

/// Accepts any certificate and handshake signature, as a validator does (RFC 8737 §3): webpki
/// refuses the challenge certificate's critical acmeIdentifier extension, which it does not know.
#[derive(Debug)]
struct Unverified(Arc<CryptoProvider>);

impl ServerCertVerifier for Unverified {
    /// Accepts the certificate.
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    /// Accepts the signature.
    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    /// Accepts the signature.
    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    /// Every scheme the provider verifies.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[tokio::test]
async fn a_validation_gets_the_challenge_certificate_only_while_it_is_answered_for_its_name() {
    let node = TestNode::start(api(), V4).await;
    assert!(node.handshake(DOMAIN, ACME_TLS_ALPN).await.is_err(), "nothing is being validated");

    let answer = node.key.answer(DOMAIN, DIGEST).unwrap();
    let mut validation = node.handshake(DOMAIN, ACME_TLS_ALPN).await.unwrap();
    let (_, connection) = validation.get_ref();
    assert_eq!(connection.alpn_protocol(), Some(ACME_TLS_ALPN));
    let presented = connection.peer_certificates().unwrap()[0].clone();
    let (_, challenge) = X509Certificate::from_der(&presented).unwrap();
    let identifier =
        challenge.extensions().iter().find(|e| e.oid.to_id_string() == ACME_IDENTIFIER).expect("acmeIdentifier");
    assert!(identifier.critical);
    assert_eq!(identifier.value, [&[0x04, 32][..], &DIGEST].concat(), "an OCTET STRING of the digest");
    let names = challenge.subject_alternative_name().unwrap().unwrap().value.general_names.clone();
    assert_eq!(names, [GeneralName::DNSName(DOMAIN)]);
    let challenge_key: [u8; 32] = Sha256::digest(challenge.public_key().raw).into();
    assert_ne!(challenge_key, attested_tls_key(&node.attestation), "a throwaway key, not the API's");
    assert_eq!(read_until_closed(&mut validation).await, "", "closed after the handshake, with nothing served");

    assert!(node.handshake("other.engine.test", ACME_TLS_ALPN).await.is_err(), "another name gets nothing");
    assert!(node.handshake(&DOMAIN.to_uppercase(), ACME_TLS_ALPN).await.is_ok(), "names compare without case");
    // Every other handshake gets the API's certificate and HTTP, meanwhile.
    let mut api = node.handshake(DOMAIN, b"http/1.1").await.unwrap();
    assert_eq!(api.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
    assert_eq!(presented_key(&api), attested_tls_key(&node.attestation));
    assert_eq!(http(&mut api, "GET", "/v1/attestation", "").await.0, 200);

    drop(answer);
    assert!(node.handshake(DOMAIN, ACME_TLS_ALPN).await.is_err(), "a finished attempt leaves nothing answered");
}

#[tokio::test]
async fn an_installed_ca_certificate_serves_the_next_handshake_without_a_restart() {
    let node = TestNode::start(api(), V4).await;
    let mut before = node.connect(V4).await.unwrap();
    let (root, chain) = ca_chain(&node.key);
    // A client that trusts only the CA, as a browser trusts its roots, refuses the self-signed one.
    assert!(node.verified(&root).await.is_err());

    node.key.install(chain).unwrap();
    let mut verified = node.verified(&root).await.unwrap();
    assert_eq!(presented_key(&verified), attested_tls_key(&node.attestation), "the attested key, now CA-issued");
    assert_eq!(http(&mut verified, "GET", "/v1/attestation", "").await.0, 200);
    assert_eq!(http(&mut before, "GET", "/v1/attestation", "").await.0, 200, "an open connection carries on");

    // A certificate for any other key is refused, and the CA certificate stays served.
    let other = TlsIdentity::generate().unwrap();
    assert!(node.key.install(ca_chain(&other.key).1).is_err());
    let verified = node.verified(&root).await.unwrap();
    assert_eq!(presented_key(&verified), attested_tls_key(&node.attestation));
}

#[tokio::test]
async fn validation_connections_are_held_to_the_connection_caps() {
    let mut api = api();
    api.max_connections_per_ip = NonZeroUsize::new(1).unwrap();
    let node = TestNode::start(api, V4).await;
    let _answer = node.key.answer(DOMAIN, DIGEST).unwrap();
    let held = node.connect(V4).await.unwrap();
    assert!(node.handshake(DOMAIN, ACME_TLS_ALPN).await.is_err(), "over the per-client cap: closed unread");
    drop(held);
    for _ in 0..500 {
        if node.handshake(DOMAIN, ACME_TLS_ALPN).await.is_ok() {
            return;
        }
    }
    panic!("the validation was not answered once its place was freed");
}

#[tokio::test]
async fn a_validation_that_stalls_mid_handshake_is_closed_at_the_timeout() {
    let mut api = api();
    api.max_connections_per_ip = NonZeroUsize::new(1).unwrap();
    api.handshake_timeout = Duration::from_millis(200);
    let node = TestNode::start(api, V4).await;
    let _answer = node.key.answer(DOMAIN, DIGEST).unwrap();
    // A ClientHello offering acme-tls/1 for the domain, then nothing: the server answers and waits
    // for the client's Finished.
    let mut config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Unverified(Arc::new(default_provider()))))
        .with_no_client_auth();
    config.alpn_protocols = vec![ACME_TLS_ALPN.to_vec()];
    let mut hello = Vec::new();
    rustls::ClientConnection::new(Arc::new(config), ServerName::try_from(DOMAIN).unwrap())
        .unwrap()
        .write_tls(&mut hello)
        .unwrap();
    let mut stalled = TcpStream::connect((V4, node.port)).await.unwrap();
    stalled.write_all(&hello).await.unwrap();
    assert!(node.connect(V4).await.is_err(), "the stalled validation holds the only place");

    let mut answered = Vec::new();
    let closed = timeout(Duration::from_secs(5), stalled.read_to_end(&mut answered)).await;
    closed.expect("closed by the server").unwrap();
    assert!(!answered.is_empty(), "the server took the hello and answered it");
    node.connect_eventually(V4).await;
}

// ── Helpers ──

impl TestNode {
    /// A handshake from 127.0.0.1 naming `server_name` and offering `alpn` alone, accepting any
    /// certificate.
    /// Error cases:
    /// - the connection or handshake fails
    async fn handshake(&self, server_name: &str, alpn: &[u8]) -> io::Result<ClientTls<TcpStream>> {
        let provider = Arc::new(default_provider());
        let verifier = Arc::new(Unverified(provider.clone()));
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = vec![alpn.to_vec()];
        let tcp = TcpStream::connect((V4, self.port)).await?;
        let name = ServerName::try_from(server_name.to_owned()).unwrap();
        TlsConnector::from(Arc::new(config)).connect(name, tcp).await
    }

    /// A handshake for `DOMAIN` that verifies the chain against `root` alone.
    /// Error cases:
    /// - the connection or handshake fails, verification included
    async fn verified(&self, root: &CertificateDer<'static>) -> io::Result<ClientTls<TcpStream>> {
        let mut roots = RootCertStore::empty();
        roots.add(root.clone()).unwrap();
        let config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tcp = TcpStream::connect((V4, self.port)).await?;
        TlsConnector::from(Arc::new(config)).connect(ServerName::try_from(DOMAIN).unwrap(), tcp).await
    }
}

/// A fresh CA's root, and a chain it issued for `key` naming `DOMAIN`.
fn ca_chain(key: &TlsKey) -> (CertificateDer<'static>, Vec<CertificateDer<'static>>) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.distinguished_name.push(DnType::CommonName, "test CA");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let root = ca.self_signed(&ca_key).unwrap().der().clone();
    let leaf = CertificateParams::new(vec![DOMAIN.to_owned()]).unwrap();
    let leaf = leaf.signed_by(key.key_pair(), &Issuer::new(ca, &ca_key)).unwrap();
    (root.clone(), vec![leaf.der().clone(), root])
}

/// SHA-256 of the DER SubjectPublicKeyInfo of the certificate `conn`'s server presented.
fn presented_key(conn: &ClientTls<TcpStream>) -> [u8; 32] {
    let leaf = conn.get_ref().1.peer_certificates().unwrap()[0].clone();
    let (_, leaf) = X509Certificate::from_der(&leaf).unwrap();
    Sha256::digest(leaf.public_key().raw).into()
}
