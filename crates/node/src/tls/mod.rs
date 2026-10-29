//! The public API's server: TLS terminated inside the enclave, so the host can neither read nor
//! alter client traffic, under the connection caps and timeouts in `[api]`. The key is generated
//! in the enclave at boot and never leaves it; a restart makes a new one, and a new attestation.
//! What makes the key trustworthy is the attestation, whose bundle carries the SHA-256 of its DER
//! SubjectPublicKeyInfo in bytes 73..105 (see `tee::AttestationBundle`). A client that has checked
//! the quote, and that the bundle hashes to its report data, pins that hash, which is the key pin
//! curl takes as `--pinnedpubkey sha256//<base64 of bundle bytes 73..105>`.
//!
//! The certificate is self-signed until `[api.tls.acme]` has a CA issue one for the same key
//! (`acme`), so browsers trust the API's domain too. The CA validates the domain by TLS-ALPN-01
//! (RFC 8737) on this listener: a handshake offering `acme-tls/1` gets the challenge certificate
//! while one is being answered, and every other handshake the served certificate with `http/1.1`.

use std::io::{self, ErrorKind};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::extract::{ConnectInfo, Request};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use rcgen::{CertificateParams, DnType, KeyPair, PublicKeyData};
use rustls::ServerConfig;
use rustls::crypto::ring::default_provider;
use rustls::server::Acceptor;
use rustls::sign::CertifiedKey;
use rustls::version::TLS13;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::{Sleep, sleep, timeout};
use tokio_rustls::LazyConfigAcceptor;
use tokio_rustls::server::TlsStream;
use tower_service::Service;
use tracing::debug;

pub mod acme;
mod certificates;
use crate::config::ApiSection;
use crate::limits::{ApiLimits, ConnectionCaps, ConnectionSlot, ThrottledAlert};
pub use crate::tls::certificates::TlsKey;
use crate::tls::certificates::{Answering, Served, load_key};

/// The ALPN protocol of a TLS-ALPN-01 validation (RFC 8737).
const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";

/// The API's TLS key and the certificates served for it.
pub struct TlsIdentity {
    pub acceptors: Acceptors,
    /// SHA-256 of the key's DER SubjectPublicKeyInfo: what the attestation commits to.
    pub public_key_sha256: [u8; 32],
    /// The key, and the certificates it serves: what `acme` needs to have a CA certify it.
    pub key: TlsKey,
}

/// The server configs a connection's ClientHello picks between: the API's, and the one that
/// answers a TLS-ALPN-01 validation.
#[derive(Clone)]
pub struct Acceptors {
    api: Arc<ServerConfig>,
    validation: Arc<ServerConfig>,
}

/// A connection after its handshake.
enum Accepted {
    Api(Box<TlsStream<TcpStream>>),
    /// A TLS-ALPN-01 validation, answered and closed.
    Validation,
}

/// A client connection after its TLS handshake. It holds its place under the connection caps
/// until it closes, including after an upgrade to a WebSocket, which takes it over. A write that
/// the client leaves blocked for `send_timeout` fails, which closes the connection.
struct TlsConn {
    stream: TlsStream<TcpStream>,
    send_timeout: Duration,
    /// Running while a write is blocked on the client.
    stalled: Option<Pin<Box<Sleep>>>,
    _slot: ConnectionSlot,
}

impl AsyncRead for TlsConn {
    /// Reads decrypted bytes.
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsConn {
    /// Writes bytes to be encrypted.
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let written = Pin::new(&mut self.stream).poll_write(cx, buf);
        self.within_send_timeout(cx, written)
    }

    /// Writes a vector of buffers to be encrypted.
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let written = Pin::new(&mut self.stream).poll_write_vectored(cx, bufs);
        self.within_send_timeout(cx, written)
    }

    /// Whether vectored writes are efficient on the TLS stream.
    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    /// Flushes encrypted bytes to the socket.
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let flushed = Pin::new(&mut self.stream).poll_flush(cx);
        self.within_send_timeout(cx, flushed)
    }

    /// Sends the TLS close and shuts the socket down for writing.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let shut = Pin::new(&mut self.stream).poll_shutdown(cx);
        self.within_send_timeout(cx, shut)
    }
}

