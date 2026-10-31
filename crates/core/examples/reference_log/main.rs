//! Writes a protocol reference log: a session through every command and main path, with each command's state
//! root. Run once per protocol version and commit the output; it refuses to overwrite. Usage:
//! `cargo run -p engine-core --example reference_log -- crates/core/tests/reference/v<N>.jsonl`

#[path = "../../tests/common/mod.rs"]
mod common;
mod helpers;
mod stress;
mod v10;
mod v11;
mod v12;
mod v14;
mod v15;
mod v16;
mod v17;
mod v18;
mod v19;
mod v20;
mod v21;
mod v22;
mod v23;
mod v24;
mod v25;
mod v26;
mod v27;
mod v28;
mod v29;
mod v30;
mod v31;
mod v32;
mod v33;
mod v34;
mod v35;
mod v37;
mod v38;
mod v39;
use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::anyhow;
use chrono::NaiveDate;
use common::reference::{ReferenceEntry, root_hex};
use common::{
    AAPL, BTC, Client, DE40, ETH, EURUSD, Harness, OPERATOR_SEED, UNLISTED, USDJPY, acct, de40, f, market, ny,
    operator, params, quote, us_equities,
};
use engine_core::Engine;
use engine_core::auth::MAX_EXPIRY_AHEAD_MS;
use engine_core::command::{AdminCommand, Command, OrderKind, Sequenced};
use engine_core::market::{MarketSpec, QuoteCurrency, Tier};
use engine_core::params::{ProtocolParams, ToxicityParams};
use engine_core::price::{MAX_QUOTE_LEAD_MS, SourceQuote};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::{Period, Schedule};
use engine_core::types::{AssetClass, MarketId, Side};
use helpers::{accepted, event_kind, reducing, small_buy, write_entries};

const SOL: MarketId = 10;
const GOLD: MarketId = 11;
const OIL: MarketId = 12;

/// Writes the session to the given path and prints the event counts.
/// Fails if the path is missing, already exists, or cannot be written.
fn main() -> anyhow::Result<()> {
    let path: PathBuf =
        std::env::args_os().nth(1).ok_or_else(|| anyhow!("usage: reference_log <output.jsonl>"))?.into();
    let mut engine = Engine::new();
    let mut tally: BTreeMap<String, usize> = BTreeMap::new();
    let mut entries = Vec::new();
    for sequenced in session() {
        for event in engine.apply(&sequenced)? {
            *tally.entry(event_kind(&event)).or_default() += 1;
        }
        entries.push(ReferenceEntry { sequenced, root: root_hex(engine.state_root()) });
    }
    write_entries(&path, &entries)?;
    println!("wrote {} commands to {}", entries.len(), path.display());
    tally.iter().for_each(|(kind, count)| println!("{count:>5} {kind}"));
    Ok(())
}

