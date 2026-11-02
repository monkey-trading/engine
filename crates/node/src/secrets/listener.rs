//! The enclave's side of a delivery: one connection at a time, each with its own one-time key.
//! Runs through the node's whole life: alone while it holds for its required secrets, and as a
//! supervised task once it serves.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, ensure};
use engine_core::types::AccountId;
use hpke::{Kem, Serializable};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep, timeout};
use tracing::{Instrument, info, info_span, warn};

use crate::clock::Clock;
use crate::config::{NamedSecret, NodeConfig, TeeProvider, is_published_operator};
use crate::handover::evidence::{self, Quoting};
use crate::handover::{Kx, MESSAGE_TIMEOUT, receive, send};
use crate::log::CheckpointDomain;
use crate::secrets::delivery::{
    self, Delivery, Hello, Offer, Receipt, Reply, Status, offer_report_data, receipt_report_data,
};
use crate::secrets::{SignedBundle, bundle};
use crate::sequencer::Request;
use crate::tee::Attestor;
use crate::tee::sealing::Platform;
use crate::tee::secrets::{Replaced, SecretStore};

/// How long the accept loop waits after a failed accept, such as a full file table.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);

/// Who may deliver now.
#[derive(Clone)]
pub(crate) enum Authority {
    /// The key the attested config names, before any log can be authenticated.
    Configured(AccountId),
    /// The key the log records, or a rotation's that has come due by the node's clock, as
    /// `POST /v1/admin` checks it, with the configured key until the log records one. On a node a
    /// pending rotation names as the successor, its own configured key alone.
    Log { tx: mpsc::Sender<Request>, configured: AccountId, clock: Arc<dyn Clock> },
}

/// The delivery listener and what it delivers into.
pub(crate) struct Listener {
    listener: TcpListener,
    domain: CheckpointDomain,
    provider: TeeProvider,
    processor: Option<String>,
    kds_base: String,
    platform: Platform,
    attestor: Box<dyn Attestor>,
    store: Arc<SecretStore>,
    named: Vec<NamedSecret>,
    /// The bundle this start runs with, once it has started: a later one needs a restart.
    running: Option<Option<u64>>,
    connections: u64,
}

impl Authority {
    /// Whether `signer` may deliver now.
    /// Error cases:
    /// - the sequencer has stopped
    pub(crate) async fn admits(&self, signer: AccountId) -> anyhow::Result<bool> {
        match self {
            Authority::Configured(configured) => Ok(signer == *configured),
            Authority::Log { tx, configured, clock } => {
                let (now, configured) = (clock.now_ms(), *configured);
                let (reply, answered) = oneshot::channel();
                tx.send(Request::Query(Box::new(move |engine| {
                    let recorded = engine.operator().map_or(configured, |operator| operator.address);
                    // A pending rotation to this node's own key names this node's operator the
                    // successor, as recovery reads it: during a promotion's notice the outgoing key
                    // still signs admin commands, but it reaches into no successor's enclave.
                    let successor = engine.operator_bounds().rotation.is_some_and(|r| r.address == configured);
                    let admitted = if successor {
                        signer == configured
                    } else {
                        signer == recorded || engine.due_rotation(now).is_some_and(|r| r.address == signer)
                    };
                    let _ = reply.send(admitted);
                })))
                .await
                .map_err(|_| anyhow!("the node is stopping"))?;
                answered.await.map_err(|_| anyhow!("the node is stopping"))
            }
        }
    }
}

