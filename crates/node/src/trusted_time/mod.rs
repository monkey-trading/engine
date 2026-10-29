//! Trusted time: bounds the host's wall clock with signed Roughtime readings (RFC 10049).
//!
//! The host controls the enclave's network and wall clock. It cannot forge a signed reading, but
//! it can delay or drop replies, so a reply only places true time, at the moment it arrives,
//! between the server's midpoint less its radius and the midpoint plus its radius plus the round
//! trip. A round makes a sample only when replies come from a strict majority of the configured
//! servers, and at least two of them share an instant. As many servers as fall outside that
//! majority may sign a wrong time, so true time lies wherever all but that many replies overlap;
//! the sample spans those instants, and counts only if it is no wider than the drift bound. The
//! sequencer checks every stamp against the latest sample through `TimeGate`, and the API the time
//! it signs answers with through `TrustedClock`.
//!
//! The servers and their keys come from the config file, on host storage; the attestation binds that
//! file's SHA-256, so a verifier checks them against a published copy.
//!
//! Servers are queried concurrently over UDP, each with its own random nonce; the node does not
//! chain requests into malfeasance reports (RFC 10049 §8.2), since agreement is all it needs.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::VerifyingKey;
use engine_core::types::Millis;
use rand::RngCore;
use rand::rngs::OsRng;
use tokio::net::{UdpSocket, lookup_host};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant as TokioInstant, MissedTickBehavior, interval, timeout};
use tracing::{debug, info, warn};

mod gate;
pub use crate::trusted_time::gate::{NO_SAMPLE_GRACE, TimeGate, TrustedClock};

use crate::clock::Clock;
use crate::config::{RoughtimeServer, TimeSection};
use crate::roughtime::{self, Reading};
use crate::sequencer::Request;

/// Consecutive failed queries before a server is alerted. A single failure is normal: UDP drops
/// packets, and servers send a fraction of invalid replies on purpose (RFC 10049 §7).
const FAILURES_BEFORE_ALERT: u32 = 3;

/// True time as a round of replies bounds it: at `at`, it lay within `earliest_ms..=latest_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub at: Instant,
    pub earliest_ms: Millis,
    pub latest_ms: Millis,
}

/// One server's verified reply, bounding true time at the moment it arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Reply {
    received: Instant,
    earliest_ms: Millis,
    latest_ms: Millis,
}

/// A configured server and what the task has seen of it.
struct Server {
    config: RoughtimeServer,
    failures: u32,
    retry_at: Option<Instant>,
    /// Whether its last verified reply fell outside the agreed sample.
    dissenting: bool,
}

/// The configured servers, queried one round at a time.
struct Sampler {
    servers: Vec<Server>,
    /// How long each query waits for its reply.
    wait: Duration,
    /// The longest back-off before a failing server is asked again.
    max_retry: Duration,
    max_width_ms: Millis,
    /// Whether the last round made a sample.
    sampling: bool,
}

impl Reply {
    /// The bounds `reading` puts on true time when it arrived at `received`, `round_trip` after
    /// its request left.
    /// Error cases:
    /// - the bounds do not fit in milliseconds since the epoch
    fn new(reading: Reading, round_trip: Duration, received: Instant) -> anyhow::Result<Self> {
        let midpoint_ms = i64::try_from(reading.midpoint_s).ok().and_then(|s| s.checked_mul(1000));
        let midpoint_ms = midpoint_ms.context("midpoint out of range")?;
        let radius_ms = Millis::from(reading.radius_s) * 1000;
        let latest_ms = midpoint_ms.checked_add(radius_ms).and_then(|ms| ms.checked_add(ceil_millis(round_trip)));
        Ok(Self {
            received,
            earliest_ms: midpoint_ms - radius_ms,
            latest_ms: latest_ms.context("midpoint out of range")?,
        })
    }

    /// The bounds this reply puts on true time at `at`, no earlier than its arrival: true time
    /// has moved on by the monotonic time elapsed.
    fn bounds_at(&self, at: Instant) -> (Millis, Millis) {
        let since = at.saturating_duration_since(self.received);
        (self.earliest_ms.saturating_add(floor_millis(since)), self.latest_ms.saturating_add(ceil_millis(since)))
    }
}

impl Server {
    /// A server nothing has been heard from yet.
    fn new(config: RoughtimeServer) -> Self {
        Self { config, failures: 0, retry_at: None, dissenting: false }
    }

