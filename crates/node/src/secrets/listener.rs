//! The enclave's side of a delivery, each connection with its own one-time key. Runs through the
//! node's whole life: alone while it holds for its required secrets, and from then on beside whatever
//! else the node is doing, however long the vault takes to name its key. A few connections run at
//! once, so an idle peer cannot make the operator wait; the sealed store serializes what they seal.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, anyhow, ensure};
use engine_core::types::AccountId;
use hpke::{Kem, Serializable};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use tracing::{Instrument, info, info_span, warn};

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
use crate::trusted_time::TrustedClock;

/// How long the accept loop waits after a failed accept, such as a full file table.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);
/// How many connections run at once. It bounds the quotes and vendor fetches strangers can cause.
const MAX_CONNECTIONS: usize = 8;
/// How long a peer gets to say hello: the tool does so at once, and nothing is quoted before it.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Who may deliver now.
#[derive(Clone)]
pub(crate) enum Authority {
    /// The key the attested config names, before any log can be authenticated.
    Configured(AccountId),
    /// The key the log records, or a rotation's that has come due by trusted time, as
    /// `POST /v1/admin` checks it, with the configured key until the log records one. On a node a
    /// pending rotation names as the successor, its own configured key alone. Weak, so a listener
    /// that outlives the node's other tasks does not hold the sequencer open past its drain.
    Log { tx: mpsc::WeakSender<Request>, configured: AccountId, trusted: Arc<TrustedClock> },
}

/// The delivery listener.
pub(crate) struct Listener {
    listener: TcpListener,
    authority: watch::Receiver<Authority>,
    deliveries: Arc<Deliveries>,
}

/// What a connection delivers into.
struct Deliveries {
    domain: CheckpointDomain,
    provider: TeeProvider,
    processor: Option<String>,
    kds_base: String,
    platform: Platform,
    attestor: Box<dyn Attestor>,
    store: Arc<SecretStore>,
    named: Vec<NamedSecret>,
    /// The bundle this start runs with, once it has started: a later one needs a restart.
    running: OnceLock<Option<u64>>,
    connections: AtomicU64,
}

impl Authority {
    /// Whether `signer` may deliver now. An untrusted clock leaves a pending rotation not yet due:
    /// the host's clock must not bring a key in early, and the recorded key still delivers.
    /// Error cases:
    /// - the sequencer has stopped
    pub(crate) async fn admits(&self, signer: AccountId) -> anyhow::Result<bool> {
        match self {
            Authority::Configured(configured) => Ok(signer == *configured),
            Authority::Log { tx, configured, trusted } => {
                let tx = tx.upgrade().ok_or_else(|| anyhow!("the node is stopping"))?;
                let (now, configured) = (trusted.now_ms(), *configured);
                let (reply, answered) = oneshot::channel();
                tx.send(Request::Query(Box::new(move |engine| {
                    let recorded = engine.operator().map_or(configured, |operator| operator.address);
                    // A pending rotation to this node's own key names this node's operator the
                    // successor, as recovery reads it: during a promotion's notice the outgoing key
                    // still signs admin commands, but it reaches into no successor's enclave.
                    let successor = engine.operator_bounds().rotation.is_some_and(|r| r.address == configured);
                    let due = now.and_then(|now| engine.due_rotation(now)).is_some_and(|r| r.address == signer);
                    let _ = reply.send(if successor { signer == configured } else { signer == recorded || due });
                })))
                .await
                .map_err(|_| anyhow!("the node is stopping"))?;
                answered.await.map_err(|_| anyhow!("the node is stopping"))
            }
        }
    }
}

