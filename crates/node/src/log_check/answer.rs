//! A following member's side of the boundary's log check: the checkpoints its replica verified, and
//! the answers it gives on its registered handover endpoint while it follows, which the handover
//! listener takes over while it holds a turn. It attests a statement only when its replica verified a
//! checkpoint at the statement's seq with the same link and root, in the turn assignment taken there.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Context;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::config::{NodeConfig, TeeProvider};
use crate::handover::evidence::{self, Quoting};
use crate::handover::{MESSAGE_TIMEOUT, receive, send};
use crate::log::CheckpointDomain;
use crate::log_check::{Answer, Ask, Statement, VerifiedHead, report_data};
use crate::tee::Attestor;
use crate::tee::sealing::Platform;

/// How many of the latest checkpoints a replica keeps for its answers: an operator asks about one it
/// wrote minutes before.
const RETAINED: usize = 1024;
/// How long a member waits before binding its endpoint again once the bind failed.
const REBIND: Duration = if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(5) };

/// A checkpoint a replica verified, and the turn assignment taken when it applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Checked {
    pub head: VerifiedHead,
    pub taken: Option<(u64, u16)>,
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
        }))
    }

    /// Answers each operator that connects, one at a time, for as long as it runs: a member runs it
    /// beside its following, and drops it, freeing the endpoint, before it takes a turn. A bind that
    /// fails is tried again.
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
        let mut connection = 0_u64;
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    warn!("accepting a log check connection failed: {e}");
                    continue;
                }
            };
            connection += 1;
            let span = info_span!("log_check", id = connection, peer = %peer);
            if let Err(e) = self.serve(stream).instrument(span.clone()).await {
                span.in_scope(|| warn!("a log check connection failed: {e:#}"));
            }
        }
    }

    /// Reads one operator's question and sends its answer.
    /// Error cases:
    /// - the question does not arrive in time or does not parse, or the answer cannot be sent
    async fn serve(&self, mut stream: TcpStream) -> anyhow::Result<()> {
        let ask: Ask = timeout(MESSAGE_TIMEOUT, receive(&mut stream)).await.context("no question in time")??;
        let answer = self.answer(&ask).await;
        match &answer {
            Answer::Verified { .. } => {
                info!(seq = ask.statement.head.seq, turn = ask.statement.turn, "verified the log for the operator")
            }
            Answer::Refused { reason } => warn!(seq = ask.statement.head.seq, "refused a log check: {reason}"),
        }
        send(&mut stream, &answer).await
    }

    /// This member's answer to `ask`: its quote over the statement once its replica verified that
    /// checkpoint in that turn assignment, at that turn's boundary; otherwise why not.
    pub(crate) async fn answer(&self, ask: &Ask) -> Answer {
        match self.attest(ask).await {
            Ok(evidence) => Answer::Verified { config_sha256: hex::encode(self.config_sha256), evidence },
            Err(e) => Answer::Refused { reason: format!("{e:#}") },
        }
    }

    /// The quote over `ask`'s statement.
    /// Error cases:
    /// - the challenge is not 32 bytes of hex, the replica has not verified that checkpoint or holds
    ///   another, or the boundary is not the turn's, or the quote cannot be produced
    async fn attest(&self, ask: &Ask) -> anyhow::Result<crate::handover::Evidence> {
        let nonce: [u8; 32] = hex::decode(&ask.nonce)
            .ok()
            .and_then(|nonce| nonce.try_into().ok())
            .context("the challenge is not 32 bytes")?;
        self.verifies(&ask.statement)?;
        let quoting =
            Quoting { provider: self.provider, processor: self.processor.as_deref(), kds_base: &self.kds_base };
        let bound = report_data(self.domain, &nonce, &ask.statement, &self.config_sha256);
        evidence::produce(&quoting, self.attestor.as_ref(), &self.platform, bound).await
    }

    /// Whether this member's replica verified `statement`'s checkpoint in its turn assignment.
    /// Error cases:
    /// - the boundary is not the turn's, the replica has not reached the seq or no longer retains
    ///   it, or holds another link, root or taken assignment there
    fn verifies(&self, statement: &Statement) -> anyhow::Result<()> {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests;
