//! The price feed tasks a node runs: the mock feed, or one task per venue connection. A rotation-set
//! member opens its venue connections a lead before its own turn's boundary (`Warm`), with their
//! quotes held back (`feeds::venue::live`) on the request channel the node it starts then sequences
//! from, so its first commands have fresh quotes from every venue.

use std::time::Duration;

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use engine_core::Engine;
use engine_core::session::Schedule;
use engine_core::types::{MarketId, Millis};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::info;

use crate::config::{FeedProvider, FeedsSection};
use crate::feeds;
use crate::feeds::status::{Connection, FeedStatus, SymbolState, SymbolStatus};
use crate::feeds::venue::{self, Held, Timing};
use crate::node::CHANNEL_CAPACITY;
use crate::node::secrets::Delivered;
use crate::node::shutdown::TaskResult;
use crate::sequencer::Request;

/// How long before its own turn's boundary a member opens its venue connections: past a connection's
/// subscription, its first books and its first answered pings, with time to spare for a reconnect.
pub(crate) const WARM_LEAD: Duration = Duration::from_secs(60);

/// A node's request channel, which its sequencer reads.
type Channel = (mpsc::Sender<Request>, mpsc::Receiver<Request>);

/// Venue feeds a member opened ahead of its turn, on the request channel the node it starts sequences
/// from, their quotes held back until that node lets them through (`Live::start`). Dropping it closes
/// the connections.
pub(crate) struct Warm {
    channel: Channel,
    feeds: Live,
}

/// Warm feeds' tasks, the switch that lets their quotes through, and what they report to the API.
pub(crate) struct Live {
    tasks: JoinSet<TaskResult>,
    live: watch::Sender<bool>,
    status: FeedStatus,
}

impl Warm {
    /// Opens `feeds`' venue connections with their quotes held back, on a channel of their own, judging
    /// their markets' hours by `hours` until the engine answers; `None` for the mock feed, which opens
    /// nothing.
    /// Error cases:
    /// - as `spawn`
    pub(crate) fn open(
        feeds: &FeedsSection,
        delivered: &Delivered,
        hours: BTreeMap<MarketId, Schedule>,
    ) -> anyhow::Result<Option<Self>> {
        if feeds.provider != FeedProvider::Venues {
            return Ok(None);
        }
        let (channel, (live, switch)) = (mpsc::channel(CHANNEL_CAPACITY), watch::channel(false));
        let (mut tasks, status) = (JoinSet::new(), FeedStatus::default());
        let held = Held { live: switch, hours: Arc::new(hours) };
        spawn(&mut tasks, feeds.clone(), delivered, (&channel.0, &held), &status)?;
        Ok(Some(Self { channel, feeds: Live { tasks, live, status } }))
    }
}

impl Live {
    /// Lets the feeds' quotes through to the sequencer now reading their channel, and supervises their
    /// tasks among `tasks`, as the node's own feeds are: one stopping stops the node. With every
    /// connection off for want of its delivered secret, there is no task to supervise.
    fn start(self, tasks: &mut JoinSet<TaskResult>) {
        let Live { tasks: mut feeds, live, .. } = self;
        live.send_replace(true);
        info!(connections = feeds.len(), "the venue feeds opened before the turn send their quotes");
        if feeds.is_empty() {
            return;
        }
        tasks.spawn(async move {
            match feeds.join_next().await {
                Some(Ok(stopped)) => stopped,
                Some(Err(e)) => ("feeds", Err(anyhow!("a venue feed task failed: {e}"))),
                None => ("feeds", Ok(())),
            }
        });
    }
}

/// The node's request channel: the one the feeds a member opened before its turn write to, with those
/// feeds, which report to `status` from then on, or a fresh one.
pub(super) fn channel(warm: Option<Warm>, status: &mut FeedStatus) -> (Channel, Option<Live>) {
    match warm {
        Some(Warm { channel, feeds }) => {
            *status = feeds.status.clone();
            (channel, Some(feeds))
        }
        None => (mpsc::channel(CHANNEL_CAPACITY), None),
    }
}

/// Starts the node's price feeds: lets through the quotes of those a member opened before its turn,
/// or spawns `feeds`' own, sending to `tx` and reporting to `status`.
/// Error cases:
/// - as `spawn`
pub(super) fn start(
    tasks: &mut JoinSet<TaskResult>,
    warm: Option<Live>,
    (feeds, delivered): (FeedsSection, &Delivered),
    tx: &mpsc::Sender<Request>,
    status: &FeedStatus,
) -> anyhow::Result<()> {
    match warm {
        Some(warm) => {
            warm.start(tasks);
            Ok(())
        }
        None => spawn(tasks, feeds, delivered, (tx, &Held::never()), status),
    }
}

