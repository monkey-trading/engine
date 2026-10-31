//! The RWA price feeds' dev scenario offline: loopback Massive, TraderMade, and Databento vendors
//! price an FX pair, an index and a commodity from front futures contracts, and a US share through
//! the sequencer, two vendors each. The share's market is closed while the vendors start quoting
//! it; once setup is done, a signed relisting opens it two seconds on, and another rolls the index
//! onto its next contract. The log then replays with `engine replay`'s checks.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::{DateTime, Utc};
use engine_core::Fixed;
use engine_core::command::{Command, OrderKind};
use engine_core::event::Event;
use engine_core::futures::FuturesContract;
use engine_core::market::MarketSpec;
use engine_core::types::{MarketId, Millis};
use tokio::net::TcpListener;
use tokio::task::{AbortHandle, JoinHandle};

use crate::clock::now_ms;
use crate::config::{ApiKey, Genesis, Venue, VenueConnection, VenueSymbol};
use crate::feeds::tests::fake::{WAIT, fast, listen};
use crate::feeds::tests::rwa_support::{Applied, around_the_clock, events_of, listing, priced, wait_for};
use crate::feeds::tests::scenario::Node;
use crate::feeds::tests::vendors::{KEY, Prices, serve_databento, serve_massive, serve_tradermade};
use crate::feeds::venue::{self, tls_config};
use crate::replay_path;
use crate::test_support::dev_config;

/// The dev genesis markets the scenario prices: EUR-USD, US 500, gold, and Apple.
const EURUSD: MarketId = 10;
const US500: MarketId = 20;
const GOLD: MarketId = 30;
const AAPL: MarketId = 40;
/// Far enough ahead that setup ends well before it, however long setup takes.
const LATER_MS: Millis = 600_000;

#[tokio::test]
#[ignore = "dev scenario, run by scripts/dev-rwa-feeds.sh"]
async fn dev_scenario_rwa_price_feeds() {
    let start = now_ms();
    let open_at = Arc::new(AtomicI64::new(start + LATER_MS));
    let apple = |open: Millis| MarketSpec { schedule: around_the_clock(Some((start - 60_000, open))), ..listing(AAPL) };
    let us500 = |roll_at: Millis| {
        let contracts = vec![contract("ESZ6", start - 86_400_000), contract("ESH7", roll_at)];
        MarketSpec { schedule: around_the_clock(None), contracts, ..listing(US500) }
    };
    let gold = MarketSpec {
        schedule: around_the_clock(None),
        contracts: vec![contract("GCZ6", start - 86_400_000)],
        ..listing(GOLD)
    };
    let eurusd = MarketSpec { schedule: around_the_clock(None), ..listing(EURUSD) };
    let genesis = genesis(vec![eurusd, us500(start + LATER_MS), gold, apple(start + LATER_MS)]);
    let mut node = Node::boot("rwa", &genesis).await;
    let applied = node.record();
    let vendors = start_vendors(&node, &open_at).await;

    // Each market prices from its two vendors: the fill is at the median of their mids.
    let both = [(EURUSD, "massive"), (EURUSD, "tradermade"), (US500, "massive"), (US500, "databento")];
    node.wait_for_quotes(&[both.as_slice(), &[(GOLD, "massive"), (GOLD, "databento")]].concat()).await;
    node.deposit().await;
    for (market, mid) in [(EURUSD, "1.08002"), (US500, "6000.6"), (GOLD, "2650.3")] {
        assert_eq!(fill_index(&node.buy(market, mid).await), Some(mid.parse().unwrap()), "market {market}");
    }

    // A buy limit above every Apple quote rests while its market is closed. The vendors quote it
    // throughout, at 200 before its open and 230 after, and only a quote after the open fills it.
    let limit = OrderKind::Limit { price: "240".parse().unwrap() };
    let events = node.buy_kind(AAPL, limit).await;
    assert!(matches!(events[..], [Event::OrderAccepted { .. }]), "{events:?}");
    let opens = now_ms() + 2_000;
    open_at.store(opens, Ordering::SeqCst);
    node.relist(apple(opens)).await;
    let fills = wait_for(WAIT, || Some(filled(&applied, AAPL)).filter(|fills| !fills.is_empty())).await;
    assert_eq!(fills.iter().map(|(_, index)| *index).collect::<Vec<_>>(), ["230.11".parse().unwrap()], "{fills:?}");
    assert!(fills[0].0 >= opens, "filled at {} before the open at {opens}", fills[0].0);
    assert!(priced(&applied, AAPL).iter().all(|(ts, _)| *ts >= opens), "Apple priced before its open");

    // US 500 rolls onto March, 40 above December, without its index moving.
    node.relist(us500(now_ms() + 500)).await;
    let rolled = Event::MarketRolled { market: US500, contract: "ESH7".into(), adjustment: "-40".parse().unwrap() };
    wait_for(WAIT, || events_of(&applied).contains(&rolled).then_some(())).await;
    assert_eq!(fill_index(&node.buy(US500, "6000.6").await), Some("6000.6".parse().unwrap()));

    // No quote reached the engine for it to refuse: none from outside its market's hours, and
    // none for a contract it does not take.
    let refused: Vec<Event> =
        events_of(&applied).into_iter().filter(|event| matches!(event, Event::PricesRejected { .. })).collect();
    assert!(refused.is_empty(), "{refused:?}");

    // With the feeds stopped, the log replays: every root, signature, and event checks.
    vendors.into_iter().for_each(|task| task.abort());
    node.submit(Command::Tick).await;
    let replayed = replay_path(&node.dir).expect("the log replays");
    assert!(replayed.commands > 0 && replayed.checkpoints > 0);
}