impl TlsConn {
    /// Passes a write's outcome through, but fails a write the client has left blocked for
    /// `send_timeout`, so a client that stops reading gives up its place.
    fn within_send_timeout<T>(&mut self, cx: &mut Context<'_>, outcome: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if outcome.is_ready() {
            self.stalled = None;
            return outcome;
        }
        let send_timeout = self.send_timeout;
        let stalled = self.stalled.get_or_insert_with(|| Box::pin(sleep(send_timeout)));
        match stalled.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(ErrorKind::TimedOut, "client stopped reading"))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl TlsIdentity {
    /// A fresh ECDSA P-256 key from the enclave's RNG and a self-signed certificate for it, served
    /// over TLS 1.3 and HTTP/1.1 only until `key` installs a CA certificate in its place. A
    /// validator may speak TLS 1.2 as well.
    ///
    /// TODO: survive restarts by sealing the key to the measurement (#28), so a restart reuses its
    /// certificate instead of ordering a new one.
    /// Error cases:
    /// - the key, the certificate, or a server config cannot be built
    pub fn generate() -> anyhow::Result<Self> {
        let provider = Arc::new(default_provider());
        let key_pair = KeyPair::generate().context("generating the TLS key")?;
        let mut params = CertificateParams::new(Vec::<String>::new()).context("certificate parameters")?;
        params.distinguished_name.push(DnType::CommonName, "engine enclave");
        let cert = params.self_signed(&key_pair).context("signing the TLS certificate")?;
        let public_key_sha256 = Sha256::digest(key_pair.subject_public_key_info()).into();
        let signer = load_key(&provider, &key_pair)?;

        let (served, served_rx) = watch::channel(Arc::new(CertifiedKey::new(vec![cert.der().clone()], signer.clone())));
        let (challenge, challenge_rx) = watch::channel(None);
        let mut api = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&TLS13])
            .context("TLS protocol versions")?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Served(served_rx)));
        api.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut validation = ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .context("TLS protocol versions")?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Answering(challenge_rx)));
        validation.alpn_protocols = vec![ACME_TLS_ALPN.to_vec()];
        Ok(Self {
            acceptors: Acceptors { api: Arc::new(api), validation: Arc::new(validation) },
            public_key_sha256,
            key: TlsKey::new(provider, key_pair, signer, public_key_sha256, served, challenge),
        })
    }
}

/// Serves `app` to clients on `listener` until this future is dropped, which aborts every HTTP
/// connection it serves, so none keeps the sequencer's channel open while the node shuts down. A
/// connection upgraded to a stream runs on outside it, holding no sender, and ends when the stream
/// does. A connection over a cap or over its client's connection rate is closed unread. Each other
/// one gets its own task, so a slow client delays nobody else: it must finish its TLS handshake
/// within `api.handshake_timeout`, send each request's headers within `api.read_timeout`, which
/// also closes a connection idle that long between requests, and take what it is sent within
/// `api.send_timeout`. A TLS-ALPN-01 validation is a connection like any other until its handshake
/// ends, and is then closed. Handlers see the socket peer as `ConnectInfo<SocketAddr>`.
/// Error cases:
/// - none; it runs until the node stops
pub async fn serve(
    listener: TcpListener,
    acceptors: Acceptors,
    app: Router,
    limits: Arc<ApiLimits>,
    api: ApiSection,
) -> anyhow::Result<()> {
    let caps = ConnectionCaps::new(api.max_connections.get(), api.max_connections_per_ip.get());
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new()).header_read_timeout(api.read_timeout);
    let mut connections = JoinSet::new();
    let accept_failed = ThrottledAlert::new("api_accept_failed", "accepting connections is failing");
    loop {
        // Reaps finished connections, so the set holds open ones only.
        while connections.try_join_next().is_some() {}
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                back_off(e, &accept_failed).await;
                continue;
            }
        };
        let Some(slot) = caps.admit(peer.ip()) else { continue };
        if limits.check_connection(peer.ip()).is_err() {
            continue;
        }
        let (acceptors, http, app) = (acceptors.clone(), http.clone(), app.clone());
        connections.spawn(async move {
            let stream = match timeout(api.handshake_timeout, accept(tcp, &acceptors)).await {
                Ok(Ok(Accepted::Api(stream))) => *stream,
                Ok(Ok(Accepted::Validation)) => return debug!("answered a TLS-ALPN-01 validation"),
                Ok(Err(e)) => return debug!("TLS handshake failed: {e}"),
                Err(_) => return debug!("TLS handshake timed out"),
            };
            let service = service_fn(move |mut req: Request<Incoming>| {
                req.extensions_mut().insert(ConnectInfo(peer));
                app.clone().call(req)
            });
            let conn = TlsConn { stream, send_timeout: api.send_timeout, stalled: None, _slot: slot };
            if let Err(e) = http.serve_connection(TokioIo::new(conn), service).with_upgrades().await {
                debug!("connection closed: {e}");
            }
        });
    }
}

/// Completes the handshake under the config the ClientHello asks for: a validation, which offers
/// `acme-tls/1`, gets the challenge certificate for its server name, or no handshake when nothing
/// is being validated for that name; every other hello gets the API's config.
/// Error cases:
/// - the client sends no ClientHello, or the handshake fails
async fn accept(tcp: TcpStream, acceptors: &Acceptors) -> io::Result<Accepted> {
    let start = LazyConfigAcceptor::new(Acceptor::default(), tcp).await?;
    let validating = start.client_hello().alpn().is_some_and(|mut offered| offered.any(|p| p == ACME_TLS_ALPN));
    if !validating {
        return Ok(Accepted::Api(Box::new(start.into_stream(acceptors.api.clone()).await?)));
    }
    let mut stream = start.into_stream(acceptors.validation.clone()).await?;
    // The validator learns everything from the handshake (RFC 8737 §3); a failed close leaves
    // nothing to do.
    if let Err(e) = stream.shutdown().await {
        debug!("closing a TLS-ALPN-01 validation: {e}");
    }
    Ok(Accepted::Validation)
}

/// Backs off after a failed accept: a client that reset its own connection is no fault of ours,
/// while anything else (out of file descriptors) raises `alert` and is waited out.
async fn back_off(e: io::Error, alert: &ThrottledAlert) {
    if matches!(e.kind(), ErrorKind::ConnectionAborted | ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset) {
        return;
    }
    alert.raise(&e);
    sleep(Duration::from_secs(1)).await;
}

#[cfg(test)]
mod tests;
