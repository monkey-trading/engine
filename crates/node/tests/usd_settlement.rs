//! The USD settlement dev scenario on the committed dev config. A Germany 40 position, quoted in
//! euros, is valued in USD at EUR-USD's index: EUR-USD moves while Germany 40 holds still, then goes
//! stale, and Germany 40 deals again once EUR-USD prices. Commands carry fixed timestamps, so the
//! markets' hours hold whenever the check runs.

use std::path::Path;

use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::event::{Event, Fill, Reject};
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Millis, Side};
use engine_core::{Engine, Fixed};
use engine_node::NodeConfig;
use engine_node::sequencer::genesis_commands;

const EURUSD: MarketId = 10;
const DE40: MarketId = 23;
const TRADER: AccountId = AccountId([7; 20]);
/// Tue 20 Oct 2026 10:00 Berlin: Germany 40 and EUR-USD are both open.
const START: Millis = 1_792_483_200_000;

#[test]
fn dev_scenario_germany_40_pnl_moves_with_eur_usd_alone_and_stops_while_eur_usd_is_stale() {
    let mut node = DevNode::genesis();
    node.apply(Command::Deposit { account: TRADER, amount: f("100000"), chain_ref: "dev:1".into() });
    node.price(EURUSD, "1.08");
    node.price(DE40, "23500");
    let fill = node.order(Side::Buy, "2", "23500").expect("the long opens");
    assert_eq!((fill.price, fill.fx_index), (f("23500"), Some(f("1.08"))));
    node.price(DE40, "23600");

    // EUR-USD moves while Germany 40 holds at 23,600: the 200 EUR of PnL, and 0.5% margin on
    // 47,200 EUR, move with EUR-USD alone.
    for (fx, pnl, margin) in [("1.08", "216", "254.88"), ("1.1", "220", "259.6"), ("1.05", "210", "247.8")] {
        node.price(EURUSD, fx);
        node.price(DE40, "23600");
        assert_eq!(node.pnl_and_margin(), (f(pnl), f(margin)), "EUR-USD at {fx}");
    }

    // EUR-USD goes stale: Germany 40 deals neither way, and the position keeps its last USD value.
    node.ts += 1_500;
    node.price(DE40, "23600");
    for side in [Side::Buy, Side::Sell] {
        assert_eq!(node.order(side, "2", "23600").unwrap_err(), Reject::NoFxPrice { fx_market: EURUSD });
    }
    assert_eq!(node.pnl_and_margin(), (f("210"), f("247.8")));

    // EUR-USD prices again, and closing realises the 200 EUR at that index.
    node.price(EURUSD, "1.06");
    let close = node.order(Side::Sell, "2", "23600").expect("the long closes");
    assert_eq!((close.realised_pnl, close.fx_index), (f("212"), Some(f("1.06"))));
}

/// An engine built from the dev config's genesis, applying commands at a clock the test moves.
struct DevNode {
    engine: Engine,
    ts: Millis,
}

impl DevNode {
    /// Boots as a fresh dev node does at `START`: records its client auth and operator, activates the
    /// latest protocol, and sequences the committed dev genesis.
    fn genesis() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/dev.toml");
        let config = NodeConfig::load(&path).expect("the dev config loads");
        let mut node = DevNode { engine: Engine::new(), ts: START };
        let auth = config.client_auth().expect("the dev config's client auth");
        let boot = [
            Command::Admin(AdminCommand::SetClientAuth(auth)),
            Command::Admin(AdminCommand::SetOperator(config.operator().expect("the dev config's operator"))),
            Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST }),
        ];
        for cmd in boot.into_iter().chain(genesis_commands(&config.genesis)) {
            let events = node.apply(cmd);
            assert!(!events.iter().any(|e| matches!(e, Event::AdminRejected { .. })), "{events:?}");
        }
        node
    }

    /// Applies `cmd` at the current time.
    fn apply(&mut self, cmd: Command) -> Vec<Event> {
        let seq = self.engine.last_seq().map_or(0, |s| s + 1);
        self.engine.apply(&Sequenced { seq, ts: self.ts, cmd }).expect("in sequence")
    }

    /// Three mock venues quoting `mid` now.
    fn price(&mut self, market: MarketId, mid: &str) {
        let quotes = ["mock-a", "mock-b", "mock-c"]
            .into_iter()
            .map(|source| SourceQuote { source: source.into(), mid: f(mid), ts: self.ts, contract: None })
            .collect();
        self.apply(Command::Prices { market, quotes });
    }

    /// A Germany 40 market order for the trader: its fill, or the rejection.
    fn order(&mut self, side: Side, qty: &str, quoted: &str) -> Result<Fill, Reject> {
        let kind = OrderKind::Market { quoted: f(quoted), max_slippage_bps: None };
        let req = OrderRequest {
            account: TRADER,
            client_id: 1,
            market: DE40,
            side,
            qty: f(qty),
            kind,
            reduce_only: false,
            auth: None,
        };
        let events = self.apply(Command::PlaceOrder(req));
        events
            .into_iter()
            .find_map(|e| match e {
                Event::Fill(fill) => Some(Ok(fill)),
                Event::OrderRejected { reason, .. } => Some(Err(reason)),
                _ => None,
            })
            .expect("a fill or a rejection")
    }

    /// The trader's unrealised PnL and margin requirement in USD, as the account API reports them.
    fn pnl_and_margin(&self) -> (Fixed, Fixed) {
        let acct = &self.engine.accounts()[&TRADER];
        let (equity, margin) = self.engine.equity_and_margin(acct, None);
        (equity - acct.balance, margin)
    }
}

/// Parses a decimal.
fn f(s: &str) -> Fixed {
    s.parse().unwrap()
}
