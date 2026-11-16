//! A following member's side of the boundary's log check: the checkpoints its replica verified, and
//! the answers it gives on its registered handover endpoint while it follows, which the handover
//! listener takes over while it holds a turn. It attests a statement only when the question is signed
//! by the enclave that signed the checkpoint, and its replica verified that checkpoint at the
//! statement's seq with the same link and root, in the turn assignment taken there. It quotes once
//! per question, answering a replay with the evidence it already produced, and at most
//! `QUOTES_PER_MINUTE` times a minute. It serves a few connections at once, each with a short deadline
//! for its question, so an idle connection holds up no other, and a full table makes room for a new
//! connection by dropping the oldest that has not asked.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context;
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::config::{NodeConfig, TeeProvider};
use crate::handover::Evidence;
use crate::handover::evidence::{self, CollateralCache, Quoting};
use crate::handover::{receive, send};
use crate::liveness::link::slots::{PeerSlot, Silent, SilentPlace, peer_key};
use crate::log::CheckpointDomain;
use crate::log_check::{Answer, Ask, Statement, VerifiedHead, ask_digest, report_data};
use crate::tee::Attestor;
use crate::tee::sealing::Platform;

/// How many of the latest checkpoints a replica keeps for its answers: an operator asks about one it
/// wrote minutes before.
const RETAINED: usize = 1024;
/// How long a member waits before binding its endpoint again once the bind failed.
const REBIND: Duration = if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(5) };
/// How many connections a member serves at once; one past them is closed unanswered, and the operator
/// asks again.
const CONNECTIONS: usize = 8;
/// How many of them one address (an IPv6 /64) may hold, so no single peer keeps the operator's
/// question out.
const PER_PEER: usize = 2;
/// How long a connection waits for the place of the silent one it told to leave.
const EVICTION_WAIT: Duration = Duration::from_millis(500);
/// How many questions' evidence a member keeps to answer a replay with.
const REPLAYS: usize = 64;
/// How many quotes a member produces in a minute, whoever asks.
const QUOTES_PER_MINUTE: usize = 30;
/// How long collateral fetched for a chip and TCB is reused.
const COLLATERAL_REUSE: Duration = Duration::from_secs(600);
/// How long a connection has to send its question, which the operator sends as soon as it connects.
const QUESTION_TIMEOUT: Duration = if cfg!(test) { Duration::from_millis(500) } else { Duration::from_secs(5) };

/// A checkpoint a replica verified: its head, the turn assignment taken when it applied, and the
/// enclave whose signature on it the replica checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Checked {
    pub head: VerifiedHead,
    pub taken: Option<(u64, u16)>,
    pub signer: AccountId,
}

/// The latest checkpoints a member's replica verified, oldest first, shared between the replica that
/// records them and the answers that read them, and kept across the replica's rebuilds.
#[derive(Clone, Debug, Default)]
pub(crate) struct CheckedHeads(Arc<Mutex<VecDeque<Checked>>>);

impl CheckedHeads {
    /// Records a checkpoint the replica verified, dropping one at the same seq or later, which a
    /// rebuilt replica replays again.
    pub(crate) fn record(&self, checked: Checked) {
        let mut heads = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        while heads.back().is_some_and(|last| last.head.seq >= checked.head.seq) {
            heads.pop_back();
        }
        heads.push_back(checked);
        if heads.len() > RETAINED {
            heads.pop_front();
        }
    }

    /// The checkpoint verified at `seq`, if one is retained.
    pub(crate) fn at(&self, seq: u64) -> Option<Checked> {
        let heads = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        heads.iter().rev().find(|checked| checked.head.seq == seq).copied()
    }

    /// The seq of the latest checkpoint verified.
    fn latest(&self) -> Option<u64> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).back().map(|checked| checked.head.seq)
    }
}

/// A member's answers on its handover endpoint.
pub(crate) struct Answering {
    listen: SocketAddr,
    domain: CheckpointDomain,
    provider: TeeProvider,
    processor: Option<String>,
    kds_base: String,
    attestor: Box<dyn Attestor>,
    platform: Platform,
    config_sha256: [u8; 32],
    checked: CheckedHeads,
    quoted: AsyncMutex<Quoted>,
    collateral: CollateralCache,
    connections: usize,
    per_peer: usize,
}

/// The evidence a member produced for each recent challenge, and when it last quoted.
#[derive(Default)]
struct Quoted {
    by_nonce: BTreeMap<[u8; 32], (B256, Evidence)>,
    order: VecDeque<[u8; 32]>,
    recent: VecDeque<Instant>,
}

