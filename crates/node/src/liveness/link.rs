//! The member link: what a rotation-set member's node answers on its registered handover endpoint
//! while it follows the operator, and how another member's node asks. Each request is one handover
//! frame (`handover::send`), answered by one:
//! - `Status`: the seq and cumulative prefix its copy durably holds, and the newest head of the asked
//!   signer epoch it has seen, signed by its standby key over the asker's fresh nonce. The operator
//!   asks for it as a member's confirmation, and any node asks for it before it sequences;
//! - `Entries`: the copy's latest entries after a seq, which a promoted member fetches a gap with,
//!   checking every one against its own copy's seal chain;
//! - `Vouch`: a vouch for a candidate's standby key (`standby_key::vouch::answer`), signed with this
//!   member's standby key while the schedule lists it as promotable;
//! - `Attestation`: the latest vouch request quoted for its standby key, whose `engine.standby.v1`
//!   quote the operator checks (`vouch::admit`) to take the member's statuses before it is registered;
//! - the operator's log check (`log_check::Ask`), which carries no tag, answered as a following member
//!   answers it on the same endpoint (`log_check::answer`).
//!
//! The host carries the connection and can drop or delay it, but forges nothing: a status recovers to
//! the member's registered standby key, an entry must extend the asker's seal chain, and a vouch
//! recovers to its voucher. The endpoint is public, so a member answers at most `PER_PEER` connections
//! from one address and `CONNECTIONS` in all, drops a connection that sends no question within
//! `QUESTION_TIMEOUT`, and works on at most `HEAVY` entries or vouch questions at once: holding the
//! operator's status questions off takes many addresses reconnecting every second.

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
use crate::liveness::link::slots::{PeerSlot, Silent, SilentPlace, peer_key};
use crate::log::{LogEntry, Offset};
use crate::log_check::Ask;
use crate::log_check::answer::Answering;
use crate::log_heads::{Head, SignedHead};
use crate::standby_key::StandbyKeys;
use crate::standby_key::chain::{StandbyChain, StandbyDomain};
use crate::standby_key::vouch::{VouchReply, VouchRequest, Voucher, answer};
use crate::tee::sealing::Platform;
use crate::trusted_time::TrustedClock;

/// How many of the copy's latest entries a member keeps to answer `Entries` from.
pub(crate) const RECENT_ENTRIES: usize = 16_384;
/// The most bytes of entries one answer carries, under the frame limit.
const ENTRIES_BYTES: usize = 768 * 1024;
/// How many signer epochs' newest heads a member keeps.
const SEEN_EPOCHS: usize = 4;
/// How many heads seen past the copy's latest command a member keeps to check as the copy reaches them.
const AHEAD_HEADS: usize = 1_024;
/// How long one question gets, connecting and answering.
pub(crate) const ASK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an answer may take to be read: a slow reader holds no place past it.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
/// How many connections a member answers at once.
const CONNECTIONS: usize = 64;
/// How many of those one peer address may hold.
const PER_PEER: usize = 2;
/// How long a connection gets to send its question: an asker sends it as soon as it connects, so a
/// silent connection is dropped before it can hold a slot for long.
const QUESTION_TIMEOUT: Duration = Duration::from_secs(1);
/// How many `Entries` and `Vouch` questions a member works on at once; statuses never wait for them.
const HEAVY: usize = 2;
/// How long a new connection waits for the place a silent one gives up.
const EVICTION_WAIT: Duration = Duration::from_millis(500);
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
    /// Its standby key's attestation.
    Attestation,
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
    /// The latest vouch request quoted for its standby key.
    Attestation {
        request: Box<VouchRequest>,
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
    /// The key that sealed the durable head's command, when the copy knows it.
    sealer: Option<AccountId>,
    /// The newest head seen of each of the latest signer epochs, by epoch and the key it was seen under.
    seen: BTreeMap<SignerEpoch, Head>,
    /// The copy's latest entries, oldest first.
    recent: VecDeque<(Offset, LogEntry)>,
    /// The newest signed head the operator's stream carried, which the liveness caller relays.
    signed: Option<SignedHead>,
    /// The cumulative prefix after each of the copy's latest commands, by seq, and the key that sealed
    /// the command, if known.
    links: BTreeMap<u64, (B256, Option<AccountId>)>,
    /// Heads seen past the copy's latest command, by seq and signer epoch: checked as the copy reaches
    /// them.
    ahead: BTreeMap<(u64, SignerEpoch), B256>,
    /// The signer epochs under which the copy holds another link than a head it saw, the copy's command
    /// there sealed by the head's own key, or two heads differ, with the first such seq: a second history
    /// on that key; the member confirms nothing under them.
    conflicted: BTreeMap<SignerEpoch, u64>,
    /// The signer epochs under which a head differs from a link of the copy that another key sealed, or
    /// one of unknown sealer, as a returning operator's own tail beside its successor's heads, with the
    /// first such seq: the member confirms nothing under them until a rebuilt copy drops them.
    diverged: BTreeMap<SignerEpoch, u64>,
    /// The enclave the copy follows: the heads it sees, on the stream or the chain, are of its epochs.
    following: Option<AccountId>,
    /// The latest vouch request the member quoted for its standby key, which it answers `Attestation`
    /// with.
    attestation: Option<VouchRequest>,
}

