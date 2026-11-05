//! The price feed tasks a node runs: the mock feed, or one task per venue connection.

use anyhow::Context;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::info;

use crate::config::{FeedProvider, FeedsSection};
use crate::feeds;
use crate::feeds::venue::{self, Timing};
use crate::node::secrets::Delivered;
use crate::node::shutdown::TaskResult;
use crate::sequencer::Request;

/// Spawns into `tasks` the feeds `feeds` configures, each sending its prices to `tx`.
/// Error cases:
/// - `provider = "venues"` without `[feeds.venues]`, or the venues' TLS config or a delivered key
///   does not read
pub(super) fn spawn(
    tasks: &mut JoinSet<TaskResult>,
    feeds: FeedsSection,
    delivered: &Delivered,
    tx: &mpsc::Sender<Request>,
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
            for conn in venues.connections {
                let (name, tls, feed_tx) = (conn.venue.name(), tls.clone(), tx.clone());
                // Off under the `secret_missing` startup raised; the monitor pages its markets.
                if delivered.withholds(&conn) {
                    continue;
                }
                let api_key = delivered.api_key(&conn)?;
                info!(venue = name, url = %conn.url, symbols = conn.symbols.len(), "starting venue feed");
                tasks.spawn(async move { (name, venue::run(conn, api_key, timing, tls, feed_tx).await) });
            }
        }
    }
    Ok(())
}