    /// Records a verified reply, ending any outage.
    fn answered(&mut self) {
        if self.failures >= FAILURES_BEFORE_ALERT {
            info!(
                alert_resolved = "roughtime_server_failing",
                server = %self.config.name,
                "roughtime server answering again after {} failures",
                self.failures
            );
        }
        self.failures = 0;
        self.retry_at = None;
    }

    /// Records a query that failed at `now`, and backs off, by at most `max_retry`, before the next.
    fn failed(&mut self, now: Instant, max_retry: Duration, error: &anyhow::Error) {
        self.failures = self.failures.saturating_add(1);
        let (server, failures) = (&self.config.name, self.failures);
        if failures == FAILURES_BEFORE_ALERT {
            // The count rises while the server stays down, so it is in the message, not the key.
            warn!(alert = "roughtime_server_failing", %server, "roughtime queries failing, {failures} in a row: {error:#}");
        } else {
            debug!(%server, failures, "roughtime query failed: {error:#}");
        }
        self.retry_at = Some(now + retry_delay(failures, max_retry));
    }

    /// Records whether `reply`, verified, agrees with the round's `sample`, and warns once when the
    /// server stops agreeing: a server signing the wrong time leaves fewer to make a quorum.
    fn compare(&mut self, reply: &Reply, sample: &Sample) {
        let (earliest, latest) = reply.bounds_at(sample.at);
        let agrees = earliest <= sample.latest_ms && sample.earliest_ms <= latest;
        let server = &self.config.name;
        if !agrees && !self.dissenting {
            warn!(
                alert = "roughtime_server_disagrees",
                %server,
                "roughtime server signed a time outside the agreed sample ({earliest} to {latest}, sample {} to {})",
                sample.earliest_ms,
                sample.latest_ms
            );
        } else if agrees && self.dissenting {
            info!(alert_resolved = "roughtime_server_disagrees", %server, "roughtime server agrees again");
        }
        self.dissenting = !agrees;
    }
}

impl Sampler {
    /// A sampler for `cfg`'s servers, logging each so the time sources are on record.
    fn new(cfg: TimeSection) -> Self {
        for s in &cfg.servers {
            info!(server = %s.name, address = %s.address, key = %STANDARD.encode(s.public_key.as_bytes()), "roughtime server");
        }
        Self {
            wait: cfg.max_drift,
            max_retry: max_retry_delay(cfg.interval, cfg.max_drift),
            max_width_ms: floor_millis(cfg.max_drift),
            servers: cfg.servers.into_iter().map(Server::new).collect(),
            sampling: true,
        }
    }

    /// Queries the servers once and returns the sample they agree on, if they do. Warns once when
    /// rounds stop making samples, and logs when they make one again.
    /// Error cases:
    /// - a query task panicked
    async fn round(&mut self, clock: &Arc<dyn Clock>) -> anyhow::Result<Option<Sample>> {
        let replies = self.query(clock).await?;
        let ranges: Vec<Reply> = replies.iter().map(|(_, reply)| *reply).collect();
        match agree(&ranges, self.servers.len(), self.max_width_ms) {
            Ok(sample) => {
                if !self.sampling {
                    info!(
                        alert_resolved = "trusted_time_no_sample",
                        "trusted time sample restored from {} replies",
                        replies.len()
                    );
                }
                self.sampling = true;
                replies.iter().for_each(|(i, reply)| self.servers[*i].compare(reply, &sample));
                debug!(
                    earliest = sample.earliest_ms,
                    latest = sample.latest_ms,
                    replies = replies.len(),
                    "trusted time sample"
                );
                Ok(Some(sample))
            }
            // The sequencer halts with a Critical alert if this lasts past the grace.
            Err(e) if self.sampling => {
                warn!(alert = "trusted_time_no_sample", "no trusted time sample: {e:#}");
                self.sampling = false;
                Ok(None)
            }
            Err(e) => {
                debug!("no trusted time sample: {e:#}");
                Ok(None)
            }
        }
    }