impl Quoted {
    /// Counts a quote at `now`.
    /// Error cases:
    /// - `QUOTES_PER_MINUTE` were produced in the last minute
    fn admit(&mut self, now: Instant) -> anyhow::Result<()> {
        while self.recent.front().is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60)) {
            self.recent.pop_front();
        }
        anyhow::ensure!(
            self.recent.len() < QUOTES_PER_MINUTE,
            "this member has quoted {QUOTES_PER_MINUTE} times this minute"
        );
        self.recent.push_back(now);
        Ok(())
    }

    fn remember(&mut self, nonce: [u8; 32], digest: B256, evidence: Evidence) {
        self.by_nonce.insert(nonce, (digest, evidence));
        self.order.push_back(nonce);
        if self.order.len() > REPLAYS
            && let Some(oldest) = self.order.pop_front()
        {
            self.by_nonce.remove(&oldest);
        }
    }
}

impl Answering {
    /// The answers `cfg`'s member gives from `checked`, quoting with `attestor` on `platform`; `None`
    /// without `[tee.sealing.handover]`, where no operator asks.
    /// Error cases:
    /// - the vault address does not parse
    pub(crate) fn new(
        cfg: &NodeConfig,
        attestor: Box<dyn Attestor>,
        platform: Platform,
        checked: CheckedHeads,
    ) -> anyhow::Result<Option<Self>> {
        let Some(handover) = cfg.tee.sealing.as_ref().and_then(|sealing| sealing.handover.as_ref()) else {
            return Ok(None);
        };
        let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
        Ok(Some(Self {
            listen: handover.listen,
            domain,
            provider: cfg.tee.provider,
            processor: cfg.tee.processor.clone(),
            kds_base: cfg.tee.kds_base.clone(),
            attestor,
            platform,
            config_sha256: cfg.file_sha256,
            checked,
            quoted: AsyncMutex::default(),
            collateral: CollateralCache::new(COLLATERAL_REUSE),
            connections: CONNECTIONS,
            per_peer: PER_PEER,
        }))
    }

