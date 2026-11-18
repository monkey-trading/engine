//! The node's outside world on the node's side of the run: a loopback stand-in for every venue and
//! vendor the production template connects to, every connection the template opens pointed at one,
//! and the signed screening source the template's `[access]` names.
//!
//! The crypto stand-ins speak Coinbase's Advanced Trade `level2`, Coinbase Exchange's
//! `level2_batch` and Kraken's v2 `book`, sending each symbol's whole book `CRYPTO_UPDATES` times a
//! second; the others are the RWA scenarios' own (`feeds::tests`: Massive, TraderMade, CME Group's
//! token endpoint and stream, and Alpaca, the last two behind TLS). Every market's mid is the book's
//! mark of 100 and half a basis point off it in turn (`mid`), moving up in odd markets and down in
//! even ones, so it never reaches the book's in-band orders, which buy in odd markets and sell in even
//! ones (`common::launch_book`). A connection sequences a price each time its mid moves, so a
//! market's price commands a second are its connections' moves a second.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::routing::post;
use axum::{Json, Router};
use engine_core::Fixed;
use engine_core::access::{Jurisdiction, SanctionsStatus, Screening};
use engine_core::eip712::{self, Signature};
use engine_core::types::{MarketId, Millis};
use futures_util::{SinkExt, StreamExt};
use rustls::ClientConfig;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::task::AbortHandle;
use tokio::time::{MissedTickBehavior, interval};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::common::close_spread::{source, source_key};
use crate::common::{CHAIN_ID, VAULT};
use crate::config::{AccessSection, Venue, VenueConnection, VenuesSection};
use crate::feeds::tests::alpaca_support::AlpacaStandIn;
use crate::feeds::tests::cme_support::{CmeStandIn, key};
use crate::feeds::tests::fake::{coinbase_exchange_book, coinbase_levels, kraken_book, listen};
use crate::feeds::tests::rwa_support::TlsFront;
use crate::feeds::tests::vendors::{KEY, Prices, serve_massive, serve_tradermade};
use crate::feeds::venue::{self, Credentials, Timing};

/// The production template, whose connections, feed timings and screening slots the run uses.
pub(super) const TEMPLATE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/runbooks/production-node.toml");

/// How often a crypto stand-in sends each symbol's book, about the rate Coinbase updates BTC-USD's.
const CRYPTO_UPDATES: f64 = 25.0;
/// Each side of a stand-in's book from its mid: a basis point of the mark.
const HALF_SPREAD: &str = "0.01";
/// How far a mid moves from the mark: half a basis point of it.
const MOVE: &str = "0.005";
/// A CME stand-in token's life, CME's own.
const TOKEN_LIFE_SECS: u64 = 1_799;
/// A ping interval and delay bound no run reaches, for a connection to a stand-in that answers no
/// ping.
const UNPROBED: Duration = Duration::from_secs(24 * 60 * 60);

/// How often each connection's mids move, in moves a second.
#[derive(Clone, Copy, Debug)]
pub(super) struct Moves {
    pub crypto: f64,
    pub rwa: f64,
}

/// A feed connection to a stand-in: the template's, pointed at the stand-in, with its credentials,
/// TLS config and timings.
pub(super) type Connection = (VenueConnection, Credentials, Arc<ClientConfig>, Timing);

/// The stand-ins, and the feed connections to them.
pub(super) struct Venues {
    pub connections: Vec<Connection>,
    /// The screening source's `[access]`.
    pub access: AccessSection,
    tasks: Vec<AbortHandle>,
    cme: CmeStandIn,
    _alpaca: AlpacaStandIn,
}

impl Drop for Venues {
    fn drop(&mut self) {
        self.tasks.iter().for_each(AbortHandle::abort);
        self.cme.stop();
    }
}

/// The template's `[feeds.venues]`.
pub(super) fn template_venues() -> VenuesSection {
    template()["feeds"]["venues"].clone().try_into().expect("the template's [feeds.venues]")
}

/// The production template, parsed.
pub(super) fn template() -> toml::Value {
    toml::from_str(&std::fs::read_to_string(TEMPLATE).expect("the production template")).expect("TOML")
}

