//! The enclave's side of a delivery, each connection with its own one-time key. Runs through the
//! node's whole life: alone while it holds for its required secrets, and from then on beside whatever
//! else the node is doing, however long the vault takes to name its key. Many connections run at
//! once, so an idle peer cannot make the operator wait, and at most `QUOTES` of them quote at a time;
//! the sealed store serializes what they seal. One address (an IPv6 /64) holds at most `PER_PEER` of
//! them, and with every place taken, or every place of the new connection's address, the oldest
//! connection there waiting on its peer, for its hello or its delivery, gives its place to it.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, anyhow, ensure};
use engine_core::Engine;
use engine_core::types::{AccountId, Millis};
use hpke::{Kem, Serializable};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};
use tracing::{Instrument, debug, info, info_span, warn};

use crate::checkpoints::RegistryOperator;
use crate::config::{NamedSecret, NodeConfig, TeeProvider, is_published_operator};
use crate::handover::evidence::{self, Quoting};
use crate::handover::{Kx, MESSAGE_TIMEOUT, receive, send};
use crate::liveness::link::slots::{PeerSlot, Silent, SilentPlace, peer_key};
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
/// How many connections run at once.
const CONNECTIONS: usize = 64;
/// How many of them quote at once. It bounds the quotes and vendor fetches strangers can cause.
const QUOTES: usize = 8;
/// How many of them one peer address may hold.
const PER_PEER: usize = 2;
/// How long a peer gets to say hello: the tool does so at once, and nothing is quoted before it.
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a new connection waits for the place of the waiting one it told to leave.
const EVICTION_WAIT: Duration = Duration::from_millis(500);

/// Who may deliver now.
#[derive(Clone)]
pub(crate) enum Authority {
    /// The key the attested config names, before any log can be authenticated.
    Configured(AccountId),
    /// The key the log records, or a rotation's that has come due by trusted time, as
    /// `POST /v1/admin` checks it, with the configured key until the log records one. While the
    /// registry elects operators, or on a node a pending rotation names as the successor, or one
    /// whose configured key the finalised registry `installed` (`RegistryOperator::unscanned`), its
    /// own configured key alone. Weak, so a listener that outlives the node's other tasks does not
    /// hold the sequencer open past its drain.
    Log {
        tx: mpsc::WeakSender<Request>,
        configured: AccountId,
        installed: Option<RegistryOperator>,
        trusted: Arc<TrustedClock>,
    },
}

/// The delivery listener.
pub(crate) struct Listener {
    listener: TcpListener,
    authority: watch::Receiver<Authority>,
    deliveries: Arc<Deliveries>,
    /// How many connections each peer address holds, and how many it may.
    peers: Arc<Mutex<BTreeMap<IpAddr, usize>>>,
    per_peer: usize,
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
    /// The newest version whose connection has finished answering, receipt sent or not.
    answered: watch::Sender<Option<u64>>,
    connections: AtomicU64,
    /// The places connections run in, and those waiting on their peer, which give theirs up to a new
    /// connection once every place is taken, each under the number it waits as.
    places: Arc<Semaphore>,
    waiting: Silent,
    waits: AtomicU64,
    quotes: Semaphore,
}

