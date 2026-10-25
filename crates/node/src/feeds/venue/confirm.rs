//! Confirming with the engine, once the sequencer has applied a venue's quotes, which symbols the
//! venue counts as quoting, which the engine does not list or take quotes for, and each market's
//! trading hours.

use std::collections::{BTreeMap, BTreeSet};
use std::mem::take;
use std::sync::Arc;

use anyhow::anyhow;
use engine_core::session::Schedule;
use engine_core::types::{MarketId, Millis};
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tracing::{info, warn};

use crate::clock::now_ms;
use crate::feeds::venue::VenueFeed;
use crate::feeds::venue::quotes::counts;
use crate::sequencer::{Query, Request};

/// What the engine says of the symbols a confirmation asks about.
#[derive(Debug, Default)]
pub(super) struct Confirmation {
    /// Symbols where the quote the engine holds from the venue counts as the venue quoting.
    pub(super) fresh: Vec<String>,
    /// Symbols whose market the engine does not list.
    pub(super) unlisted: Vec<String>,
    /// Symbols whose contract the market's listing does not name: a contract on a market not priced
    /// from futures, none on one that is, or one its listing never names.
    pub(super) unnamed: Vec<String>,
    /// Symbols for a contract the listing names but the market does not take now: one it has rolled
    /// off, or one beyond the next.
    pub(super) idle: Vec<String>,
    /// The trading hours of each market asked about whose hours differ from those the task holds.
    pub(super) schedules: BTreeMap<MarketId, Schedule>,
}

impl VenueFeed {
    /// Asks the sequencer, behind the quotes already sent, about every symbol of the connection. A
    /// symbol counts as quoted when the engine lists its market, takes quotes for its contract, and
    /// the quote it holds from this venue passes `counts` at the time the sequencer would stamp
    /// next. Judging what the engine holds, not what was sent, means a quote a lagging sequencer
    /// applies late counts as stale.
    /// Error cases:
    /// - the sequencer has gone away
    pub(super) async fn confirm(&self) -> anyhow::Result<oneshot::Receiver<Confirmation>> {
        let (reply, answer) = oneshot::channel();
        let source = self.conn.venue.name();
        let stale_after = Millis::try_from(self.timing.stale_after.as_millis()).unwrap_or(Millis::MAX);
        let asked: Vec<(String, MarketId, Option<String>)> = self
            .instruments
            .iter()
            .map(|(symbol, instrument)| (symbol.clone(), instrument.market, instrument.contract.clone()))
            .collect();
        let known = Arc::clone(&self.schedules);
        let query: Query = Box::new(move |engine| {
            let at = now_ms().max(engine.now());
            let mut confirmation = Confirmation::default();
            for (symbol, market, contract) in asked {
                let Some(state) = engine.markets().get(&market) else {
                    confirmation.unlisted.push(symbol);
                    continue;
                };
                if known.get(&market) != Some(&state.spec.schedule) {
                    confirmation.schedules.entry(market).or_insert_with(|| state.spec.schedule.clone());
                }
                if !state.names_contract(contract.as_deref()) {
                    confirmation.unnamed.push(symbol);
                    continue;
                }
                let Some(held) = state.quotes_for(contract.as_deref()) else {
                    confirmation.idle.push(symbol);
                    continue;
                };
                let max_age_ms = state.spec.price_rules.max_age_ms;
                if held.get(source).is_some_and(|quote| counts(quote.ts, at, stale_after, max_age_ms)) {
                    confirmation.fresh.push(symbol);
                }
            }
            let _ = reply.send(confirmation);
        });
        self.tx.send(Request::Query(query)).await.map_err(|_| anyhow!("sequencer gone"))?;
        Ok(answer)
    }

    /// Records an answer: the hours that changed, the symbols quoted, the ones whose market the
    /// engine does not list or whose contract its listing does not name, alerted once each and
    /// held back from then on, and the idle ones, held back without an alert. A symbol held back
    /// that the answer leaves out is taken again with a full silence window, and a silent symbol
    /// that is held back is no longer silent.
    pub(super) fn confirmed(&mut self, confirmation: Confirmation) {
        if !confirmation.schedules.is_empty() {
            Arc::make_mut(&mut self.schedules).extend(confirmation.schedules);
        }
        let unlisted: BTreeSet<String> = confirmation.unlisted.into_iter().collect();
        let unnamed: BTreeSet<String> = confirmation.unnamed.into_iter().collect();
        let idle: BTreeSet<String> = confirmation.idle.into_iter().collect();
        let alerts = [
            ("venue_market_unlisted", "the engine does not list this market, so its quotes are held back"),
            (
                "venue_contract_unlisted",
                "the market's listing does not name this contract, so its quotes are held back",
            ),
        ];
        for ((alert, message), now_held) in alerts.into_iter().zip([unlisted, unnamed]) {
            self.hold_back(alert, message, now_held);
        }
        let taken: Vec<String> = self.idle.difference(&idle).cloned().collect();
        taken.into_iter().for_each(|symbol| self.take_again(symbol));
        self.idle = idle;
        for symbol in self.held().into_iter().filter(|symbol| self.silent.contains(symbol)).collect::<Vec<_>>() {
            self.silent.remove(&symbol);
            let market = self.instruments[&symbol].market;
            let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
            info!(
                alert_resolved = "venue_symbol_silent",
                venue, feed, symbol, market, "venue symbol no longer expected to quote"
            );
        }
        confirmation.fresh.iter().for_each(|symbol| self.delivered(symbol));
    }

    /// Settles what the ended connection sequenced, so one that quoted just before it ended is not
    /// taken for a failed attempt: one last confirmation, waiting at most `timing.timeout` for the
    /// answer.
    pub(super) async fn settle(&mut self) {
        let sent = !take(&mut self.unconfirmed).is_empty();
        let awaited = self.confirming.take().is_some();
        if !sent && !awaited {
            return;
        }
        let answered = async { anyhow::Ok(self.confirm().await?.await?) };
        if let Ok(Ok(confirmation)) = timeout(self.timing.timeout, answered).await {
            self.confirmed(confirmation);
        }
    }

    /// Sets the symbols `alert` holds back to `now_held`: alerts once for each symbol it newly holds
    /// back, and resolves each it releases, which is taken again.
    fn hold_back(&mut self, alert: &'static str, message: &str, now_held: BTreeSet<String>) {
        let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
        let held = self.held_back.entry(alert).or_default();
        let released: Vec<String> = held.difference(&now_held).cloned().collect();
        let added: Vec<String> = now_held.difference(held).cloned().collect();
        *held = now_held;
        for symbol in added {
            let market = self.instruments[&symbol].market;
            warn!(alert, venue, feed, symbol, market, "{message}");
        }
        for symbol in released {
            let market = self.instruments[&symbol].market;
            info!(alert_resolved = alert, venue, feed, symbol, market, "venue symbol taken again");
            self.last_quote.insert(symbol, Instant::now());
        }
    }

    /// Counts `symbol`'s silence afresh, as for one the engine starts taking quotes for again.
    fn take_again(&mut self, symbol: String) {
        self.last_quote.insert(symbol, Instant::now());
    }
}