/// The scripted session: client auth and activation, genesis, funding, prices,
/// every order kind, signed requests refused for each auth reason, triggers (one held back by its spread), cancels,
/// reduce-only (one clipped to the position), stress, toxicity, a position flip, house capital, withdrawals, a session
/// close and gapped open, financing rollovers over longs and a short, a two-position close-out with bad debt, two-band
/// margin, an exchange holiday, a zero house capital move, an early close and a special session, admin changes, venue
/// quotes stamped ahead of the sequenced time, and dust adds to a long and a short held over a rollover, with a
/// rejection of each kind along the way. Then, under `V6`, close-outs after a margin-table rise, a close-out level
/// rise, and a rollover's financing, house cap floors at a drawn-down NAV, and a close-out just after a session close
/// that skips the closed market. Then, under `V7`, USD settlement of markets quoted in euros and yen. Then, under `V8`,
/// operator commands signed and refused for each reason, and every market reduce-only. Then, under `V9`, the state
/// tree: a new account's deposit, resting order and cancel, a dollar round trip, a euro position, and a marked markout.
/// Then, under `V10`, spreads, a margin requirement, and close-out levels that round up; and under `V11`, markouts
/// averaged by notional and scored at a valid index. Then, under `V12`, gapped opens, quotes ageing out, a session
/// close, a leading venue, and an FX index ageing out, each processed before the next command acts. Then, under
/// `V13`, stress that follows its inputs (`stress.rs`), and the parts in `v14.rs` and `v15.rs`. Then, from `V16` on,
/// one part per version in its `v<N>.rs`: toxicity memory, the move window, dated benchmarks, MONKEY emissions,
/// custody finality, futures rolls, FIFO withdrawals, access, staking, emission debt, the operator's bounds, the
/// exit's leaves, and the elected key.
fn session() -> Vec<Sequenced> {
    let mut h = Harness::signed();
    let (a, b, c, d, t, e) = (h.signer(1), h.signer(2), h.signer(3), h.signer(4), h.signer(5), h.signer(6));
    let stranger = h.signer(9);
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 430 });

    // Funding, and deposits refused as a duplicate reference or out of range.
    h.deposit(a, "50000.123456789");
    h.deposit(b, "20000");
    h.deposit(c, "1000");
    h.deposit(d, "20000000");
    h.deposit(t, "10000");
    h.deposit(e, "20000");
    let deposit =
        |amount: &str, chain_ref: &str| Command::Deposit { account: a, amount: f(amount), chain_ref: chain_ref.into() };
    h.cmd(deposit("250.5", "0xfeed:1"));
    h.cmd(deposit("250.5", "0xfeed:1"));
    h.cmd(deposit("0", "0xfeed:2"));
    h.cmd(deposit("1000000000.000000001", "0xfeed:3"));

    // Prices: an odd and an even venue count, an unlisted market, and a malformed quote.
    h.quotes(BTC, &["100000.123456789", "100000.2", "99999.9"]);
    h.quotes(ETH, &["3000.5", "3000.7"]);
    h.price(AAPL, "230.25");
    h.price(UNLISTED, "1");
    let malformed = SourceQuote { source: "venue0".into(), mid: f("-1"), ts: h.ts, contract: None };
    h.cmd(Command::Prices { market: BTC, quotes: vec![malformed] });

    // Market orders: fills, a slippage decline, a client-tightened bound, and refusals.
    h.advance(100);
    h.market_order(a, BTC, Side::Buy, "0.123456789", "100000.123456789");
    h.market_order(a, BTC, Side::Buy, "0.1", "99000");
    h.order(b, ETH, Side::Sell, "3.3", OrderKind::Market { quoted: f("3000.6"), max_slippage_bps: Some(5) });
    h.market_order(c, BTC, Side::Buy, "1", "100000.123456789");
    h.market_order(stranger, BTC, Side::Buy, "0.01", "100000.123456789");
    h.market_order(a, BTC, Side::Buy, "0", "100000.123456789");
    h.market_order(a, UNLISTED, Side::Buy, "1", "1");

    // Signed requests refused: unsigned, signed by another key, a reused nonce, an expired request,
    // and an expiry too far ahead. The last two use their nonces.
    h.market_order(acct(8), BTC, Side::Buy, "0.01", "100000.123456789");
    let key_a = |expires_at| Client { expires_at, ..Client::new(1) };
    let forger = Client { expires_at: h.ts + 60_000, ..Client::new(7) };
    h.cmd(forger.order(h.engine.next_request_nonce(a), small_buy(a)));
    h.cmd(key_a(h.ts + 60_000).order(0, small_buy(a)));
    h.cmd(key_a(h.ts - 1).order(h.engine.next_request_nonce(a), small_buy(a)));
    h.cmd(key_a(h.ts + MAX_EXPIRY_AHEAD_MS + 1).order(h.engine.next_request_nonce(a), small_buy(a)));

    // Resting orders: a limit and a stop that trigger, one cancelled, and an unknown cancel.
    h.order(a, BTC, Side::Sell, "0.05", OrderKind::Limit { price: f("101000") });
    h.order(b, ETH, Side::Buy, "1", OrderKind::Stop { price: f("3050") });
    let cancelled = accepted(&h.order(a, BTC, Side::Buy, "0.01", OrderKind::Limit { price: f("90000") }));
    h.cmd(Command::CancelOrder { account: a, order_id: cancelled, auth: None });
    h.cmd(Command::CancelOrder { account: a, order_id: 999, auth: None });
    h.advance(500);
    h.quotes(BTC, &["101200", "101250", "101210"]);
    h.price(ETH, "3060");

    // Reduce-only: new risk refused in a reduce-only market, a reducing order allowed.
    h.admin(AdminCommand::SetReduceOnly { market: Some(ETH), reduce_only: true });
    h.market_order(b, ETH, Side::Sell, "1", "3060");
    h.cmd(Command::PlaceOrder(reducing(b, ETH, Side::Buy, "0.5", "3060")));
    h.admin(AdminCommand::SetReduceOnly { market: Some(ETH), reduce_only: false });

    // Stress: an operator floor, a fill paying its spread, then a divergence trigger and cooldown.
    h.admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    h.market_order(a, BTC, Side::Buy, "0.01", "101210");
    // A buy limit at the mid triggers, but the 5bps spread prices it above the limit: it keeps
    // resting until a later price brings the fill inside it.
    h.order(a, BTC, Side::Buy, "0.01", OrderKind::Limit { price: f("101210") });
    h.admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 0 });
    h.cmd(Command::Admin(AdminCommand::SetStressFloor { market: None, level: 5 }));
    h.quotes(BTC, &["101000", "101150"]);
    h.advance(301_000);
    h.quotes(BTC, &["101000", "101010"]);

    // Toxicity: a fill marked out 49bps in the trader's favour lifts the score into the 3bps tier.
    h.price(ETH, "3060");
    h.market_order(t, ETH, Side::Buy, "1", "3060");
    h.advance(1_000);
    h.price(ETH, "3075");
    h.market_order(t, ETH, Side::Buy, "1", "3075");
    // A buy through a short: the 1.8 short closes at a loss and a 1.2 long opens at the fill price.
    h.market_order(b, ETH, Side::Buy, "3", "3075");
    // A short held over the rollover, which receives the benchmark less the markup.
    h.market_order(e, ETH, Side::Sell, "2", "3075");

    // Size spread on a large fill, then the house cap refusing a larger one.
    h.price(BTC, "101000");
    h.market_order(d, BTC, Side::Buy, "30", "101000");
    h.market_order(d, BTC, Side::Buy, "600", "101000");

    // House capital in and out, a duplicate reference, and a move out that would leave NAV at zero or
    // below.
    let capital =
        |amount: &str, chain_ref: &str| Command::HouseCapital { amount: f(amount), chain_ref: chain_ref.into() };
    h.cmd(capital("5000000", "0xcap:1"));
    h.cmd(capital("-1000000", "0xcap:2"));
    h.cmd(capital("1", "0xcap:1"));
    h.cmd(capital("-1000000000", "0xcap:3"));

    // Withdrawals: approved, beyond free cash, and out of range.
    h.cmd(Command::Withdraw { account: a, amount: f("1000"), auth: None });
    h.cmd(Command::Withdraw { account: c, amount: f("1000000"), auth: None });
    h.cmd(Command::Withdraw { account: a, amount: f("0"), auth: None });

    // Venue hours: a stop rests over the close, nothing trades while shut, and the stop fills at
    // the gapped open. The financing rollover at 22:00 London falls in between.
    h.price(AAPL, "230.25");
    h.market_order(a, AAPL, Side::Buy, "10", "230.25");
    h.order(a, AAPL, Side::Sell, "10", OrderKind::Stop { price: f("225") });
    h.advance_to(ny(2026, 10, 20, 16, 30));
    h.cmd(Command::Tick);
    h.market_order(a, AAPL, Side::Buy, "1", "230");
    h.advance_to(ny(2026, 10, 20, 17, 30));
    h.cmd(Command::Tick);
    // BTC has had no quote for hours, so it has no price.
    h.market_order(a, BTC, Side::Buy, "0.01", "101000");
    h.advance_to(ny(2026, 10, 21, 9, 30));
    h.price(AAPL, "220");

    // Leveraged longs gapped through the balance: a close-out that takes BTC (the larger loser) then
    // ETH, and bad debt.
    h.price(BTC, "101000");
    h.price(ETH, "3075");
    h.market_order(c, ETH, Side::Buy, "0.2", "3075");
    h.market_order(c, BTC, Side::Buy, "0.09", "101000");
    h.advance(100);
    h.price(BTC, "80000");
    // A limit the account cannot margin triggers at once and is cancelled as unfillable.
    h.order(c, BTC, Side::Buy, "1", OrderKind::Limit { price: f("81000") });
    // Venues 62bps apart: no index, so nothing fills.
    h.quotes(BTC, &["80000", "80500"]);
    h.market_order(a, BTC, Side::Sell, "0.01", "80250");

    // Admin: new parameters and listings, and refusals of each kind.
    h.admin(AdminCommand::SetParams(ProtocolParams { profit_fee_bps: 1_500, ..params() }));
    h.cmd(Command::Admin(AdminCommand::SetParams(ProtocolParams { closeout_level_bps: 0, ..params() })));
    let toxicity = ToxicityParams { horizon_ms: 3_600_001, ..params().toxicity };
    h.cmd(Command::Admin(AdminCommand::SetParams(ProtocolParams { toxicity, ..params() })));
    let mut eth = market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    eth.min_order_notional = f("100");
    h.admin(AdminCommand::UpsertMarket(eth));
    h.price(ETH, "3075");
    // A reduce-only sell of 5 against a long of 2 is clipped to 2.
    h.cmd(Command::PlaceOrder(reducing(t, ETH, Side::Sell, "5", "3075")));
    h.market_order(b, ETH, Side::Buy, "0.01", "3075");
    // Margin of 10% up to 10,000 notional and 20% above: a 15,000 position needs 2,000.
    let mut sol = market(SOL, "SOL-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    sol.margin_tiers = vec![Tier { up_to: Some(f("10000")), bps: 1_000 }, Tier { up_to: None, bps: 2_000 }];
    h.admin(AdminCommand::UpsertMarket(sol));
    h.price(SOL, "150");
    h.market_order(a, SOL, Side::Buy, "100", "150");
    h.cmd(Command::Admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 10_001 }));
    h.cmd(Command::Admin(AdminCommand::SetReduceOnly { market: Some(UNLISTED), reduce_only: true }));
    // An exchange holiday on Thursday keeps AAPL shut through its session hours.
    let Schedule::Weekly { tz, windows, .. } = us_equities() else { unreachable!("US equities keep weekly hours") };
    let holiday = NaiveDate::from_ymd_opt(2026, 10, 22).expect("a real date");
    let schedule = Schedule::Weekly {
        tz,
        windows: windows.clone(),
        closed_dates: vec![holiday],
        closures: vec![],
        special_sessions: vec![],
    };
    h.admin(AdminCommand::UpsertMarket(market(AAPL, "AAPL", AssetClass::Equity, schedule, 2_000)));
    h.advance_to(ny(2026, 10, 22, 10, 0));
    h.cmd(Command::Tick);

    // Venue clocks: a quote stamped at the lead limit is kept, one a millisecond further ahead is
    // refused and leaves the venue's stale quote in place, and the first venue's next quote, stamped
    // earlier than its kept one, replaces it and prices the market.
    let ts = h.ts;
    let ahead = |source: &str, lead_ms| quote(source, "80000", ts + lead_ms);
    h.cmd(Command::Prices {
        market: BTC,
        quotes: vec![ahead("venue0", MAX_QUOTE_LEAD_MS), ahead("venue1", MAX_QUOTE_LEAD_MS + 1)],
    });
    h.advance(100);
    h.quotes(BTC, &["80000", "80010"]);

    // V4 activates at this seq. A zero house capital move then records its reference, and a calendar
    // update follows: Friday closes at 13:00 and Saturday trades a special session. An order fills
    // before the early close, is refused after it, and fills again in the special session.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V4 });
    h.cmd(Command::HouseCapital { amount: f("0"), chain_ref: "house-zero".into() });
    let at = |local: &str| local.parse().expect("a local date and time");
    let schedule = Schedule::Weekly {
        tz,
        windows,
        closed_dates: vec![holiday],
        closures: vec![Period { from: at("2026-10-23T13:00:00"), to: at("2026-10-23T16:00:00") }],
        special_sessions: vec![Period { from: at("2026-10-24T10:00:00"), to: at("2026-10-24T12:00:00") }],
    };
    h.admin(AdminCommand::UpsertMarket(market(AAPL, "AAPL", AssetClass::Equity, schedule, 2_000)));
    h.advance_to(ny(2026, 10, 23, 12, 59));
    h.price(AAPL, "201");
    h.market_order(a, AAPL, Side::Buy, "1", "201");
    h.advance_to(ny(2026, 10, 23, 13, 0));
    h.cmd(Command::Tick);
    h.market_order(a, AAPL, Side::Buy, "1", "201");
    h.advance_to(ny(2026, 10, 24, 10, 0));
    h.price(AAPL, "199");
    h.market_order(a, AAPL, Side::Sell, "1", "199");
    h.advance_to(ny(2026, 10, 24, 12, 0));
    h.cmd(Command::Tick);

    // V5 activates at this seq. Dust buys one unit of precision below the long's entry and dust sells
    // one above the short's leave both entries at 150.5, where a truncated average would lower the
    // long's. Both are held over the rollover, where a charge rounds up and a credit toward zero.
    // Closing one unit above the entries realises 10 units for the long, whose 15% fee of 1.5 rounds
    // up to 2, and loses 10.000000003 units for the short, rounded down to 11.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V5 });
    let (long, short) = (h.signer(10), h.signer(11));
    h.deposit(long, "10000");
    h.deposit(short, "10000");
    h.price(SOL, "150.5");
    h.market_order(long, SOL, Side::Buy, "10", "150.5");
    h.market_order(short, SOL, Side::Sell, "10", "150.5");
    h.price(SOL, "150.499999999");
    (0..3).for_each(|_| {
        h.market_order(long, SOL, Side::Buy, "0.000000001", "150.499999999");
    });
    h.price(SOL, "150.500000001");
    (0..3).for_each(|_| {
        h.market_order(short, SOL, Side::Sell, "0.000000001", "150.500000001");
    });
    h.advance_to(ny(2026, 10, 24, 17, 1));
    h.cmd(Command::Tick);
    h.price(SOL, "150.500000001");
    h.market_order(long, SOL, Side::Sell, "10.000000003", "150.500000001");
    h.market_order(short, SOL, Side::Buy, "10.000000003", "150.500000001");

    // V6 activates at this seq: margin after every change, on two new 24/7 markets with traders of
    // their own.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V6 });
    let (g, k, m, n) = (h.signer(12), h.signer(13), h.signer(14), h.signer(15));
    for (account, amount) in [(g, "10000"), (k, "1000"), (m, "1100"), (n, "4000")] {
        h.deposit(account, amount);
    }
    let commodity =
        |id, symbol, margin_bps| market(id, symbol, AssetClass::Commodity, Schedule::AlwaysOpen, margin_bps);
    h.admin(AdminCommand::UpsertMarket(commodity(GOLD, "XAU-USD", 1_000)));
    h.admin(AdminCommand::UpsertMarket(commodity(OIL, "WTI-USD", 1_000)));
    h.advance(100);
    h.price(GOLD, "2600");
    h.price(OIL, "70");
    // OIL's margin rises from 10% to 50%: `g` falls below the close-out level, and closing OIL, its
    // larger loser, lifts it back above.
    h.market_order(g, GOLD, Side::Buy, "20", "2600");
    h.market_order(g, OIL, Side::Buy, "500", "70");
    h.price(GOLD, "2580");
    h.price(OIL, "69");
    h.admin(AdminCommand::UpsertMarket(commodity(OIL, "WTI-USD", 5_000)));
    // A close-out level of 100% closes `k`, whose equity of 924 is below its 972.8 of margin.
    h.market_order(k, GOLD, Side::Buy, "3.8", "2580");
    h.price(GOLD, "2560");
    let fee = ProtocolParams { profit_fee_bps: 1_500, ..params() };
    h.admin(AdminCommand::SetParams(ProtocolParams { closeout_level_bps: 10_000, ..fee.clone() }));
    h.admin(AdminCommand::SetParams(fee.clone()));
    // `m` sits 0.9 above the close-out level half a second before Sunday's 22:00 London rollover, and
    // a day's financing on 9,622 at 680bps a year takes it below.
    h.market_order(m, GOLD, Side::Buy, "4", "2560");
    h.advance_to(ny(2026, 10, 25, 17, 59) + 59_500);
    h.price(GOLD, "2405.5");
    h.advance(600);
    h.cmd(Command::Tick);

    // House NAV drawn down to 100,000 caps GOLD at a 50,000 share, but a 100,000 floor takes the cap
    // past the 48,110 the house already holds: `d` adds 50,515.5 and is refused one more.
    let surplus = h.engine.house_nav() - f("100000");
    h.cmd(Command::HouseCapital { amount: -surplus, chain_ref: "0xcap:4".into() });
    let floored = MarketSpec { house_cap_floor: f("100000"), ..commodity(GOLD, "XAU-USD", 1_000) };
    h.admin(AdminCommand::UpsertMarket(floored));
    h.admin(AdminCommand::SetParams(ProtocolParams { house_total_cap_floor: f("10000000"), ..fee }));
    h.market_order(d, GOLD, Side::Buy, "21", "2405.5");
    h.market_order(d, GOLD, Side::Buy, "1", "2405.5");

    // Just after AAPL's 16:00 close, with its quotes still fresh, BTC's rise takes `n` below the
    // close-out level: AAPL, the larger loser, waits for its next session, and the BTC short closes.
    h.advance_to(ny(2026, 10, 26, 15, 59) + 59_800);
    h.price(BTC, "80000");
    h.price(AAPL, "230");
    h.market_order(n, BTC, Side::Sell, "0.1", "80000");
    h.market_order(n, AAPL, Side::Buy, "50", "230");
    h.advance(100);
    h.price(AAPL, "200");
    h.advance(150);
    h.price(BTC, "92000");

    // USD settlement: a listing quoted in euros is refused under `V6`, then `V7` activates. Germany
    // 40 is quoted in euros at EUR-USD, and USD-JPY in yen at its own index. A listing that converts
    // at a market other than its pair's is refused, and Germany 40 has no price until EUR-USD marks
    // it.
    h.cmd(Command::Admin(AdminCommand::UpsertMarket(de40(500))));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V7 });
    // Fresh capital, so the house caps the drawn-down NAV set admit these trades.
    h.cmd(Command::HouseCapital { amount: f("10000000"), chain_ref: "0xcap:5".into() });
    h.list_fx();
    let uk100 = market(30, "UK100", AssetClass::Index, Schedule::AlwaysOpen, 500);
    let via_btc = MarketSpec { quote_currency: QuoteCurrency::Gbp { fx_market: BTC }, ..uk100 };
    h.cmd(Command::Admin(AdminCommand::UpsertMarket(via_btc)));
    h.price(DE40, "20000");
    h.market_order(a, DE40, Side::Buy, "2", "20000");
    h.price(EURUSD, "1.08");
    h.market_order(a, DE40, Side::Buy, "2", "20000");
    h.price(USDJPY, "150");
    h.market_order(b, USDJPY, Side::Sell, "10000", "150");
    // A leveraged Germany 40 long closed out by an EUR-USD move alone, the index holding still.
    let q = h.signer(16);
    h.admin(AdminCommand::UpsertMarket(de40(100)));
    h.deposit(q, "2300");
    h.market_order(q, DE40, Side::Buy, "10", "20000");
    h.price(DE40, "19900");
    h.price(EURUSD, "1.2");
    // The quote currency of a market with open positions cannot change.
    let in_dollars = MarketSpec { quote_currency: QuoteCurrency::Usd, ..de40(100) };
    h.cmd(Command::Admin(AdminCommand::UpsertMarket(in_dollars)));
    // EUR-USD goes stale: a limit Germany 40 reaches keeps resting and orders are refused. An index
    // that values a euro above 10 USD converts nothing; the next valid one lets the limit fill.
    h.order(a, DE40, Side::Sell, "1", OrderKind::Limit { price: f("19950") });
    h.advance(1_500);
    h.price(DE40, "20000");
    h.market_order(a, DE40, Side::Sell, "1", "20000");
    h.price(EURUSD, "11");
    h.market_order(a, DE40, Side::Sell, "1", "20000");
    h.price(EURUSD, "1.1");
    h.price(DE40, "20000");
    // Yen PnL realised at the USD-JPY index, then the Germany 40 long's euro financing charged in USD
    // at the next rollover.
    h.price(USDJPY, "151");
    h.market_order(b, USDJPY, Side::Buy, "10000", "151");
    h.advance_to(ny(2026, 10, 26, 18, 30));
    h.cmd(Command::Tick);

    // `V8` activates once the operator is recorded. An unsigned operator command is refused now that
    // funds are credited; a signed one applies, and one signed by another key, a reused nonce, an
    // expired command, one expiring too far ahead, a node-only command, and spreads that would price
    // a close at the price itself are refused. With every market reduce-only, an open is refused and
    // a close fills until it lifts.
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V8 });
    let floor = AdminCommand::SetStressFloor { market: Some(ETH), level: 1 };
    h.cmd(Command::Admin(floor.clone()));
    let by = |seed, expires_at| Client { expires_at, ..Client::new(seed) };
    h.cmd(by(7, h.ts + 60_000).admin(0, floor.clone()));
    h.operator_admin(floor.clone());
    h.cmd(by(OPERATOR_SEED, h.ts + 60_000).admin(0, floor.clone()));
    h.cmd(by(OPERATOR_SEED, h.ts - 1).admin(1, floor.clone()));
    h.cmd(by(OPERATOR_SEED, h.ts + MAX_EXPIRY_AHEAD_MS + 1).admin(2, floor));
    h.cmd(by(OPERATOR_SEED, h.ts + 60_000).admin(3, AdminCommand::ActivateProtocol { version: ProtocolVersion::V8 }));
    let mut wide = params();
    wide.stress_levels[1].min_spread_bps = 9_998;
    h.operate(AdminCommand::SetParams(wide));
    let o = h.signer(17);
    h.deposit(o, "10000");
    h.price(BTC, "92000");
    h.market_order(o, BTC, Side::Buy, "0.01", "92000");
    h.operator_admin(AdminCommand::SetReduceOnly { market: None, reduce_only: true });
    h.market_order(o, BTC, Side::Buy, "0.01", "92000");
    h.market_order(o, BTC, Side::Sell, "0.01", "92000");
    h.operator_admin(AdminCommand::SetReduceOnly { market: None, reduce_only: false });

    // V9 activates at this seq, and the state root becomes the root of the state tree: the first
    // root hashes every leaf, and each later one the leaves its command changed. A new account is
    // credited, rests an order and cancels it, holds a euro position, opens and closes a dollar one,
    // and its first fill's markout is marked and leaves the queue.
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V9 });
    let late = h.signer(18);
    h.deposit(late, "50000");
    h.price(BTC, "80000");
    let resting = accepted(&h.order(late, BTC, Side::Buy, "0.01", OrderKind::Limit { price: f("70000") }));
    h.cmd(Command::CancelOrder { account: late, order_id: resting, auth: None });
    h.market_order(late, BTC, Side::Buy, "0.02", "80000");
    h.price(EURUSD, "1.1");
    h.price(DE40, "20000");
    h.market_order(late, DE40, Side::Buy, "1", "20000");
    h.advance(2_000);
    h.price(BTC, "80100");
    h.market_order(late, BTC, Side::Sell, "0.02", "80100");
    v10::session(&mut h);
    v11::session(&mut h);
    v12::session(&mut h);
    stress::session(&mut h);
    v14::session(&mut h);
    v15::session(&mut h);
    v16::session(&mut h);
    v17::session(&mut h);
    v18::session(&mut h);
    v19::session(&mut h);
    v20::session(&mut h);
    v21::session(&mut h, a);
    for session in [v22::session, v23::session, v24::session, v25::session, v26::session, v27::session, v28::session] {
        session(&mut h);
    }
    for session in [v29::session, v30::session, v31::session, v32::session, v33::session, v34::session, v35::session] {
        session(&mut h);
    }
    v37::session(&mut h);
    v38::session(&mut h);
    v39::session(&mut h);
    h.log
}
