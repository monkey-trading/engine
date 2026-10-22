//! The public API's server: TLS terminated inside the enclave, so the host can neither read nor
//! alter client traffic, under the connection caps and timeouts in `[api]`. The key is generated
//! in the enclave at boot and never leaves it; a restart makes a new one, and a new attestation.
//! The certificate is self-signed; what makes it trustworthy is the attestation, whose bundle
//! carries the SHA-256 of the certificate's DER SubjectPublicKeyInfo in bytes 73..105 (see
//! `tee::AttestationBundle`). A client that has checked the quote, and that the bundle hashes to its
//! report data, pins that hash, which is the key pin curl takes as
//! `--pinnedpubkey sha256//<base64 of bundle bytes 73..105>`.

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
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::version::TLS13;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio::time::{Sleep, sleep, timeout};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tower_service::Service;
use tracing::debug;

use crate::config::ApiSection;
use crate::limits::{ApiLimits, ConnectionCaps, ConnectionSlot, ThrottledAlert};

/// The API's TLS key and certificate.
pub struct TlsIdentity {
    pub config: Arc<ServerConfig>,
    /// SHA-256 of the certificate's DER SubjectPublicKeyInfo: what the attestation commits to.
    pub public_key_sha256: [u8; 32],
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
    /// A fresh ECDSA P-256 key from the enclave's RNG, a self-signed certificate for it, and a
    /// server config that speaks TLS 1.3 and HTTP/1.1 only.
    ///
    /// TODO: serve a CA-issued certificate for the API's domain, its key still generated here.
    /// Error cases:
    /// - the key, the certificate, or the server config cannot be built
    pub fn generate() -> anyhow::Result<Self> {
        let key = KeyPair::generate().context("generating the TLS key")?;
        let mut params = CertificateParams::new(Vec::<String>::new()).context("certificate parameters")?;
        params.distinguished_name.push(DnType::CommonName, "engine enclave");
        let cert = params.self_signed(&key).context("signing the TLS certificate")?;
        let public_key_sha256 = Sha256::digest(key.subject_public_key_info()).into();

        let mut config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
            .with_protocol_versions(&[&TLS13])
            .context("TLS protocol versions")?
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], PrivatePkcs8KeyDer::from(key.serialize_der()).into())
            .context("TLS certificate")?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self { config: Arc::new(config), public_key_sha256 })
    }
}

/// Serves `app` to clients on `listener` until this future is dropped, which aborts every HTTP
/// connection it serves, so none keeps the sequencer's channel open while the node shuts down. A
/// connection upgraded to a stream runs on outside it, holding no sender, and ends when the stream
/// does. A connection over a cap or over its client's connection rate is closed unread. Each other
/// one gets its own task, so a slow client delays nobody else: it must finish its TLS handshake
/// within `api.handshake_timeout`, send each request's headers within `api.read_timeout`, which
/// also closes a connection idle that long between requests, and take what it is sent within
/// `api.send_timeout`. Handlers see the socket peer as `ConnectInfo<SocketAddr>`.
/// Error cases:
/// - none; it runs until the node stops
pub async fn serve(
    listener: TcpListener,
    tls: Arc<ServerConfig>,
    app: Router,
    limits: Arc<ApiLimits>,
    api: ApiSection,
) -> anyhow::Result<()> {
    let acceptor = TlsAcceptor::from(tls);
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
        let (acceptor, http, app) = (acceptor.clone(), http.clone(), app.clone());
        connections.spawn(async move {
            let stream = match timeout(api.handshake_timeout, acceptor.accept(tcp)).await {
                Ok(Ok(stream)) => stream,
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