/// `market`'s mid at `ts` when it moves `per_second` times a second: the mark, and `MOVE` off it in
/// turn, up in an odd market and down in an even one.
pub(super) fn mid(market: MarketId, per_second: f64, ts: Millis) -> Fixed {
    let mark = Fixed::from_int(100);
    let period_ms = (1_000.0 / per_second.max(0.001)) as Millis;
    if (ts / period_ms.max(1)) % 2 == 0 {
        return mark;
    }
    let moved: Fixed = MOVE.parse().expect("a decimal");
    if market % 2 == 1 { mark + moved } else { mark - moved }
}

/// A book around `market`'s mid at `ts`.
fn book(market: MarketId, per_second: f64, ts: Millis) -> (Fixed, Fixed) {
    let (mid, half) = (mid(market, per_second, ts), HALF_SPREAD.parse::<Fixed>().expect("a decimal"));
    (mid - half, mid + half)
}

/// Starts every stand-in and the screening source, and points each of the template's connections at
/// its stand-in, at the template's feed timings. The RWA stand-ins read nothing once they stream, so
/// they answer no ping: their connections send none, as the RWA scenarios' do, and only the crypto
/// connections hold their streams to the template's delay bound.
pub(super) async fn start(moves: Moves) -> Venues {
    let venues = template_venues();
    let timing = Timing::for_venues(&venues);
    let tls = TlsFront::new();
    let plain = venue::tls_config().expect("tls config");
    let mut tasks = Vec::new();
    // Each symbol a venue quotes, and its market, from every connection to it.
    let mut symbols: BTreeMap<Venue, BTreeMap<String, MarketId>> = BTreeMap::new();
    for conn in &venues.connections {
        let quoted = symbols.entry(conn.venue).or_default();
        quoted.extend(conn.symbols.iter().map(|symbol| (symbol.symbol.clone(), symbol.market)));
    }
    let prices = |venue: Venue| -> Prices {
        let quoted = symbols.get(&venue).cloned().unwrap_or_default();
        Arc::new(move |symbol: &str, ts: Millis| Some(book(*quoted.get(symbol)?, moves.rwa, ts)))
    };
    let mut urls: BTreeMap<Venue, String> = BTreeMap::new();
    for venue in [Venue::Coinbase, Venue::CoinbaseExchange, Venue::Kraken] {
        let (listener, url) = listen().await;
        let quoted = symbols.get(&venue).cloned().unwrap_or_default();
        tasks.push(tokio::spawn(serve_crypto(listener, venue, quoted, moves.crypto)).abort_handle());
        urls.insert(venue, url);
    }
    let (massive, massive_url) = listen().await;
    tasks.push(serve_massive(massive, prices(Venue::Massive)).abort_handle());
    let (tradermade, tradermade_url) = listen().await;
    tasks.push(serve_tradermade(tradermade, prices(Venue::Tradermade)).abort_handle());
    let cme = CmeStandIn::start(&tls, TOKEN_LIFE_SECS, prices(Venue::Cme)).await;
    let alpaca = AlpacaStandIn::start(&tls, prices(Venue::Alpaca)).await;
    let connections = venues
        .connections
        .into_iter()
        .map(|conn| {
            let path = conn.url.parse::<axum::http::Uri>().map(|uri| uri.path().to_owned()).unwrap_or_default();
            let unprobed = Timing { ping_interval: UNPROBED, max_delay: UNPROBED, ..timing };
            let (url, credentials, config) = match conn.venue {
                Venue::Massive => (format!("{massive_url}{path}"), Credentials::ApiKey(key(KEY)), plain.clone()),
                Venue::Tradermade => (tradermade_url.clone(), Credentials::ApiKey(key(KEY)), plain.clone()),
                Venue::Cme => {
                    (cme.stream_url.clone(), Credentials::OAuth(cme.client(&tls, timing.timeout)), tls.client.clone())
                }
                Venue::Alpaca => (alpaca.url.clone(), alpaca.credentials(), tls.client.clone()),
                venue => (urls[&venue].clone(), Credentials::Public, plain.clone()),
            };
            let probed = matches!(conn.venue, Venue::Coinbase | Venue::CoinbaseExchange | Venue::Kraken);
            let timing = if probed { timing } else { unprobed };
            let conn =
                VenueConnection { url, api_key_env: None, api_key_secret: None, oauth: None, key_pair: None, ..conn };
            (conn, credentials, config, timing)
        })
        .collect();
    let (access, source) = start_access().await;
    tasks.push(source);
    Venues { connections, access, tasks, cme, _alpaca: alpaca }
}

