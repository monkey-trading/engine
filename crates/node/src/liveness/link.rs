//! The member link: what a rotation-set member's node answers on its registered handover endpoint
//! while it follows the operator, and how another member's node asks. Each request is one handover
//! frame (`handover::send`), answered by one:
//! - `Status`: the seq and cumulative prefix its copy durably holds, and the newest head of the asked
//!   signer epoch it has seen, signed by its standby key over the asker's fresh nonce. The operator
//!   asks for it as a member's confirmation, and any node asks for it before it sequences;
//! - `Entries`: the copy's latest entries after a seq, which a promoted member fetches a gap with,
//!   checking every one against its own copy's seal chain;
//! - `Vouch`: a vouch for a candidate's standby key (`standby_key::vouch::answer`), signed with this
//!   member's standby key while the schedule lists it as promotable.
//!
//! The host carries the connection and can drop or delay it, but forges nothing: a status recovers to
//! the member's registered standby key, an entry must extend the asker's seal chain, and a vouch
//! recovers to its voucher.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use alloy::primitives::{Address, B256};
use anyhow::{Context, bail};
use engine_core::eip712::Signature;
use engine_core::types::AccountId;
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::config::TeeProvider;
use crate::handover::{receive, send};
use crate::liveness::Status;
use crate::log::{LogEntry, Offset};
use crate::log_heads::{Head, SignedHead};
use crate::standby_key::StandbyKeys;
use crate::standby_key::chain::{StandbyChain, StandbyDomain};
use crate::standby_key::vouch::{VouchReply, VouchRequest, Voucher, answer};
use crate::tee::sealing::Platform;
use crate::trusted_time::TrustedClock;

/// How many of the copy's latest entries a member keeps to answer `Entries` from.
const RECENT_ENTRIES: usize = 16_384;
/// The most bytes of entries one answer carries, under the frame limit.
const ENTRIES_BYTES: usize = 768 * 1024;
/// How many signer epochs' newest heads a member keeps.
const SEEN_EPOCHS: usize = 4;
/// How long one question gets, connecting and answering.
pub(crate) const ASK_TIMEOUT: Duration = Duration::from_secs(10);
/// How many connections a member answers at once.
const CONNECTIONS: usize = 8;
/// How long a member waits before binding its endpoint again once the bind failed.
const REBIND: Duration = if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(5) };

/// A question to a member.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Request {
    /// Its status under the asker's signer epoch, over the asker's fresh nonce.
    Status { epoch: u64, nonce: B256 },
    /// Its copy's entries after the entry at `after`.
    Entries { after: Offset },
    /// A vouch for a standby key.
    Vouch { request: Box<VouchRequest> },
}

/// A member's answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Reply {
    /// Its status, signed by its standby key.
    Status {
        member: AccountId,
        status: Status,
        signature: Signature,
    },
    /// Its copy's entries after the asked offset, oldest first, as many as fit one answer.
    Entries {
        entries: Vec<(Offset, LogEntry)>,
    },
    Vouch {
        reply: VouchReply,
    },
    /// Why it does not answer.
    Refused {
        reason: String,
    },
}

/// What a member's copy holds, shared between its following, which records it, and its answers.
#[derive(Clone, Debug, Default)]
pub(crate) struct Copy(Arc<Mutex<CopyState>>);

#[derive(Debug, Default)]
struct CopyState {
    /// The copy's durable head: its last command's seq and the cumulative prefix after it.
    durable: Option<Head>,
    /// The newest head seen of each of the latest signer epochs.
    seen: BTreeMap<u64, Head>,
    /// The copy's latest entries, oldest first.
    recent: VecDeque<(Offset, LogEntry)>,
    /// The newest signed head the operator's stream carried, which the liveness caller relays.
    signed: Option<SignedHead>,
}

/// A member's answers on its handover endpoint.
pub(crate) struct Server<C> {
    pub listen: SocketAddr,
    pub deployment: (u64, Address),
    /// The member's candidate, which its statuses name.
    pub member: AccountId,
    pub copy: Copy,
    pub standby: Arc<StandbyKeys>,
    pub provider: TeeProvider,
    pub platform: Platform,
    pub trusted: TrustedClock,
    pub chain: C,
    pub domain: StandbyDomain,
}