impl Authority {
    /// Whether `signer` may deliver now. An untrusted clock leaves a pending rotation not yet due:
    /// the host's clock must not bring a key in early, and the recorded key still delivers.
    /// Error cases:
    /// - the sequencer has stopped
    pub(crate) async fn admits(&self, signer: AccountId) -> anyhow::Result<bool> {
        match self {
            Authority::Configured(configured) => Ok(signer == *configured),
            Authority::Log { tx, configured, installed, trusted } => {
                let tx = tx.upgrade().ok_or_else(|| anyhow!("the node is stopping"))?;
                let (now, configured, installed) = (trusted.now_ms(), *configured, *installed);
                let (reply, answered) = oneshot::channel();
                tx.send(Request::Query(Box::new(move |engine| {
                    let _ = reply.send(log_admits(engine, signer, configured, installed, now));
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
            answered: watch::Sender::new(None),
            connections: AtomicU64::new(0),
            places: Arc::new(Semaphore::new(CONNECTIONS)),
            waiting: Silent::default(),
            waits: AtomicU64::new(0),
            quotes: Semaphore::new(QUOTES),
        };
        let deliveries = Arc::new(deliveries);
        Ok(Self { listener, authority, deliveries, peers: Arc::default(), per_peer: PER_PEER })
    }

    /// The newest version whose connection has finished answering: a node about to exit waits on it,
    /// or the operator has no receipt and no node left to retry on.
    pub(crate) fn answered(&self) -> watch::Receiver<Option<u64>> {
        self.deliveries.answered.subscribe()
    }

    /// The address the listener is bound to.
    #[cfg(test)]
    pub(crate) fn local_addr(&self) -> std::net::SocketAddr {
        self.listener.local_addr().unwrap()
    }

    /// The listener, letting one address hold any number of connections, as peers on as many addresses
    /// would: a test's connections all come from loopback.
    #[cfg(test)]
    pub(crate) fn loopback(mut self) -> Self {
        self.per_peer = usize::MAX;
        self
    }

    /// Marks the start complete, running with `version`, so a later bundle says it needs a restart,
    /// and ends the warning a past start raised, a restart being what it asked for. `SecretStore`
    /// calls this under the lock a delivery seals under, so a delivery's own warning always comes
    /// after this, and this cannot end it.
    pub(crate) fn started(&self, version: Option<u64>) {
        let _ = self.deliveries.running.set(version);
        info!(alert_resolved = "secrets_restart_required", "this start runs the newest sealed secrets");
    }

    /// Takes one more connection onto `serving`, or reaps one that ended. Cancel-safe, so a hold can
    /// race it against its retries.
    pub(crate) async fn step(&self, serving: &mut JoinSet<()>) {
        tokio::select! {
            accepted = self.listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let (deliveries, key) = (self.deliveries.clone(), peer_key(peer.ip()));
                    let Some((slot, evicted)) =
                        PeerSlot::take_or_evict(&self.peers, key, self.per_peer, &deliveries.waiting)
                    else {
                        let holds = self.per_peer;
                        debug!(%peer, "closed a secrets delivery connection: its address's {holds} are busy");
                        return;
                    };
                    // A full listener: the oldest connection waiting on its peer gives its place up, unless
                    // the address's own oldest just did, and the new one waits for that place.
                    let permit = match deliveries.places.clone().try_acquire_owned() {
                        Ok(permit) => Some(permit),
                        Err(_) if deliveries.waiting.make_room(evicted) => None,
                        Err(_) => {
                            debug!(%peer, "closed a secrets delivery connection: {CONNECTIONS} are being served");
                            return;
                        }
                    };
                    // Waiting for its hello from the start, so a newer connection from its address finds it.
                    let place = deliveries.waiting_place(key);
                    let authority = self.authority.clone();
                    let connection = deliveries.serve_connection(stream, (peer, key, place), permit, authority);
                    serving.spawn(async move {
                        let _held = slot;
                        connection.await;
                    });
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
    /// One connection, from `peer` counted under `key` and waiting for its hello in `waiting_hello`, in its
    /// own span, in its place, or, with none (`place`), in the one a connection told to leave as it
    /// arrived gives up. Whatever it ends in, the listener goes on.
    async fn serve_connection(
        self: Arc<Self>,
        mut stream: TcpStream,
        (peer, key, waiting_hello): (std::net::SocketAddr, IpAddr, SilentPlace),
        place: Option<OwnedSemaphorePermit>,
        authority: watch::Receiver<Authority>,
    ) {
        let id = self.connections.fetch_add(1, Ordering::Relaxed) + 1;
        let span = info_span!("secrets_delivery", id, peer = %peer);
        async {
            // Without a place yet, it waits for the one it freed, unless a newer connection tells it to leave.
            let freed = timeout(EVICTION_WAIT, self.places.clone().acquire_owned());
            let permit = match place {
                Some(permit) => Some(permit),
                None => tokio::select! {
                    freed = freed => freed.ok().and_then(Result::ok),
                    () = waiting_hello.evicted() => None,
                },
            };
            let Some(_permit) = permit else {
                debug!("closed a secrets delivery connection: {CONNECTIONS} are being served");
                return;
            };
            if let Err(e) = self.serve(&mut stream, (key, waiting_hello), &authority).await {
                warn!("a secrets delivery connection ended: {e:#}");
            }
        }
        .instrument(span)
        .await;
    }

    /// A place among the connections waiting on their peer, for one counted under `key`.
    fn waiting_place(&self, key: IpAddr) -> SilentPlace {
        self.waiting.enter(self.waits.fetch_add(1, Ordering::Relaxed), key)
    }

    /// `wait`'s outcome, the connection holding `place` among those waiting on their peer meanwhile;
    /// `None` once a new connection takes it, which it may once every place, or its address's share, is
    /// taken.
    async fn waiting_on_peer<T>(place: SilentPlace, wait: impl Future<Output = T>) -> Option<T> {
        let waited = tokio::select! {
            waited = wait => Some(waited),
            () = place.evicted() => None,
        };
        waited.filter(|_| place.leave())
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

    /// One tool's hello, this enclave's offer, and, if it goes on, its delivery and the answer. A peer
    /// that sends no hello or no delivery ends it quietly.
    /// Error cases:
    /// - the hello's nonce is malformed, the quote fails, or the peer stops reading
    async fn serve(
        &self,
        stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
        (key, waiting_hello): (IpAddr, SilentPlace),
        authority: &watch::Receiver<Authority>,
    ) -> anyhow::Result<()> {
        // Any stranger can end a connection before its hello, so those endings are logged quietly.
        let hello: Hello = match Self::waiting_on_peer(waiting_hello, timeout(HELLO_TIMEOUT, receive(stream))).await {
            Some(Ok(Ok(hello))) => hello,
            Some(Ok(Err(e))) => {
                debug!("the peer sent no hello: {e:#}");
                return Ok(());
            }
            Some(Err(_)) => {
                debug!("the peer sent no hello in time");
                return Ok(());
            }
            None => {
                debug!("a new connection took the place of this one, waiting for its hello");
                return Ok(());
            }
        };
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
        let delivery: Delivery =
            match Self::waiting_on_peer(self.waiting_place(key), timeout(MESSAGE_TIMEOUT, receive(stream))).await {
                Some(Ok(Ok(delivery))) => delivery,
                Some(Ok(Err(e))) => {
                    info!("the peer sent no delivery: {e:#}");
                    return Ok(());
                }
                Some(Err(_)) => {
                    info!("the peer sent no delivery in time");
                    return Ok(());
                }
                None => {
                    debug!("a new connection took the place of this one, waiting for its delivery");
                    return Ok(());
                }
            };
        match self.seal_delivery(&secret, &nonce, &delivery, authority).await {
            Ok(receipt) => {
                let version = receipt.version;
                let bound = receipt_report_data(self.domain, &nonce, &delivery, &receipt);
                let answer = async {
                    let evidence = self.quote(bound).await?;
                    send(stream, &Reply::Sealed { receipt, evidence }).await
                };
                let answered = answer.await;
                self.answered.send_modify(|newest| *newest = (*newest).max(Some(version)));
                answered
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
        let restart_required = self.running.get().is_some_and(|running| *running < Some(version));
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
        let _permit = self.quotes.acquire().await.expect("the quote limit is never closed");
        evidence::produce(&quoting, self.attestor.as_ref(), &self.platform, report_data).await
    }
}

/// Whether `signer` may deliver to a node whose config names `configured`, by `engine`'s log at trusted
/// time `now` (`Authority::Log`).
fn log_admits(
    engine: &Engine,
    signer: AccountId,
    configured: AccountId,
    installed: Option<RegistryOperator>,
    now: Option<Millis>,
) -> bool {
    let recorded = engine.operator().map_or(configured, |operator| operator.address);
    // Under elections each operator's secrets are its own, so a key the registry
    // elects reaches into no other operator's enclave. That narrowing holds only
    // while an election that governs the key is in force: one still handing the
    // key over (the pending rotation is its), or one whose key the log records.
    // An election the log moved on from is historical and narrows nothing, and
    // with none recorded the key the log records stays in effect. A pending
    // rotation to this node's own key names this node's operator the successor,
    // as recovery reads it: during a promotion's notice the outgoing key still
    // signs admin commands, but it reaches into no successor's enclave. So do an
    // election still to take effect that names this node's key, waiting or the
    // last behind them, and the registry's installation of this node's key,
    // before the log's scan passes it.
    // From `V88` the elections the last waits behind govern as the last does.
    let election = engine.operator_election();
    let governing = engine.election_governs_key();
    // Under daily turns each member's node takes its own operator's delivery only, for as long as the
    // turns hand the keys between members: drawn turns stand after rotation is turned off, until an
    // installation ends them.
    let turns = engine.rotation_by_election() && !engine.daily_turns().sets.is_empty();
    let own_key_only = (election.rotation_enabled && governing)
        || turns
        || engine.operator_bounds().rotation.is_some_and(|r| r.address == configured)
        || engine.awaited_election_keys().any(|key| key == configured)
        || installed.and_then(|seated| seated.unscanned(engine)) == Some(configured);
    let due = now.and_then(|now| engine.due_key(now)).is_some_and(|r| r.address == signer);
    if own_key_only { signer == configured } else { signer == recorded || due }
}

#[cfg(test)]
mod tests;