/// Serves `venue`'s books of `symbols` to every connection a feed opens, each until it closes: every
/// symbol's whole book `CRYPTO_UPDATES` times a second, or twice as often as its mid moves if that is
/// more, so each move is sent.
async fn serve_crypto(listener: TcpListener, venue: Venue, symbols: BTreeMap<String, MarketId>, moves: f64) {
    let every = Duration::from_secs_f64(1.0 / CRYPTO_UPDATES.max(2.0 * moves));
    while let Ok((tcp, _)) = listener.accept().await {
        let symbols = symbols.clone();
        tokio::spawn(async move {
            let Ok(mut ws) = accept_async(tcp).await else { return };
            let mut ticks = interval(every);
            ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
            let mut sequence = 0u64;
            loop {
                tokio::select! {
                    // The feed's subscriptions and pings, read and answered as tungstenite reads them.
                    read = ws.next() => if !matches!(read, Some(Ok(_))) { return },
                    _ = ticks.tick() => {
                        let ts = now_ms();
                        for (symbol, market) in &symbols {
                            let (bid, ask) = book(*market, moves, ts);
                            let (bid, ask) = (bid.to_string(), ask.to_string());
                            let text = match venue {
                                Venue::Coinbase => coinbase_levels(sequence, true, symbol, &bid, &ask, ts),
                                Venue::CoinbaseExchange => coinbase_exchange_book(symbol, &bid, &ask, ts),
                                _ => kraken_book(symbol, &bid, &ask, ts),
                            };
                            sequence += 1;
                            if ws.send(Message::text(text)).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
    }
}

/// Starts the signed screening source, and the template's `[access]` pointed at it under the book's
/// source key.
async fn start_access() -> (AccessSection, AbortHandle) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let endpoint = format!("http://{}", listener.local_addr().expect("local addr"));
    let mut section = template()["access"].clone();
    let table = section.as_table_mut().expect("the template's [access]");
    table.insert("source".into(), toml::Value::String(format!("{}", source_key())));
    table.insert("endpoint".into(), toml::Value::String(endpoint));
    let access: AccessSection = section.try_into().expect("the template's [access], pointed at the stand-in");
    access.validate().expect("a valid [access]");
    let app = Router::new().route("/", post(screen));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.expect("the screening source") });
    (access, task.abort_handle())
}

/// Answers a screening request clear, with evidence the book's source key signs now.
async fn screen(Json(request): Json<Value>) -> Json<Screening> {
    let mut proof = Screening {
        source: source_key(),
        request_hash: serde_json::from_value(request["request_hash"].clone()).expect("a request hash"),
        observed_at: now_ms(),
        account: serde_json::from_value(request["account"].clone()).expect("an account"),
        payer: serde_json::from_value(request["payer"].clone()).expect("a payer"),
        jurisdiction: Jurisdiction::Allowed,
        account_status: SanctionsStatus::Clear,
        payer_status: SanctionsStatus::Clear,
        signature: Signature::ZERO,
    };
    proof.signature = eip712::sign(&source(), &eip712::screening_hash(&eip712::domain(CHAIN_ID, VAULT), &proof));
    Json(proof)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mid_moves_away_from_the_books_in_band_orders_at_its_rate() {
        let (mark, moved) = (Fixed::from_int(100), "0.005".parse::<Fixed>().unwrap());
        // Four moves a second: 250 ms at the mark, then 250 ms off it.
        assert_eq!(mid(1, 4.0, 1_000), mark);
        assert_eq!(mid(1, 4.0, 1_250), mark + moved, "up in an odd market, away from its in-band buys");
        assert_eq!(mid(2, 4.0, 1_250), mark - moved, "down in an even one, away from its in-band sells");
        assert_eq!(mid(2, 4.0, 1_500), mark);
        let (bid, ask) = book(3, 4.0, 1_250);
        assert_eq!((bid, ask), ("99.995".parse().unwrap(), "100.015".parse().unwrap()));
    }
}
