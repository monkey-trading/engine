//! The predecessor's listener. Every member's node connects to the endpoint the registry names, so it
//! is public: it sends each connection its challenge at once, holds at most `CONNECTIONS` connections
//! and `PER_PEER` from one address (an IPv6 /64), drops a connection that sends no request within
//! `REQUEST_TIMEOUT` of its challenge, and, with every place taken, or every place of the new
//! connection's address, gives the place of the oldest connection there that has not sent its request
//! to the new one. Holding a successor off takes many addresses reconnecting faster than it quotes, or,
//! behind address translation, one reconnecting that fast. The requests are served one at a time.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use ring::rand::{SecureRandom, SystemRandom};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio::time::timeout;
use tracing::{Instrument, debug, error, info_span, warn};

use crate::checkpoints::CheckpointChain;
use crate::handover::assigned::Assigned;
use crate::handover::predecessor::{Predecessor, Served};
use crate::handover::{Challenge, Request, receive, send};
use crate::liveness::link::slots::{PeerSlot, Silent, SilentPlace, peer_key};
use crate::tee::keys::Head;

/// How many connections the listener holds at once.
pub(super) const CONNECTIONS: usize = 64;
/// How many of them one peer address may hold.
pub(super) const PER_PEER: usize = 2;
/// How long a connection has, from its challenge, to send its request: a successor fetches its quote's
/// collateral before it connects, so its request follows the challenge by one quote.
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a new connection waits for the place of the silent one it told to leave.
const EVICTION_WAIT: Duration = Duration::from_millis(500);

impl<C: CheckpointChain> Predecessor<C> {
    /// Serves successors until one confirms a handover, which ends the node's run with the assignment
    /// the successor bound, if any (#596). Once sequencing has stopped it goes on serving them, under
    /// `handover_unconfirmed`: the vault names the successor's release, so this release's own copy of
    /// the keys no longer opens, and only a confirmed handover or the operator ends it.
    /// Error cases:
    /// - the node cannot stop sequencing, or has no final checkpoint to hand over
    pub(crate) async fn run(self) -> anyhow::Result<Option<Assigned>> {
        // The final checkpoint once sequencing has stopped, held by the one request being served.
        let head = AsyncMutex::new(None);
        let (peers, silent) = (Arc::default(), Silent::default());
        let (mut connection, mut serving) = (0_u64, FuturesUnordered::new());
        loop {
            let served = tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(accepted) => accepted,
                        Err(e) => {
                            warn!("accepting a handover connection failed: {e}");
                            continue;
                        }
                    };
                    let key = peer_key(peer.ip());
                    let Some((slot, evicted)) = PeerSlot::take_or_evict(&peers, key, self.per_peer, &silent) else {
                        let share = self.per_peer;
                        debug!(%peer, "closed a handover connection: its address's {share} have sent requests");
                        continue;
                    };
                    // A full listener: the oldest connection that has not sent its request gives its place up,
                    // unless the address's own oldest just did, and whatever connection ends first is settled
                    // below.
                    let freed = if serving.len() < CONNECTIONS {
                        None
                    } else if silent.make_room(evicted)
                        && let Ok(Some(freed)) = timeout(EVICTION_WAIT, serving.next()).await
                    {
                        Some(freed)
                    } else {
                        debug!(%peer, "closed a handover connection: {} are being served", serving.len());
                        continue;
                    };
                    connection += 1;
                    let span = info_span!("handover", id = connection, peer = %peer);
                    serving.push(self.connection(stream, slot, silent.enter(connection, key), &head).instrument(span));
                    match freed {
                        Some(served) => served,
                        None => continue,
                    }
                }
                Some(served) = serving.next(), if !serving.is_empty() => served,
            };
            if let Served::Confirmed(bound) = served? {
                return Ok(bound);
            }
        }
    }

    /// One connection, holding `_slot` of its address's share: its challenge, its request within
    /// `REQUEST_TIMEOUT` unless a newer connection takes its `place` among the silent ones first, then,
    /// once no other request is being served, its answer (`serve`).
    /// Error cases:
    /// - the node cannot stop sequencing or has no final checkpoint, which ends the node
    async fn connection(
        &self,
        mut stream: TcpStream,
        _slot: PeerSlot,
        place: SilentPlace,
        head: &AsyncMutex<Option<Head>>,
    ) -> anyhow::Result<Served> {
        let mut challenge = [0u8; 32];
        if SystemRandom::new().fill(&mut challenge).is_err() {
            warn!("the RNG failed: no challenge for this connection");
            return Ok(Served::Unconfirmed);
        }
        let greeted = tokio::select! {
            greeted = timeout(REQUEST_TIMEOUT, greet(&mut stream, challenge)) => Some(greeted),
            () = place.evicted() => None,
        };
        let Some(greeted) = greeted.filter(|_| place.leave()) else {
            debug!("a newer connection took this silent one's place");
            return Ok(Served::Unconfirmed);
        };
        // Any stranger can end a connection before its request, so those endings are logged quietly.
        let request = match greeted {
            Ok(Ok(greeted)) => greeted,
            Ok(Err(e)) => {
                debug!("the connection ended before its request: {e:#}");
                return Ok(Served::Unconfirmed);
            }
            Err(_) => {
                debug!("no request in time");
                return Ok(Served::Unconfirmed);
            }
        };
        let mut head = head.lock().await;
        let served = self.serve(&mut stream, challenge, request, &mut head).await?;
        if matches!(served, Served::Unconfirmed) && head.is_some() {
            error!(
                alert = "handover_unconfirmed",
                "sequencing has stopped and no successor holds the keys yet: this release goes on serving \
                 successors, and stopping it loses the keys, since its own copy no longer opens"
            );
        }
        Ok(served)
    }
}

/// Sends `challenge` and reads the request that must bind it.
/// Error cases:
/// - the challenge cannot be sent, or the request is unreadable
async fn greet(stream: &mut TcpStream, challenge: [u8; 32]) -> anyhow::Result<Request> {
    let challenge = Challenge { nonce: hex::encode(challenge), standby: true };
    send(stream, &challenge).await.context("sending the challenge failed")?;
    receive(stream).await.context("an unreadable request")
}