impl Listener {
    /// The listener `[tee.sealing.secrets]` asks for, bound, taking deliveries from whichever key
    /// `authority` holds when each connection arrives.
    /// Error cases:
    /// - `custody.vault` does not parse, or the listener cannot be bound
    pub(crate) async fn bind(
        cfg: &NodeConfig,
        listen: std::net::SocketAddr,
        platform: Platform,
        attestor: Box<dyn Attestor>,
        store: Arc<SecretStore>,
        named: Vec<NamedSecret>,
        authority: watch::Receiver<Authority>,
    ) -> anyhow::Result<Self> {
        let listener =
            TcpListener::bind(listen).await.with_context(|| format!("binding the delivery listener {listen}"))?;
        info!(listen = %listener.local_addr()?, "secrets delivery listener up");
        let deliveries = Deliveries {
            domain: CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? },
            provider: cfg.tee.provider,
            processor: cfg.tee.processor.clone(),
            kds_base: cfg.tee.kds_base.clone(),
            platform,
            attestor,
            store,
            named,
            running: OnceLock::new(),
            connections: AtomicU64::new(0),
        };
        Ok(Self { listener, authority, deliveries: Arc::new(deliveries) })
    }

    /// The address the listener is bound to.
    #[cfg(test)]
    pub(crate) fn local_addr(&self) -> std::net::SocketAddr {
        self.listener.local_addr().unwrap()
    }

    /// Marks the start complete, running with `version`, so a later bundle says it needs a restart.
    pub(crate) fn started(&self, version: Option<u64>) {
        let _ = self.deliveries.running.set(version);
    }

    /// Takes one more connection onto `serving`, or reaps one that ended. Cancel-safe, so a hold can
    /// race it against its retries.
    pub(crate) async fn step(&self, serving: &mut JoinSet<()>) {
        tokio::select! {
            accepted = self.listener.accept(), if serving.len() < MAX_CONNECTIONS => match accepted {
                Ok((stream, peer)) => {
                    serving.spawn(self.deliveries.clone().serve_connection(stream, peer, self.authority.clone()));
                }
                Err(e) => {
                    warn!("accepting a secrets delivery failed: {e}");
                    sleep(ACCEPT_RETRY).await;
                }
            },
            Some(_) = serving.join_next() => {}
        }
    }

    /// Serves deliveries until the node stops.
    /// Error cases:
    /// - none; a failed connection ends only itself
    pub(crate) async fn run(self) -> anyhow::Result<()> {
        let mut serving = JoinSet::new();
        loop {
            self.step(&mut serving).await;
        }
    }
}

impl Deliveries {
    /// One connection, in its own span. Whatever it ends in, the listener goes on.
    async fn serve_connection(
        self: Arc<Self>,
        mut stream: TcpStream,
        peer: std::net::SocketAddr,
        authority: watch::Receiver<Authority>,
    ) {
        let id = self.connections.fetch_add(1, Ordering::Relaxed) + 1;
        let span = info_span!("secrets_delivery", id, peer = %peer);
        if let Err(e) = self.serve(&mut stream, &authority).instrument(span).await {
            warn!("a secrets delivery connection ended: {e:#}");
        }
    }

    /// What this enclave reports it holds.
    fn status(&self) -> Status {
        let held = self.store.current();
        let missing = match &held {
            Some(signed) => signed.bundle.missing(&self.named),
            None => bundle::missing_from(&Default::default(), &self.named),
        };
        Status {
            sealed_version: held.map(|signed| signed.bundle.version),
            running_version: self.running.get().copied().flatten(),
            missing,
        }
    }

    /// One tool's hello, this enclave's offer, and, if it goes on, its delivery and the answer.
    /// Error cases:
    /// - the hello is unreadable or late, the quote fails, or the peer stops reading
    async fn serve(
        &self,
        stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
        authority: &watch::Receiver<Authority>,
    ) -> anyhow::Result<()> {
        let hello: Hello = timeout(HELLO_TIMEOUT, receive(stream)).await.context("no hello in time")??;
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
        authority: &watch::Receiver<Authority>,
    ) -> anyhow::Result<Receipt> {
        let signed: SignedBundle = delivery::decrypt(self.domain, &self.store.release(), secret, nonce, delivery)?;
        let signer = signed.signer(self.domain)?;
        // Read now, not at the hello: a connection can outlive the switch to the log's key.
        let authority = authority.borrow().clone();
        let admitted =
            timeout(MESSAGE_TIMEOUT, authority.admits(signer)).await.context("the sequencer did not answer")??;
        ensure!(admitted, "the bundle is signed by {signer}, not the operator key in effect");
        ensure!(
            self.provider == TeeProvider::Mock || !is_published_operator(signer)?,
            "the bundle is signed by a published test key"
        );
        signed.bundle.check(&self.named)?;
        let (version, missing) = (signed.bundle.version, signed.bundle.missing(&self.named));
        let was_unreadable = self.store.unreadable().is_some();
        let replaced = self.store.replace(signed)?;
        if was_unreadable {
            info!(alert_resolved = "secrets_unreadable", "a delivery replaced the sealed file that did not open");
        }
        let restart_required = self.running.get().is_some_and(|running| *running != Some(version));
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