impl Listener {
    /// The listener `[tee.sealing.secrets]` asks for, bound.
    /// Error cases:
    /// - `custody.vault` does not parse, or the listener cannot be bound
    pub(crate) async fn bind(
        cfg: &NodeConfig,
        listen: std::net::SocketAddr,
        platform: Platform,
        attestor: Box<dyn Attestor>,
        store: Arc<SecretStore>,
        named: Vec<NamedSecret>,
    ) -> anyhow::Result<Self> {
        let listener =
            TcpListener::bind(listen).await.with_context(|| format!("binding the delivery listener {listen}"))?;
        info!(listen = %listener.local_addr()?, "secrets delivery listener up");
        Ok(Self {
            listener,
            domain: CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? },
            provider: cfg.tee.provider,
            processor: cfg.tee.processor.clone(),
            kds_base: cfg.tee.kds_base.clone(),
            platform,
            attestor,
            store,
            named,
            running: None,
            connections: 0,
        })
    }

    /// The address the listener is bound to.
    #[cfg(test)]
    pub(crate) fn local_addr(&self) -> std::net::SocketAddr {
        self.listener.local_addr().unwrap()
    }

    /// Marks the start complete, running with `version`, so a later bundle says it needs a restart.
    pub(crate) fn started(&mut self, version: Option<u64>) {
        self.running = Some(version);
    }

    /// Accepts one connection, whichever comes first being cancel-safe in a `select!`.
    /// Error cases:
    /// - the accept fails
    pub(crate) async fn accept(&self) -> std::io::Result<(TcpStream, std::net::SocketAddr)> {
        self.listener.accept().await
    }

    /// Serves deliveries until the node stops, under whichever authority `authority` holds when each
    /// arrives: a node can wait for days between its hold and its sequencer, for a key the vault does
    /// not yet name, and it still takes the operator's deliveries.
    /// Error cases:
    /// - none; a failed connection ends only itself
    pub(crate) async fn run(mut self, authority: watch::Receiver<Authority>) -> anyhow::Result<()> {
        loop {
            match self.listener.accept().await {
                Ok((stream, peer)) => {
                    let authority = authority.borrow().clone();
                    self.serve_connection(stream, peer, &authority).await;
                }
                Err(e) => {
                    warn!("accepting a secrets delivery failed: {e}");
                    sleep(ACCEPT_RETRY).await;
                }
            }
        }
    }

    /// One connection, in its own span. Whatever it ends in, the listener goes on.
    pub(crate) async fn serve_connection(
        &mut self,
        mut stream: TcpStream,
        peer: std::net::SocketAddr,
        authority: &Authority,
    ) {
        self.connections += 1;
        let span = info_span!("secrets_delivery", id = self.connections, peer = %peer);
        if let Err(e) = self.serve(&mut stream, authority).instrument(span).await {
            warn!("a secrets delivery connection ended: {e:#}");
        }
    }

    /// What this enclave reports it holds.
    pub(crate) fn status(&self) -> Status {
        let held = self.store.current();
        let missing = match &held {
            Some(signed) => signed.bundle.missing(&self.named),
            None => bundle::missing_from(&Default::default(), &self.named),
        };
        Status {
            sealed_version: held.map(|signed| signed.bundle.version),
            running_version: self.running.flatten(),
            missing,
        }
    }

    /// One tool's hello, this enclave's offer, and, if it goes on, its delivery and the answer.
    /// Error cases:
    /// - the hello is unreadable or late, the quote fails, or the peer stops reading
    async fn serve(
        &mut self,
        stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
        authority: &Authority,
    ) -> anyhow::Result<()> {
        let hello: Hello = timeout(MESSAGE_TIMEOUT, receive(stream)).await.context("no hello in time")??;
        let nonce: [u8; 32] = hex::decode(&hello.nonce)
            .ok()
            .and_then(|nonce| nonce.try_into().ok())
            .context("the hello's nonce is not 32 bytes of hex")?;
        let (secret, public) = Kx::gen_keypair();
        let public_key = public.to_bytes().to_vec();
        let status = self.status();
        let config_sha256 = self.store.release().config_sha256;
        let report_data = offer_report_data(self.domain, &nonce, &public_key, &config_sha256, &status);
        let evidence = self.quote(report_data).await?;
        let offer =
            Offer { public_key: hex::encode(&public_key), config_sha256: hex::encode(config_sha256), status, evidence };
        send(stream, &offer).await?;
        // A status read ends here, with the peer closing the connection.
        let delivery: Delivery = match timeout(MESSAGE_TIMEOUT, receive(stream)).await {
            Ok(Ok(delivery)) => delivery,
            Ok(Err(e)) => {
                info!("the peer sent no delivery: {e:#}");
                return Ok(());
            }
            Err(_) => {
                info!("the peer sent no delivery in time");
                return Ok(());
            }
        };
        match self.seal_delivery(&secret, &nonce, &delivery, authority).await {
            Ok(receipt) => {
                let bound = receipt_report_data(self.domain, &nonce, &delivery, &receipt);
                let evidence = self.quote(bound).await?;
                send(stream, &Reply::Sealed { receipt, evidence }).await
            }
            Err(e) => {
                warn!("refusing the delivery: {e:#}");
                send(stream, &Reply::Refused { reason: format!("{e:#}") }).await
            }
        }
    }

    /// Opens a delivery, checks who signed it and what it holds, and seals it.
    /// Error cases:
    /// - it does not decrypt on this connection for this release
    /// - its signer is not the operator key in effect, or is a published test key on a real enclave
    /// - its contents fail `Bundle::check`, or it is older than what is sealed (see
    ///   `SecretStore::replace`), or it cannot be sealed
    async fn seal_delivery(
        &self,
        secret: &<Kx as Kem>::PrivateKey,
        nonce: &[u8; 32],
        delivery: &Delivery,
        authority: &Authority,
    ) -> anyhow::Result<Receipt> {
        let signed: SignedBundle = delivery::decrypt(self.domain, &self.store.release(), secret, nonce, delivery)?;
        let signer = signed.signer(self.domain)?;
        ensure!(authority.admits(signer).await?, "the bundle is signed by {signer}, not the operator key in effect");
        ensure!(
            self.provider == TeeProvider::Mock || !is_published_operator(signer)?,
            "the bundle is signed by a published test key"
        );
        signed.bundle.check(&self.named)?;
        let (version, missing) = (signed.bundle.version, signed.bundle.missing(&self.named));
        let replaced = self.store.replace(signed)?;
        let restart_required = self.running.is_some_and(|running| running != Some(version));
        info!(version, %signer, ?replaced, restart_required, "sealed a delivered secrets bundle");
        if restart_required {
            warn!(
                alert = "secrets_restart_required",
                "bundle version {version} is sealed, and what runs keeps the secrets it started with until a restart: \
                 the custody quorum, feeds, and webhooks build their clients once"
            );
        }
        Ok(Receipt { version, already_sealed: replaced == Replaced::AlreadySealed, missing, restart_required })
    }

    /// This enclave's evidence over `report_data`.
    /// Error cases:
    /// - the attestor cannot quote, or the hardware collateral cannot be fetched
    async fn quote(&self, report_data: [u8; 64]) -> anyhow::Result<crate::handover::Evidence> {
        let quoting =
            Quoting { provider: self.provider, processor: self.processor.as_deref(), kds_base: &self.kds_base };
        evidence::produce(&quoting, self.attestor.as_ref(), &self.platform, report_data).await
    }
}