/// How a node asks a member.
pub(crate) trait Asking: Send + Sync {
    /// The member at `endpoint`'s answer to `request`.
    /// Error cases:
    /// - the connection, the question or the answer fails or outruns `ASK_TIMEOUT`
    fn ask(&self, endpoint: &str, request: &Request) -> impl Future<Output = anyhow::Result<Reply>> + Send;
}

/// Asks over TCP.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Tcp;

impl Asking for Tcp {
    async fn ask(&self, endpoint: &str, request: &Request) -> anyhow::Result<Reply> {
        timeout(ASK_TIMEOUT, async {
            let mut stream = TcpStream::connect(endpoint).await.with_context(|| format!("connecting to {endpoint}"))?;
            send(&mut stream, request).await?;
            receive(&mut stream).await
        })
        .await
        .with_context(|| format!("asking {endpoint} timed out"))?
    }
}

impl Copy {
    /// Records an entry the copy took.
    pub(crate) fn appended(&self, offset: Offset, entry: &LogEntry) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.recent.push_back((offset, entry.clone()));
        if state.recent.len() > RECENT_ENTRIES {
            state.recent.pop_front();
        }
    }

    /// Records that the copy is durable through `head`.
    pub(crate) fn durable(&self, head: Head) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).durable = Some(head);
    }

    /// Records a head of `epoch` seen on the stream or the chain, keeping the newest of each epoch.
    pub(crate) fn saw(&self, epoch: u64, head: Head) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let newest = state.seen.entry(epoch).or_insert(head);
        if head.seq > newest.seq {
            *newest = head;
        }
        while state.seen.len() > SEEN_EPOCHS {
            state.seen.pop_first();
        }
    }

    /// Records a signed head the operator's stream carried, keeping the newest.
    pub(crate) fn signed_head(&self, head: SignedHead) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let newer = state.signed.as_ref().is_none_or(|held| {
            (head.epoch, head.head.head.seq, head.head.signed_at)
                > (held.epoch, held.head.head.seq, held.head.signed_at)
        });
        if newer {
            state.signed = Some(head);
        }
    }

    /// The newest signed head the operator's stream carried.
    pub(crate) fn signed(&self) -> Option<SignedHead> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).signed.clone()
    }

    /// The status under `epoch`, once the copy is durable through a command.
    pub(crate) fn status(&self, epoch: u64) -> Option<Status> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.durable.map(|durable| Status { epoch, durable, seen: state.seen.get(&epoch).copied() })
    }

    /// The copy's retained entries after the entry at `after`, oldest first, up to `ENTRIES_BYTES` of
    /// JSON. Empty unless the copy retains the entry at `after` or the one right after it: a gap in
    /// what is retained would leave the asker with entries it cannot chain.
    pub(crate) fn entries_after(&self, after: Offset) -> Vec<(Offset, LogEntry)> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(start) = state.recent.iter().position(|(offset, _)| *offset > after) else { return Vec::new() };
        let next = [Offset { seq: after.seq, index: after.index + 1 }, Offset::start_of(after.seq + 1)];
        let joined = match start {
            0 => next.contains(&state.recent[0].0),
            _ => state.recent[start - 1].0 == after,
        };
        if !joined {
            return Vec::new();
        }
        let mut bytes = 0;
        state
            .recent
            .iter()
            .skip(start)
            .take_while(|(_, entry)| {
                bytes += serde_json::to_vec(entry).map_or(usize::MAX / 2, |encoded| encoded.len());
                bytes <= ENTRIES_BYTES
            })
            .cloned()
            .collect()
    }
}

