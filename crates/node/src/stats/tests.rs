use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::Event;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::StakingConfig;
use engine_core::types::{AccountId, MarketId, Millis, Side};
use engine_core::{Engine, Fixed};

use crate::sequencer::genesis_commands;
use crate::stats::Stats;
use crate::stats::series::Metric;
use crate::test_support::{dev_config, stats};

pub(crate) const T0: Millis = 1_792_504_800_000;
pub(crate) const BTC: MarketId = 1;
pub(crate) const TRADER: AccountId = AccountId([7; 20]);
pub(super) const OTHER: AccountId = AccountId([8; 20]);

/// The dev genesis with BTC priced at 100000, `TRADER` and `OTHER` funded with 100000 each, and a
/// record that has seen every command.
pub(crate) struct Rig {
    pub engine: Engine,
    pub stats: Stats,
}

impl Rig {
    /// Trading at `V23`, the last version that takes unsigned orders, with emissions from `T0`.
    pub(crate) fn new() -> Self {
        Self::at(ProtocolVersion::V23, false)
    }

    /// `V25`, with staking from `T0` over a day-long epoch, a floor of 1000 and a three-day cooldown.
    /// Its orders need signatures, so it trades nothing.
    pub(crate) fn staking() -> Self {
        Self::at(ProtocolVersion::V25, true)
    }

    fn at(version: ProtocolVersion, staking: bool) -> Self {
        let cfg = dev_config();
        let mut rig = Self { engine: Engine::new(), stats: stats() };
        let mut activation = vec![
            AdminCommand::SetClientAuth(engine_core::auth::ClientAuth::Unsigned),
            AdminCommand::SetOperator(cfg.operator().unwrap()),
            AdminCommand::ActivateProtocol { version: ProtocolVersion::V23 },
        ];
        if staking {
            let day = 86_400_000;
            activation.push(AdminCommand::ActivateProtocol { version });
            activation.push(AdminCommand::InitStaking(StakingConfig {
                liquidity_floor: Fixed::from_int(1_000),
                epoch_ms: day,
                start_ms: T0,
                cooldown_ms: 3 * day,
            }));
        }
        for cmd in activation.into_iter().map(Command::Admin) {
            rig.apply(T0, cmd);
        }
        let operator = cfg.operator().unwrap();
        let key = k256::ecdsa::SigningKey::from_slice(&[0x42; 32]).expect("the dev operator key");
        let schedule = EmissionSchedule { start_ms: T0, period_budget: TokenUnits(1_000_000) };
        let nonce = rig.engine.next_request_nonce(operator.address);
        let init = crate::admin::sign(&key, &operator, AdminCommand::InitEmissions(schedule), nonce, T0 + 60_000);
        let events = rig.apply(T0, Command::SignedAdmin(Box::new(init)));
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        let mut genesis = genesis_commands(&cfg.genesis);
        let capital = genesis.pop().expect("genesis credits house capital");
        for cmd in genesis.into_iter().chain([capital]) {
            rig.apply(T0, cmd);
        }
        for account in [TRADER, OTHER] {
            let chain_ref = format!("dep-{account}");
            rig.apply(T0, Command::Deposit { account, amount: Fixed::from_int(100_000), chain_ref });
        }
        rig.price(T0, 100_000);
        rig
    }

    pub(crate) fn apply(&mut self, ts: Millis, cmd: Command) -> Vec<Event> {
        let seq = self.engine.last_seq().map_or(0, |seq| seq + 1);
        let events = self.engine.apply(&Sequenced { seq, ts, cmd }).unwrap();
        self.stats.observe(&self.engine, ts, &events);
        events
    }

    pub(crate) fn price(&mut self, ts: Millis, mid: i64) -> Vec<Event> {
        let quotes = ["a", "b"]
            .map(|source| SourceQuote { source: source.into(), mid: Fixed::from_int(mid), ts, contract: None })
            .to_vec();
        self.apply(ts, Command::Prices { market: BTC, quotes })
    }