/// A signer epoch and the key a head of it was seen under: a reorg can hand an epoch number to another
/// key, and put the first back, so evidence is kept per key.
type SignerEpoch = (u64, AccountId);

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
    /// The member's answers to the operator's log checks (`log_check::answer`), which share the
    /// endpoint; none without `[tee.sealing.handover]`.
    pub answering: Option<Arc<Answering>>,
}

/// A question on the endpoint: a node's `Request`, or the operator's log check, which carries no tag.
#[derive(Deserialize)]
#[serde(untagged)]
enum Question {
    Member(Request),
    LogCheck(Ask),
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

    /// Records `entries`, the newest a copy opened from disk holds with their offsets, oldest first, as
    /// the entries it serves: a restarted member answers a gap fetch from what it holds.
    pub(crate) fn served(&self, entries: impl IntoIterator<Item = (Offset, LogEntry)>) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.recent.extend(entries);
        while state.recent.len() > RECENT_ENTRIES {
            state.recent.pop_front();
        }
    }

    /// Records that the copy is durable through `head`, whose command `sealer` sealed.
    pub(crate) fn durable(&self, head: Head, sealer: Option<AccountId>) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        (state.durable, state.sealer) = (Some(head), sealer);
    }

    /// Records the copy's cumulative prefix `link` after the command at `seq`, which `sealer` sealed,
    /// and checks every head seen at that seq against it.
    pub(crate) fn linked(&self, seq: u64, link: B256, sealer: Option<AccountId>) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.links.insert(seq, (link, sealer));
        while state.links.len() > RECENT_ENTRIES {
            state.links.pop_first();
        }
        while let Some(entry) = state.ahead.first_entry().filter(|entry| entry.key().0 <= seq) {
            let ((at, epoch), seen) = entry.remove_entry();
            if state.links.get(&at).is_some_and(|(held, _)| *held != seen) {
                state.mismatch(epoch, at);
            }
        }
    }

    /// Records `head`, the last command the log on disk holds, which `sealer` sealed, as the copy's
    /// durable head until a follower links the copy (`Follower::link_with`): an operator that handed
    /// its keys over answers from it while its replica builds, so its successor's start check waits on
    /// no replay.
    pub(crate) fn seed(&self, head: Head, sealer: AccountId) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        (state.durable, state.sealer) = (Some(head), Some(sealer));
        state.links.insert(head.seq, (head.link, Some(sealer)));
    }

    /// Clears the links and entries of a copy set aside, and its divergence from heads it saw: a
    /// rebuilt copy records its own. Conflicts stand.
    pub(crate) fn rebuilt(&self) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.diverged.clear();
        state.links.clear();
        state.ahead.clear();
        state.recent.clear();
        (state.durable, state.sealer) = (None, None);
    }

    /// Records that the copy follows `enclave`, whose epochs the heads it sees from now on are of, and
    /// whose epochs its statuses answer for: another key's evidence stays, under that key, should a
    /// reorg put it back. Another enclave than the last drops the signed head the caller relays.
    pub(crate) fn follows(&self, enclave: AccountId) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.following.replace(enclave).is_some_and(|last| last != enclave) {
            state.signed = None;
        }
    }

    /// Records a head of `epoch` seen on the stream or the chain, keeping the newest of each epoch, and
    /// checks it against the copy's link at its seq, now or once the copy reaches it.
    pub(crate) fn saw(&self, epoch: u64, head: Head) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let epoch = state.under(epoch);
        // Two signed heads of one epoch with other links at one seq are two histories on its key.
        let pending = state.ahead.get(&(head.seq, epoch)).copied();
        let newest = state.seen.get(&epoch).filter(|seen| seen.seq == head.seq).map(|seen| seen.link);
        if [pending, newest].into_iter().flatten().any(|link| link != head.link) {
            state.conflict(epoch, head.seq);
        }
        let newest = state.seen.entry(epoch).or_insert(head);
        if head.seq > newest.seq {
            *newest = head;
        }
        while state.seen.len() > SEEN_EPOCHS {
            state.seen.pop_first();
        }
        match state.links.get(&head.seq).map(|(held, _)| *held) {
            Some(held) if held != head.link => state.mismatch(epoch, head.seq),
            Some(_) => {}
            None if state.links.last_key_value().is_none_or(|(last, _)| head.seq > *last) => {
                state.ahead.entry((head.seq, epoch)).or_insert(head.link);
                while state.ahead.len() > AHEAD_HEADS {
                    state.ahead.pop_last();
                }
            }
            None => {}
        }
    }

    /// The first seq at which the copy holds another link than a head of `epoch` it saw, if any, and
    /// whether that is a conflict rather than the copy's own divergence.
    #[cfg(test)]
    pub(crate) fn conflicted(&self, epoch: u64) -> Option<u64> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let under = state.under(epoch);
        state.conflicted.get(&under).or(state.diverged.get(&under)).copied()
    }

    /// Records a signed head the operator's stream carried, keeping the newest.
    pub(crate) fn signed_head(&self, head: SignedHead) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        // The followed enclave's heads only: another epoch than the held one is the one in effect now.
        let newer = state.signed.as_ref().is_none_or(|held| {
            head.epoch != held.epoch
                || (head.head.head.seq, head.head.signed_at) > (held.head.head.seq, held.head.signed_at)
        });
        if newer {
            state.signed = Some(head);
        }
    }

    /// The newest signed head the operator's stream carried.
    pub(crate) fn signed(&self) -> Option<SignedHead> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).signed.clone()
    }

    /// The status under `epoch`, with no durable head while the copy holds no command.
    /// Error cases:
    /// - the copy holds another link than a head of `epoch` it saw
    pub(crate) fn status(&self, epoch: u64) -> anyhow::Result<Status> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (epoch, key) = state.under(epoch);
        if let Some(seq) = state.conflicted.get(&(epoch, key)).or(state.diverged.get(&(epoch, key))) {
            bail!("the copy holds another link at seq {seq} than a head of signer epoch {epoch}: it confirms nothing");
        }
        Ok(Status {
            epoch,
            durable: state.durable,
            sealer: state.sealer.unwrap_or(AccountId([0; 20])),
            seen: state.seen.get(&(epoch, key)).copied(),
        })
    }

    /// The copy's retained entries after the entry at `after`, oldest first, up to `ENTRIES_BYTES` of
    /// JSON. Empty unless the copy retains the entry at `after` or the one right after it: a gap in
    /// what is retained would leave the asker with entries it cannot chain.
    pub(crate) fn entries_after(&self, after: Offset) -> Vec<(Offset, LogEntry)> {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(start) = state.recent.iter().position(|(offset, _)| *offset > after) else { return Vec::new() };
        // `after` is any peer's: the offsets right after it saturate rather than overflow.
        let next = [after.next(), Offset::start_of(after.seq.saturating_add(1))];
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
        let heavy = Arc::new(Semaphore::new(HEAVY));
        let peers: Arc<Mutex<BTreeMap<std::net::IpAddr, usize>>> = Arc::default();
        let silent = Silent::default();
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
            let Some(peer_slot) = PeerSlot::take(&peers, peer_key(peer.ip()), PER_PEER) else {
                debug!(%peer, "too many member link connections from this address: dropped");
                continue;
            };
            let permit = match permits.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                // Every place taken: the oldest connection that has not asked yet gives its place up.
                Err(_) if silent.evict_oldest() => {
                    timeout(EVICTION_WAIT, permits.clone().acquire_owned()).await.ok().and_then(Result::ok)
                }
                Err(_) => None,
            };
            let Some(permit) = permit else {
                debug!(%peer, "too many member link connections: dropped");
                continue;
            };
            connection += 1;
            let span = info_span!("member_link", id = connection, peer = %peer);
            let (server, heavy, place) = (server.clone(), heavy.clone(), silent.enter(connection));
            answering.spawn(
                async move {
                    let _held = (permit, peer_slot);
                    if let Err(e) = server.serve(stream, &heavy, place).await {
                        debug!("a member link connection failed: {e:#}");
                    }
                }
                .instrument(span),
            );
        }
    }

    /// Reads one question and sends its answer, read within `ANSWER_TIMEOUT`: an `Entries` or `Vouch`
    /// question only while one of the `heavy` permits is free, and a refusal at once otherwise, so no
    /// connection holds its place waiting behind them while status questions come. Until its question
    /// comes, the connection holds its `place` among the silent ones, which a newer one may take.
    /// Error cases:
    /// - the question does not arrive in time or does not parse, a newer connection takes the place of
    ///   this silent one, or the answer is not read in time
    async fn serve(&self, mut stream: TcpStream, heavy: &Semaphore, place: SilentPlace) -> anyhow::Result<()> {
        let question: Question = tokio::select! {
            question = timeout(QUESTION_TIMEOUT, receive(&mut stream)) => question.context("no question in time")??,
            () = place.evicted() => bail!("a newer connection took this silent one's place"),
        };
        drop(place);
        // A log check quotes only for a question its checkpoint's signer signed, so it needs no permit.
        let request = match question {
            Question::Member(request) => request,
            Question::LogCheck(ask) => match &self.answering {
                Some(answering) => return answering.respond(&mut stream, &ask).await,
                None => bail!("a log check on an endpoint that answers none"),
            },
        };
        let permit = match request {
            Request::Status { .. } | Request::Attestation => Ok(None),
            Request::Entries { .. } | Request::Vouch { .. } => heavy.try_acquire().map(Some),
        };
        // A heavy answer keeps its permit until it is sent.
        let (reply, _held) = match permit {
            Ok(held) => match self.answer(&request).await {
                Ok(reply) => (reply, held),
                Err(e) => (Reply::Refused { reason: format!("{e:#}") }, held),
            },
            Err(_) => (Reply::Refused { reason: "busy answering entries and vouches: ask again".into() }, None),
        };
        timeout(ANSWER_TIMEOUT, send(&mut stream, &reply)).await.context("the answer was not read in time")?
    }

    /// The answer to `request`.
    /// Error cases:
    /// - the copy is not yet durable through a command, or the vouch is refused
    pub(crate) async fn answer(&self, request: &Request) -> anyhow::Result<Reply> {
        match request {
            Request::Status { epoch, nonce } => {
                let status = self.copy.status(*epoch)?;
                let (chain_id, vault) = self.deployment;
                let signature = self.standby.sign(status.digest(chain_id, vault, self.member, &nonce.0));
                Ok(Reply::Status { member: self.member, status, signature })
            }
            Request::Entries { after } => Ok(Reply::Entries { entries: self.copy.entries_after(*after) }),
            Request::Attestation => Ok(Reply::Attestation { request: self.copy.attestation(self.standby.address())? }),
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

mod ask;
mod attested;
mod state;
pub(crate) use ask::{Asking, Tcp, describe, verified_status};
pub(crate) mod slots;

#[cfg(test)]
mod tests;