impl<C: StandbyChain + Send + 'static> Server<C> {
    /// Answers each node that connects, a few at once, for as long as it runs: a member runs it beside
    /// its following and drops it, freeing the endpoint, before it starts sequencing. A bind that fails
    /// is tried again.
    pub(crate) async fn run(self) -> std::convert::Infallible {
        let mut failed = false;
        let listener = loop {
            match TcpListener::bind(self.listen).await {
                Ok(listener) => break listener,
                Err(e) if failed => debug!(listen = %self.listen, "binding the member link failed again: {e}"),
                Err(e) => {
                    warn!(listen = %self.listen, "binding the member link failed, retrying: {e}");
                    failed = true;
                }
            }
            tokio::time::sleep(REBIND).await;
        };
        info!(listen = %self.listen, "answering members while following");
        let (server, permits) = (Arc::new(self), Arc::new(Semaphore::new(CONNECTIONS)));
        let mut connection = 0_u64;
        let mut answering = tokio::task::JoinSet::new();
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    warn!("accepting a member link connection failed: {e}");
                    continue;
                }
            };
            while answering.try_join_next().is_some() {}
            let Ok(permit) = permits.clone().try_acquire_owned() else {
                debug!(%peer, "too many member link connections: dropped");
                continue;
            };
            connection += 1;
            let span = info_span!("member_link", id = connection, peer = %peer);
            let server = server.clone();
            answering.spawn(
                async move {
                    let _permit = permit;
                    if let Err(e) = server.serve(stream).await {
                        debug!("a member link connection failed: {e:#}");
                    }
                }
                .instrument(span),
            );
        }
    }

    /// Reads one question and sends its answer.
    /// Error cases:
    /// - the question does not arrive in time or does not parse, or the answer cannot be sent
    async fn serve(&self, mut stream: TcpStream) -> anyhow::Result<()> {
        let request: Request = timeout(ASK_TIMEOUT, receive(&mut stream)).await.context("no question in time")??;
        let reply = match self.answer(&request).await {
            Ok(reply) => reply,
            Err(e) => Reply::Refused { reason: format!("{e:#}") },
        };
        send(&mut stream, &reply).await
    }

    /// The answer to `request`.
    /// Error cases:
    /// - the copy is not yet durable through a command, or the vouch is refused
    pub(crate) async fn answer(&self, request: &Request) -> anyhow::Result<Reply> {
        match request {
            Request::Status { epoch, nonce } => {
                let status = self.copy.status(*epoch).context("the copy is not yet durable through a command")?;
                let (chain_id, vault) = self.deployment;
                let signature = self.standby.sign(status.digest(chain_id, vault, self.member, &nonce.0));
                Ok(Reply::Status { member: self.member, status, signature })
            }
            Request::Entries { after } => Ok(Reply::Entries { entries: self.copy.entries_after(*after) }),
            Request::Vouch { request } => {
                let voucher = Voucher {
                    provider: self.provider,
                    platform: self.platform,
                    trusted: &self.trusted,
                    chain: &self.chain,
                    domain: self.domain,
                    signer: None,
                    standby: Some(self.standby.as_ref()),
                };
                Ok(Reply::Vouch { reply: answer(request, &voucher).await? })
            }
        }
    }
}

/// The status `reply` carries, once it recovers to `member`'s standby key `key` over the digest for
/// `nonce` in the deployment `vault` on `chain_id`.
/// Error cases:
/// - the reply is no status, names another member, or does not recover to the key
pub(crate) fn verified_status(
    reply: Reply,
    (chain_id, vault): (u64, Address),
    member: AccountId,
    key: AccountId,
    nonce: &[u8; 32],
) -> anyhow::Result<Status> {
    let Reply::Status { member: named, status, signature } = reply else {
        bail!("the member answered no status: {}", describe(&reply));
    };
    anyhow::ensure!(named == member, "asked {member}, answered as {named}");
    let digest = B256::from(status.digest(chain_id, vault, member, nonce));
    anyhow::ensure!(
        engine_core::eip712::recover(&digest, &signature) == Some(key),
        "the status does not recover to {member}'s standby key {key}"
    );
    Ok(status)
}

/// A short account of a reply that was not the one asked for.
pub(crate) fn describe(reply: &Reply) -> String {
    match reply {
        Reply::Refused { reason } => format!("refused: {reason}"),
        Reply::Status { .. } => "a status".into(),
        Reply::Entries { entries } => format!("{} entries", entries.len()),
        Reply::Vouch { .. } => "a vouch".into(),
    }
}

#[cfg(test)]
mod tests;