    pub(crate) fn trade(&mut self, ts: Millis, account: AccountId, side: Side, qty: i64) -> Vec<Event> {
        let order = OrderRequest {
            account,
            client_id: ts as u64,
            market: BTC,
            side,
            qty: Fixed::from_int(qty),
            kind: OrderKind::Market { quoted: self.engine.markets()[&BTC].mark.unwrap(), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        };
        self.apply(ts, Command::PlaceOrder(order))
    }
}

#[test]
fn a_fill_adds_its_notional_to_the_market_and_total_volume() {
    let mut rig = Rig::new();
    let events = rig.trade(T0 + 1_000, TRADER, Side::Buy, 2);
    assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    let page = |market| {
        let query = crate::stats::SeriesQuery {
            metric: "volume".into(),
            market,
            resolution: "1m".into(),
            from_ms: None,
            to_ms: None,
            limit: None,
        };
        serde_json::to_value(rig.stats.series(&rig.engine, &query).unwrap()).unwrap()
    };
    let (all, btc) = (page(None), page(Some(BTC)));
    assert_eq!(all["entries"], btc["entries"]);
    let value: Fixed = btc["entries"][0]["value"].as_str().unwrap().parse().unwrap();
    // Two BTC filled within the spread of the 100000 index.
    assert!(value > Fixed::from_int(199_000) && value < Fixed::from_int(201_000), "{value}");
    assert_eq!(btc["kind"], "flow");
    assert_eq!(btc["from_genesis"], true);
}

#[test]
fn nav_is_a_level_whose_bucket_holds_the_open_high_low_and_close() {
    let mut rig = Rig::new();
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 2);
    rig.price(T0 + 2_000, 101_000);
    rig.price(T0 + 3_000, 99_000);
    let query = |resolution: &str| crate::stats::SeriesQuery {
        metric: "nav".into(),
        market: None,
        resolution: resolution.into(),
        from_ms: Some(T0),
        to_ms: Some(T0 + 59_999),
        limit: None,
    };
    let page = serde_json::to_value(rig.stats.series(&rig.engine, &query("1m")).unwrap()).unwrap();
    let bucket = &page["entries"][0];
    let nav = |key: &str| bucket[key].as_str().unwrap().parse::<Fixed>().unwrap();
    assert_eq!(nav("close"), rig.engine.house_nav());
    // The client is long two BTC: the house gains when the index falls, so its NAV peaks at 99000.
    assert!(nav("high") > nav("open") && nav("low") < nav("high"), "{bucket}");
    assert_eq!(page["unit"], "usd");
    assert!(rig.stats.series(&rig.engine, &query("1s")).is_err());
}

#[test]
fn a_series_names_where_its_data_begins_and_pages_oldest_first() {
    let mut rig = Rig::new();
    for step in 1..=4 {
        rig.price(T0 + step * 60_000, 100_000 + step);
    }
    let query = |from_ms, limit| crate::stats::SeriesQuery {
        metric: "cash".into(),
        market: None,
        resolution: "1m".into(),
        from_ms,
        to_ms: None,
        limit,
    };
    let first = serde_json::to_value(rig.stats.series(&rig.engine, &query(None, Some(2))).unwrap()).unwrap();
    assert_eq!(first["begins_seq"], 0);
    assert_eq!(first["begins_ms"], T0);
    assert_eq!(first["entries"].as_array().unwrap().len(), 2);
    let next = first["next"].as_i64().unwrap();
    let second = serde_json::to_value(rig.stats.series(&rig.engine, &query(Some(next), Some(2))).unwrap()).unwrap();
    assert_eq!(second["entries"][0]["ts"], next);
    assert!(second["entries"][0]["ts"].as_i64() > first["entries"][1]["ts"].as_i64());
    assert!(rig.stats.series(&rig.engine, &query(None, Some(0))).is_err());
    let _ = Metric::Cash;
}

#[test]
fn a_series_rejects_a_market_it_does_not_take_and_one_it_needs() {
    let rig = Rig::new();
    let ask = |metric: &str, market| {
        let query = crate::stats::SeriesQuery {
            metric: metric.into(),
            market,
            resolution: "1h".into(),
            from_ms: None,
            to_ms: None,
            limit: None,
        };
        rig.stats.series(&rig.engine, &query).map(|_| ())
    };
    assert!(ask("long", None).is_err());
    assert!(ask("nav", Some(BTC)).is_err());
    assert!(ask("long", Some(777)).is_err());
    assert!(ask("volume", None).is_ok() && ask("long", Some(BTC)).is_ok());
    assert!(ask("bogus", None).unwrap_err().contains("nav"));
}

mod closeout_map;
mod monkey;
mod replay;