// ── Helpers ──

/// The dev genesis's crypto markets and `markets`.
fn genesis(markets: Vec<MarketSpec>) -> Genesis {
    let mut genesis = dev_config().genesis;
    genesis.markets.retain(|market| [1, 2].contains(&market.id));
    genesis.close_spread_ceilings.retain(|ceiling| [1, 2].contains(&ceiling.market));
    genesis.markets.extend(markets);
    genesis
}

/// A contract rolling in at `from`.
fn contract(name: &str, from: Millis) -> FuturesContract {
    FuturesContract { contract: name.into(), from: DateTime::<Utc>::from_timestamp_millis(from).expect("in range") }
}

/// Each vendor's books: Databento's 0.2 above Massive's and TraderMade's 0.00002 above, Apple 30
/// higher from `open_at`. Two vendors' medians are the mean of their mids.
fn prices(venue: Venue, open_at: Arc<AtomicI64>) -> Prices {
    Arc::new(move |symbol, ts| {
        let (bid, ask) = match symbol {
            "EUR-USD" | "EURUSD" => ("1.08000", "1.08002"),
            "ESZ6" => ("6000", "6001"),
            "ESH7" => ("6040", "6041"),
            "GCZ6" => ("2650", "2650.4"),
            "AAPL" if ts < open_at.load(Ordering::SeqCst) => ("200", "200.02"),
            "AAPL" => ("230", "230.02"),
            _ => return None,
        };
        let shift = match venue {
            Venue::Tradermade => "0.00002",
            Venue::Databento => "0.2",
            _ => "0",
        };
        let at = |price: &str| price.parse::<Fixed>().expect("a price") + shift.parse::<Fixed>().expect("a shift");
        Some((at(bid), at(ask)))
    })
}

/// Starts the loopback vendors and the six feeds that read them: Massive's FX, futures, and stocks
/// feeds, TraderMade, and Databento's CME futures and US equities datasets. Aborting the returned
/// tasks stops them all.
async fn start_vendors(node: &Node, open_at: &Arc<AtomicI64>) -> Vec<AbortHandle> {
    let (massive, massive_url) = listen().await;
    let (tradermade, tradermade_url) = listen().await;
    let databento = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let databento_url = format!("tcp://{}", databento.local_addr().expect("local addr"));
    let futures = [(US500, "ESZ6", Some("ESZ6")), (US500, "ESH7", Some("ESH7")), (GOLD, "GCZ6", Some("GCZ6"))];
    let feeds = [
        (Venue::Massive, format!("{massive_url}/forex"), None, vec![(EURUSD, "EUR-USD", None)]),
        (Venue::Massive, format!("{massive_url}/futures"), None, futures.to_vec()),
        (Venue::Massive, format!("{massive_url}/stocks"), None, vec![(AAPL, "AAPL", None)]),
        (Venue::Tradermade, tradermade_url, None, vec![(EURUSD, "EURUSD", None)]),
        (Venue::Databento, databento_url.clone(), Some("GLBX.MDP3"), futures.to_vec()),
        (Venue::Databento, databento_url, Some("EQUS.MINI"), vec![(AAPL, "AAPL", None)]),
    ];
    let mut tasks: Vec<AbortHandle> = [
        serve_massive(massive, prices(Venue::Massive, open_at.clone())),
        serve_tradermade(tradermade, prices(Venue::Tradermade, open_at.clone())),
        serve_databento(databento, prices(Venue::Databento, open_at.clone())),
    ]
    .iter()
    .map(JoinHandle::abort_handle)
    .collect();
    for (venue, url, dataset, symbols) in feeds {
        let symbols = symbols
            .into_iter()
            .map(|(market, symbol, contract)| VenueSymbol {
                market,
                symbol: symbol.into(),
                contract: contract.map(Into::into),
            })
            .collect();
        let api_key = Some(ApiKey::from_env("VENDOR_API_KEY", |_| Some(KEY.into())).expect("a key"));
        let api_key_env = Some("VENDOR_API_KEY".into());
        let conn = VenueConnection { venue, url, api_key_env, dataset: dataset.map(Into::into), symbols };
        let feed =
            tokio::spawn(venue::run(conn, api_key, fast(WAIT), tls_config().expect("tls config"), node.tx.clone()));
        tasks.push(feed.abort_handle());
    }
    tasks
}

/// The stamp and index of each fill in `market`.
fn filled(applied: &Applied, market: MarketId) -> Vec<(Millis, Fixed)> {
    let applied = applied.lock().expect("recorder");
    applied
        .iter()
        .flat_map(|(ts, _, events)| events.iter().map(move |event| (*ts, event)))
        .filter_map(|(ts, event)| match event {
            Event::Fill(fill) if fill.market == market => Some((ts, fill.index)),
            _ => None,
        })
        .collect()
}

/// The index of the fill among `events`, if any.
fn fill_index(events: &[Event]) -> Option<Fixed> {
    events.iter().find_map(|event| if let Event::Fill(fill) = event { Some(fill.index) } else { None })
}