/// Each market's trading hours in `engine`, which a member's warm feeds judge closures by.
pub(crate) fn market_hours(engine: &Engine) -> BTreeMap<MarketId, Schedule> {
    engine.markets().iter().map(|(id, market)| (*id, market.spec.schedule.clone())).collect()
}

/// Whether a member whose trusted time has reached `reached` opens its venue connections for its own
/// turn at `boundary`: within `WARM_LEAD` of it, or past it while it takes the turn.
pub(crate) fn warm_due(boundary: Millis, reached: Millis) -> bool {
    let lead = Millis::try_from(WARM_LEAD.as_millis()).expect("the lead fits in milliseconds");
    boundary.saturating_sub(reached) <= lead
}

/// Spawns into `tasks` the feeds `feeds` configures, each sending its prices to `tx` as `held` lets it.
/// Error cases:
/// - `provider = "venues"` without `[feeds.venues]`, or the venues' TLS config or a connection's
///   credentials do not read
fn spawn(
    tasks: &mut JoinSet<TaskResult>,
    feeds: FeedsSection,
    delivered: &Delivered,
    (tx, held): (&mpsc::Sender<Request>, &Held),
    status: &FeedStatus,
) -> anyhow::Result<()> {
    match feeds.provider {
        FeedProvider::Mock => {
            let feed_tx = tx.clone();
            tasks.spawn(async move {
                feeds::run_mock(&feeds, feed_tx).await;
                ("feeds", Ok(()))
            });
        }
        FeedProvider::Venues => {
            let venues = feeds.venues.context("feeds.provider = \"venues\" needs [feeds.venues]")?;
            let timing = Timing::for_venues(&venues);
            let tls = venue::tls_config()?;
            for (index, conn) in venues.connections.into_iter().enumerate() {
                let (name, tls, feed_tx) = (conn.venue.name(), tls.clone(), tx.clone());
                // Off under the `secret_missing` startup raised; the monitor pages its markets.
                if delivered.withholds(&conn) {
                    let symbols = conn
                        .symbols
                        .iter()
                        .map(|s| SymbolStatus {
                            symbol: s.symbol.clone(),
                            market: s.market,
                            contract: s.contract.clone(),
                            state: SymbolState::Off,
                        })
                        .collect();
                    status.publish(name, index, Connection { symbols });
                    continue;
                }
                let credentials = delivered.credentials(&conn, &tls, timing.timeout)?;
                info!(venue = name, url = %conn.url, symbols = conn.symbols.len(), "starting venue feed");
                let (report, held) = ((status.clone(), index), held.clone());
                tasks.spawn(async move {
                    (name, venue::run_reporting(conn, credentials, timing, tls, (feed_tx, held), report).await)
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Warm feeds go live when the node starts them, and their tasks join the node's: a feed task's
    /// stopping stops the node, but with every connection off for want of its secret no task is added,
    /// so none stops it at once.
    #[tokio::test]
    async fn starting_warm_feeds_lets_them_through_and_supervises_only_their_tasks() {
        for running in [false, true] {
            let (live, switch) = watch::channel(false);
            let mut feeds = JoinSet::new();
            if running {
                feeds.spawn(std::future::pending::<TaskResult>());
            }
            let mut node = JoinSet::new();
            Live { tasks: feeds, live, status: FeedStatus::default() }.start(&mut node);
            assert!(*switch.borrow(), "the switch was not set");
            assert_eq!(node.len(), usize::from(running), "running {running}: the node supervises {} tasks", node.len());
        }
    }

    #[test]
    fn a_member_opens_its_feeds_within_the_lead_of_its_turn_and_while_it_takes_it() {
        const BOUNDARY: Millis = 1_800_000_000_000;
        let lead = Millis::try_from(WARM_LEAD.as_millis()).unwrap();
        assert!(!warm_due(BOUNDARY, BOUNDARY - lead - 1), "before the lead");
        assert!(warm_due(BOUNDARY, BOUNDARY - lead), "at the lead");
        assert!(warm_due(BOUNDARY, BOUNDARY), "at the boundary");
        assert!(warm_due(BOUNDARY, BOUNDARY + 30_000), "past it, while it takes the turn");
    }
}