    /// Answers each operator that connects, up to `CONNECTIONS` at once and `PER_PEER` from one
    /// address, for as long as it runs: a member runs it beside its following, and drops it, freeing
    /// the endpoint and every connection, before it takes a turn. A bind that fails is tried again.
    pub(crate) async fn run(&self) -> Infallible {
        let mut failed = false;
        let listener = loop {
            match TcpListener::bind(self.listen).await {
                Ok(listener) => break listener,
                Err(e) if failed => debug!(listen = %self.listen, "binding the handover endpoint failed again: {e}"),
                Err(e) => {
                    warn!(listen = %self.listen, "binding the handover endpoint for log checks failed, retrying: {e}");
                    failed = true;
                }
            }
            tokio::time::sleep(REBIND).await;
        };
        info!(listen = %self.listen, "answering log checks while following");
        let (mut connection, mut serving) = (0_u64, FuturesUnordered::new());
        let (peers, silent) = (Arc::default(), Silent::default());
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(accepted) => accepted,
                        Err(e) => {
                            warn!("accepting a log check connection failed: {e}");
                            continue;
                        }
                    };
                    let Some(slot) = PeerSlot::take(&peers, peer_key(peer.ip()), self.per_peer) else {
                        debug!(%peer, "closed a log check connection: its address holds {} already", self.per_peer);
                        continue;
                    };
                    // A full table: the oldest connection that has not asked yet gives its place up.
                    if serving.len() >= self.connections
                        && !(silent.evict_oldest() && timeout(EVICTION_WAIT, serving.next()).await.is_ok())
                    {
                        debug!(%peer, "closed a log check connection: {} are being served", serving.len());
                        continue;
                    }
                    connection += 1;
                    let span = info_span!("log_check", id = connection, peer = %peer);
                    let place = silent.enter(connection);
                    serving.push(async move {
                        let _held = slot;
                        if let Err(e) = self.serve(stream, place).await {
                            warn!("a log check connection failed: {e:#}");
                        }
                    }.instrument(span));
                }
                Some(()) = serving.next(), if !serving.is_empty() => {}
            }
        }
    }

    /// Reads one operator's question and sends its answer.
    /// Error cases:
    /// - the question does not arrive in time or does not parse, a newer connection takes the place of
    ///   this silent one, or the answer cannot be sent
    async fn serve(&self, mut stream: TcpStream, place: SilentPlace) -> anyhow::Result<()> {
        let ask: Ask = tokio::select! {
            ask = timeout(QUESTION_TIMEOUT, receive(&mut stream)) => ask.context("no question in time")??,
            () = place.evicted() => anyhow::bail!("a newer connection took this silent one's place"),
        };
        drop(place);
        self.respond(&mut stream, &ask).await
    }

    /// Sends this member's answer to `ask`, read from `stream`: here, or by the member link, which
    /// answers on the same endpoint when the liveness rule runs (`liveness::link`).
    /// Error cases:
    /// - the answer cannot be sent
    pub(crate) async fn respond(&self, stream: &mut TcpStream, ask: &Ask) -> anyhow::Result<()> {
        let answer = self.answer(ask).await;
        match &answer {
            Answer::Verified { .. } => {
                info!(seq = ask.statement.head.seq, turn = ask.statement.turn, "verified the log for the operator")
            }
            Answer::Refused { reason } => warn!(seq = ask.statement.head.seq, "refused a log check: {reason}"),
        }
        send(stream, &answer).await
    }

    /// This member's answer to `ask`: its quote over the statement once the question is signed by the
    /// checkpoint's signer and its replica verified that checkpoint in that turn assignment, at that
    /// turn's boundary; otherwise why not.
    pub(crate) async fn answer(&self, ask: &Ask) -> Answer {
        match self.attest(ask).await {
            Ok(evidence) => Answer::Verified { config_sha256: hex::encode(self.config_sha256), evidence },
            Err(e) => Answer::Refused { reason: format!("{e:#}") },
        }
    }

    /// The quote over `ask`'s statement, produced only once the question is authenticated, and once
    /// per challenge: a replay of the same question gets the evidence already produced.
    /// Error cases:
    /// - the challenge is not 32 bytes of hex, the replica has not verified that checkpoint or holds
    ///   another, the boundary is not the turn's, the question is not signed by the checkpoint's
    ///   signer, the challenge was answered for another question, `QUOTES_PER_MINUTE` were produced in
    ///   the last minute, or the quote cannot be produced
    async fn attest(&self, ask: &Ask) -> anyhow::Result<Evidence> {
        let nonce: [u8; 32] = hex::decode(&ask.nonce)
            .ok()
            .and_then(|nonce| nonce.try_into().ok())
            .context("the challenge is not 32 bytes")?;
        let checked = self.verifies(&ask.statement)?;
        let signature: [u8; 65] = hex::decode(ask.signature.trim_start_matches("0x"))
            .ok()
            .and_then(|signature| signature.try_into().ok())
            .context("the question's signature is not 65 bytes of hex")?;
        let digest = ask_digest(self.domain, &nonce, &ask.statement);
        let signer = eip712::recover(&digest, &Signature::from(signature));
        anyhow::ensure!(
            signer == Some(checked.signer),
            "the question is not signed by {}, which signed the checkpoint at seq {}",
            checked.signer,
            ask.statement.head.seq
        );
        // Held across the quote, so replays sent in parallel wait for it and find its evidence.
        let mut quoted = self.quoted.lock().await;
        if let Some((asked, evidence)) = quoted.by_nonce.get(&nonce) {
            anyhow::ensure!(*asked == digest, "this challenge was answered for another question");
            return Ok(evidence.clone());
        }
        quoted.admit(Instant::now())?;
        let quoting =
            Quoting { provider: self.provider, processor: self.processor.as_deref(), kds_base: &self.kds_base };
        let bound = report_data(self.domain, &nonce, &ask.statement, &self.config_sha256);
        let evidence =
            evidence::produce_with(&quoting, self.attestor.as_ref(), &self.platform, bound, Some(&self.collateral))
                .await?;
        quoted.remember(nonce, digest, evidence.clone());
        Ok(evidence)
    }

    /// The checkpoint this member's replica verified for `statement`, in its turn assignment.
    /// Error cases:
    /// - the boundary is not the turn's, the replica has not reached the seq or no longer retains
    ///   it, or holds another link, root or taken assignment there
    fn verifies(&self, statement: &Statement) -> anyhow::Result<Checked> {
        let head = statement.head;
        anyhow::ensure!(
            engine_core::turns::boundary(statement.turn) == Some(statement.boundary),
            "turn {}'s boundary is not {}",
            statement.turn,
            statement.boundary
        );
        let checked = self.checked.at(head.seq).with_context(|| match self.checked.latest() {
            Some(latest) if latest < head.seq => {
                format!("this replica has verified checkpoints through seq {latest}, not yet seq {}", head.seq)
            }
            _ => format!("this replica retains no checkpoint at seq {}", head.seq),
        })?;
        anyhow::ensure!(
            checked.head == head,
            "this replica verified another log at seq {}: link {} root {}",
            head.seq,
            checked.head.link,
            checked.head.root
        );
        anyhow::ensure!(
            checked.taken == Some((statement.turn, statement.position)),
            "this replica's log had taken {:?} at seq {}, not turn {} at position {}",
            checked.taken,
            head.seq,
            statement.turn,
            statement.position
        );
        Ok(checked)
    }
}

#[cfg(test)]
pub(crate) mod tests;