    /// Queries every server that is not backing off, concurrently, and returns each verified
    /// reply with its server's index.
    /// Error cases:
    /// - a query task panicked
    async fn query(&mut self, clock: &Arc<dyn Clock>) -> anyhow::Result<Vec<(usize, Reply)>> {
        let now = clock.instant();
        let wait = self.wait;
        let mut queries = JoinSet::new();
        for (i, server) in self.servers.iter().enumerate().filter(|(_, s)| s.retry_at.is_none_or(|at| at <= now)) {
            let (address, key, clock) = (server.config.address.clone(), server.config.public_key, clock.clone());
            queries.spawn(async move {
                let reply = timeout(wait, query(&address, &key, clock.as_ref(), wait)).await;
                (i, reply.unwrap_or_else(|_| Err(anyhow!("no reply within {wait:?}"))))
            });
        }
        let mut replies = Vec::new();
        while let Some(joined) = queries.join_next().await {
            let (i, reply) = joined.context("roughtime query panicked")?;
            let server = &mut self.servers[i];
            match reply {
                Ok(reply) => {
                    debug!(server = %server.config.name, earliest = reply.earliest_ms, latest = reply.latest_ms, "roughtime reply");
                    server.answered();
                    replies.push((i, reply));
                }
                Err(e) => server.failed(now, self.max_retry, &e),
            }
        }
        Ok(replies)
    }
}

/// Samples the configured servers every `interval`, and publishes each agreed sample on `published`
/// and sends it to the sequencer. Network failures are retried here, so it returns only when the
/// sequencer has gone.
/// Error cases:
/// - the sequencer has gone away
/// - a query task panicked
pub async fn run(
    cfg: TimeSection,
    clock: Arc<dyn Clock>,
    tx: mpsc::Sender<Request>,
    published: watch::Sender<Option<Sample>>,
) -> anyhow::Result<()> {
    let mut rounds = interval(cfg.interval);
    rounds.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut sampler = Sampler::new(cfg);
    loop {
        rounds.tick().await;
        if let Some(sample) = sampler.round(&clock).await? {
            published.send_replace(Some(sample));
            tx.send(Request::TimeSample(sample)).await.map_err(|_| anyhow!("sequencer gone"))?;
        }
    }
}

/// Resolves `address` and asks its addresses for a reply verified under `key`, within `wait`.
/// Error cases:
/// - the address does not resolve
/// - no address gives a verified reply (see `query_addresses`)
async fn query(address: &str, key: &VerifyingKey, clock: &dyn Clock, wait: Duration) -> anyhow::Result<Reply> {
    let deadline = TokioInstant::now() + wait;
    // The host answers DNS and carries the packets, so a reply counts only once it verifies under
    // the configured key; a wrong address or a forged reply is a failed query.
    let peers: Vec<SocketAddr> = lookup_host(address).await.with_context(|| format!("resolving {address}"))?.collect();
    query_addresses(&peers, key, clock, deadline).await.with_context(|| format!("querying {address}"))
}

/// Asks each of one server's addresses in turn, each for an equal share of the time left before
/// `deadline`, and returns the first verified reply, so one unreachable address does not lose the
/// server.
/// Error cases:
/// - there are no addresses
/// - every address fails, or stays silent for its share
async fn query_addresses(
    peers: &[SocketAddr],
    key: &VerifyingKey,
    clock: &dyn Clock,
    deadline: TokioInstant,
) -> anyhow::Result<Reply> {
    ensure!(!peers.is_empty(), "no addresses");
    let mut failures = Vec::new();
    for (i, peer) in peers.iter().enumerate() {
        let left = u32::try_from(peers.len() - i).unwrap_or(u32::MAX);
        let share = deadline.saturating_duration_since(TokioInstant::now()) / left;
        match timeout(share, query_address(*peer, key, clock)).await {
            Ok(Ok(reply)) => return Ok(reply),
            Ok(Err(e)) => failures.push(format!("{e:#}")),
            Err(_) => failures.push(format!("{peer}: no reply within {share:?}")),
        }
    }
    bail!("{}", failures.join("; "))
}

/// Sends one request to `peer` over UDP and verifies the reply under `key`.
/// Error cases:
/// - the socket cannot be bound, send, or receive
/// - the reply fails verification, or its bounds overflow
async fn query_address(peer: SocketAddr, key: &VerifyingKey, clock: &dyn Clock) -> anyhow::Result<Reply> {
    let local: SocketAddr =
        if peer.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() };
    let socket = UdpSocket::bind(local).await.context("binding a UDP socket")?;
    socket.connect(peer).await.with_context(|| format!("connecting to {peer}"))?;
    let mut nonce = [0; 32];
    OsRng.fill_bytes(&mut nonce);
    let request = roughtime::Request::new(key, nonce);
    // A reply may not be larger than its request; a larger one is cut short and fails to parse.
    let mut buf = vec![0; request.packet().len()];
    let sent = clock.instant();
    socket.send(request.packet()).await.with_context(|| format!("sending to {peer}"))?;
    let len = socket.recv(&mut buf).await.with_context(|| format!("receiving from {peer}"))?;
    let received = clock.instant();
    let reading = request.verify(&buf[..len], key).with_context(|| format!("reply from {peer}"))?;
    Reply::new(reading, received.saturating_duration_since(sent), received)
}

