//! A client's connection to the API as a trader's arrives through the operator's pass-through proxy:
//! a TCP connection from the loopback, the node's trusted proxy, that opens with a PROXY protocol v2
//! header naming the client's own address, then TLS, then HTTP/1.1 kept alive, or a WebSocket. The
//! node's certificate is accepted unchecked: the loopback is the run's own.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use anyhow::Context;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::client::conn::http1::{SendRequest, handshake};
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use ppp::v2::{Builder, Command, Protocol, Version};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring::default_provider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// The TLS client config every connection uses.
pub(super) fn tls_config() -> Arc<ClientConfig> {
    let provider = Arc::new(default_provider());
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("TLS protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Unchecked(provider)))
        .with_no_client_auth();
    Arc::new(config)
}

/// Accepts the node's certificate and handshake signature: the run trusts its own loopback.
#[derive(Debug)]
struct Unchecked(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for Unchecked {
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

/// A TLS connection to `api` for the client at `client`, through the proxy's header.
/// Error cases:
/// - the connection, the header's write or the handshake fails
pub(super) async fn connect(
    api: SocketAddr,
    client: IpAddr,
    tls: &Arc<ClientConfig>,
) -> io::Result<TlsStream<TcpStream>> {
    let mut tcp = TcpStream::connect(api).await?;
    tcp.set_nodelay(true)?;
    // Where the proxy accepted the client; the node never reads it.
    let proxy_side: SocketAddr = "192.0.2.1:443".parse().expect("an address");
    let header = Builder::with_addresses(
        Version::Two | Command::Proxy,
        Protocol::Stream,
        (SocketAddr::new(client, 4_000), proxy_side),
    )
    .build()
    .map_err(|e| io::Error::other(format!("a PROXY header: {e}")))?;
    tcp.write_all(&header).await?;
    TlsConnector::from(tls.clone()).connect(ServerName::try_from("engine").expect("a name"), tcp).await
}

/// One client's HTTP/1.1 connection, kept alive between requests.
pub(super) struct Http {
    sender: SendRequest<Full<Bytes>>,
    connection: JoinHandle<()>,
}

impl Drop for Http {
    fn drop(&mut self) {
        self.connection.abort();
    }
}

impl Http {
    /// Opens a connection for the client at `client`.
    /// Error cases:
    /// - as `connect`, or the HTTP handshake fails
    pub(super) async fn open(api: SocketAddr, client: IpAddr, tls: &Arc<ClientConfig>) -> anyhow::Result<Self> {
        let stream = connect(api, client, tls).await.context("connecting")?;
        let (sender, connection) = handshake(TokioIo::new(stream)).await.context("the HTTP handshake")?;
        let connection = tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(Http { sender, connection })
    }

    /// Whether the server has closed the connection, as it does one idle for its read timeout.
    pub(super) fn closed(&self) -> bool {
        self.sender.is_closed() || self.connection.is_finished()
    }

    /// Sends `body` to `path` with `method` and returns the reply's status and body.
    /// Error cases:
    /// - the connection fails before the reply is read
    pub(super) async fn send(&mut self, method: Method, path: &str, body: String) -> anyhow::Result<(u16, Bytes)> {
        self.sender.ready().await.context("the connection")?;
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header(HOST, "engine")
            .header(CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))
            .expect("a request");
        let response = self.sender.send_request(request).await.context("sending")?;
        let status = response.status().as_u16();
        let body = response.into_body().collect().await.context("the reply's body")?.to_bytes();
        Ok((status, body))
    }
}