/// Combines replies, each bounding true time when it arrived, into one sample at the last arrival.
/// It needs replies from a quorum of the `servers` configured, at least two of which share an
/// instant. As many servers as fall outside a quorum may sign a wrong time, so true time lies in
/// all but that many of the replies, and the sample spans every instant that many replies cover.
/// Error cases:
/// - fewer replies than a quorum
/// - no two replies share an instant, or no instant is covered by enough replies
/// - the sample is wider than `max_width_ms`, too uncertain to check a stamp against
fn agree(replies: &[Reply], servers: usize, max_width_ms: Millis) -> anyhow::Result<Sample> {
    let needed = quorum(servers);
    ensure!(replies.len() >= needed, "only {} replies, {needed} needed", replies.len());
    // A quorum is a strict majority, so this is at least one.
    let covering = replies.len() - (servers - needed);
    let at = replies.iter().map(|r| r.received).max().context("no replies")?;
    let ranges: Vec<(Millis, Millis)> = replies.iter().map(|r| r.bounds_at(at)).collect();
    let cover = |t: Millis| ranges.iter().filter(|(lo, hi)| (*lo..=*hi).contains(&t)).count();
    // More than one server must agree: some instant lies in at least two replies.
    ensure!(ranges.iter().map(|r| cover(r.0)).max() >= Some(2), "no two replies agree on an instant");
    let earliest_ms = ranges.iter().map(|r| r.0).filter(|t| cover(*t) >= covering).min();
    let latest_ms = ranges.iter().map(|r| r.1).filter(|t| cover(*t) >= covering).max();
    let (Some(earliest_ms), Some(latest_ms)) = (earliest_ms, latest_ms) else {
        bail!("no instant is covered by {covering} of {} replies", replies.len());
    };
    let width_ms = latest_ms.saturating_sub(earliest_ms);
    ensure!(width_ms <= max_width_ms, "replies agree only within {width_ms}ms, wider than the {max_width_ms}ms bound");
    Ok(Sample { at, earliest_ms, latest_ms })
}

/// The replies a sample needs from `servers` configured servers: a strict majority, and at least
/// two once config validation has required two servers.
fn quorum(servers: usize) -> usize {
    servers / 2 + 1
}

/// The longest back-off before a failing server is asked again. Rounds start every `interval`, or
/// every `max_drift` while a silent server holds them, and a reply takes up to `max_drift`, so this
/// puts a server that recovers back in a sample within the grace of its last failure. It is at
/// least RFC 10049's one second.
fn max_retry_delay(interval: Duration, max_drift: Duration) -> Duration {
    let round_and_reply = interval.max(max_drift).saturating_add(max_drift);
    NO_SAMPLE_GRACE.saturating_sub(round_and_reply).max(Duration::from_secs(1))
}

/// RFC 10049 §5: wait at least 1.5^(n-1) seconds before retrying after n failures, here capped at
/// `max_retry`.
fn retry_delay(failures: u32, max_retry: Duration) -> Duration {
    let cap = u64::try_from(max_retry.as_millis()).unwrap_or(u64::MAX);
    let ms = (1..failures).try_fold(1_000u64, |ms, _| (ms < cap).then(|| ms.saturating_mul(3) / 2)).unwrap_or(cap);
    Duration::from_millis(ms.min(cap))
}

/// `d` in whole milliseconds, rounded down.
fn floor_millis(d: Duration) -> Millis {
    Millis::try_from(d.as_millis()).unwrap_or(Millis::MAX)
}

/// `d` in whole milliseconds, rounded up.
fn ceil_millis(d: Duration) -> Millis {
    Millis::try_from(d.as_nanos().div_ceil(1_000_000)).unwrap_or(Millis::MAX)
}

#[cfg(test)]
pub(crate) mod tests;
