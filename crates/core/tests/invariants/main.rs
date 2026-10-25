//! Accounting invariants under generated sessions. Each case sequences random client, feed,
//! custody, and operator commands, with values inside and outside the engine's input bounds, under
//! `V2`, the rules every log starts under, and again under `V6`, under `V7` with markets quoted in
//! other currencies, under `V11`, and under `V12` with those markets and the operator signing its
//! commands, as `V13`, `V14`, `V15`, `V16`, and `V17` then do. It checks after every command that it
//! did not panic, that cash is conserved, and that each market's aggregates match its accounts'
//! positions; under `V12`, also that every market has processed the index its quotes give at the
//! sequenced time, holds no quote stamped after it, and was filled only at that index; under
//! `V14`, that resting orders keep to the cap, placing an order fills or cancels no other, and no
//! market is financed at a rollover that records its benchmark as having no rate; under `V15`,
//! that a resting order accepted below its market's minimum notional, or with no dollar value, can
//! only reduce its account's position in that market; and under `V16`, that no account's toxicity
//! history exceeds the memory in force. Under each version the same sessions must reach every path
//! that moves cash and the refusal of out-of-range amounts, order numbers, and quotes; under `V6`
//! a close-out that no price triggered; under `V12` a resting order filled by a command with
//! neither prices nor an order, which `V2` never fills; under `V13`, which runs as `V12` does, a
//! stress change, as under `V17`; under `V14` alone its cap and missing rates; under `V15` a reducing order
//! resting below the minimum; and under `V16` a history clamped by a lowered memory; so the
//! checks cannot pass vacuously.

mod access;
mod charges;
mod checks;
#[path = "../common/mod.rs"]
mod common;
mod rounding;
mod session;
mod state_root;
mod toxicity;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use checks::{check_v14, check_v15, check_v16};
use common::{HOUSE_CAPITAL, Harness, acct};
use engine_core::command::{AdminCommand, Command};
use engine_core::engine::MarketState;
use engine_core::event::{Event, Fill, Reject};
use engine_core::market::QuoteCurrency;
use engine_core::price::compute_index;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::MarketId;
use engine_core::{Engine, Fixed};
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestRunner};
use session::{Session, command, session};

/// Seed for every run, so CI and local runs check the same sessions.
const SEED: u64 = 0x5eed_0021;
/// Sessions per run: enough to find edge-of-range panics, few enough to run in a few seconds.
const CASES: u32 = 1_024;
/// The coverage path of a resting order filled by a command that carries neither prices nor an
/// order, as only an index changed without a price update can fill it.
const QUIET_RESTING_FILL: &str = "resting order filled without a price or an order";
/// The coverage path of a fill in a market quoted in yen.
const YEN_FILL: &str = "fill in yen";

#[test]
fn generated_sessions_hold_the_invariants_and_reach_every_checked_path() {
    // Coverage is recorded per protocol version, from the same sessions whose invariants are
    // checked.
    let reached: RefCell<BTreeMap<ProtocolVersion, BTreeSet<&str>>> = RefCell::default();
    let result = TestRunner::new(config()).run(&session(), |session| {
        let v12 = || {
            let mut h = Harness::v12();
            h.list_fx();
            h
        };
        let v13 = || {
            let mut h = Harness::v13();
            h.list_fx();
            h
        };
        let v14 = || {
            let mut h = Harness::v14();
            h.list_fx();
            h
        };
        let v15 = || {
            let mut h = Harness::v15();
            h.list_fx();
            h
        };
        let v16 = || {
            let mut h = Harness::v16();
            h.list_fx();
            h
        };
        let latest = || {
            let mut h = Harness::v17();
            h.list_fx();
            h
        };
        for genesis in
            [Harness::new(), Harness::v6(), Harness::fx(), Harness::v11(), v12(), v13(), v14(), v15(), v16(), latest()]
        {
            let mut h = funded(genesis, &session);
            let version = h.engine.protocol_version();
            let mut inflow = h.deposits + Fixed::from_int(HOUSE_CAPITAL);
            check(&h.engine, inflow).map_err(TestCaseError::fail)?;
            for step in &session.steps {
                h.advance(step.advance_ms);
                let cmd = command(&step.action, &h.engine, h.ts);
                // The `V16` coverage path: a `SetParams` applied while an account's history exceeds
                // the memory it sets, so the clamp has something to clamp.
                let clamps = version >= ProtocolVersion::V16 && {
                    let memory = match &cmd {
                        Command::SignedAdmin(req) => match &req.command {
                            AdminCommand::SetParams(params) => {
                                params.toxicity.notional.map(|weighting| weighting.memory)
                            }
                            _ => None,
                        },
                        _ => None,
                    };
                    memory.is_some_and(|memory| h.engine.accounts().values().any(|acct| acct.toxicity_history > memory))
                };
                // `Harness::cmd` panics if the engine refuses the sequence; a panic fails the case.
                let events = h.cmd(cmd.clone());
                let mut reached = reached.borrow_mut();
                let paths = reached.entry(version).or_default();
                paths.extend(events.iter().map(path));
                if clamps && events.iter().any(|event| matches!(event, Event::AdminApplied)) {
                    paths.insert("toxicity history clamped");
                }
                if !matches!(cmd, Command::Prices { .. }) && events.iter().any(|e| matches!(e, Event::CloseOut { .. }))
                {
                    paths.insert("close-out without a price");
                }
                let quiet = !matches!(cmd, Command::Prices { .. } | Command::PlaceOrder(_));
                if quiet && events.iter().any(|e| matches!(e, Event::Fill(fill) if !fill.liquidation)) {
                    paths.insert(QUIET_RESTING_FILL);
                }
                let in_yen = |fill: &Fill| {
                    matches!(h.engine.markets()[&fill.market].spec.quote_currency, QuoteCurrency::Jpy { .. })
                };
                if events.iter().any(|e| matches!(e, Event::Fill(fill) if in_yen(fill))) {
                    paths.insert(YEN_FILL);
                }
                inflow += cash_moved(&events);
                let fail = |e| TestCaseError::fail(format!("{version:?}, after {cmd:?}: {e}"));
                check(&h.engine, inflow).map_err(fail)?;
                if version >= ProtocolVersion::V12 {
                    check_processed(&h.engine).map_err(fail)?;
                }
                // An operator command can change an index after the command processed an earlier one.
                let admin = matches!(cmd, Command::Admin(_) | Command::SignedAdmin(_));
                if version >= ProtocolVersion::V12 && !admin {
                    check_fills(&h.engine, &events).map_err(fail)?;
                }
                if version >= ProtocolVersion::V14 {
                    check_v14(&h.engine, &cmd, &events).map_err(fail)?;
                }
                if version >= ProtocolVersion::V15 && check_v15(&h.engine, &cmd, &events).map_err(fail)? {
                    paths.insert("reducing order rests below the minimum");
                }
                if version >= ProtocolVersion::V16 {
                    check_v16(&h.engine).map_err(fail)?;
                }
            }
        }
        Ok(())
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
    let expected = [
        "deposit",
        "deposit rejected",
        "deposit out of range",
        "withdrawal",
        "withdrawal rejected",
        "withdrawal out of range",
        "house capital",
        "house capital out of range",
        "fill",
        "fill with profit fee",
        "liquidation",
        "bad debt",
        "financing",
        "order out of range",
        "malformed quote",
        "quote stamped ahead",
        "admin applied",
        "admin rejected",
    ];
    let v14_only = ["resting order cap", "benchmark rate missing"];
    let reached = reached.into_inner();
    let paths = |version| reached.get(&version).cloned().unwrap_or_default();
    let missed: Vec<(ProtocolVersion, &str)> = [
        ProtocolVersion::V2,
        ProtocolVersion::V6,
        ProtocolVersion::V7,
        ProtocolVersion::V11,
        ProtocolVersion::V12,
        ProtocolVersion::V13,
        ProtocolVersion::V14,
        ProtocolVersion::V15,
        ProtocolVersion::V16,
        ProtocolVersion::V17,
    ]
    .into_iter()
    .flat_map(|version| expected.iter().map(move |path| (version, *path)))
    .chain([
        (ProtocolVersion::V6, "close-out without a price"),
        (ProtocolVersion::V12, "close-out without a price"),
        (ProtocolVersion::V14, "close-out without a price"),
        (ProtocolVersion::V15, "close-out without a price"),
        (ProtocolVersion::V16, "close-out without a price"),
        (ProtocolVersion::V17, "close-out without a price"),
    ])
    // Only `V7`, `V12`, `V14`, `V15`, `V16`, and `V17` list markets quoted in another currency.
    .chain([(ProtocolVersion::V7, "fill converted to USD"), (ProtocolVersion::V7, "no FX price")])
    .chain([(ProtocolVersion::V12, "fill converted to USD"), (ProtocolVersion::V12, "no FX price")])
    .chain([(ProtocolVersion::V14, "fill converted to USD"), (ProtocolVersion::V14, "no FX price")])
    .chain([(ProtocolVersion::V15, "fill converted to USD"), (ProtocolVersion::V15, "no FX price")])
    .chain([(ProtocolVersion::V16, "fill converted to USD"), (ProtocolVersion::V16, "no FX price")])
    .chain([(ProtocolVersion::V12, QUIET_RESTING_FILL), (ProtocolVersion::V12, YEN_FILL)])
    .chain([(ProtocolVersion::V14, QUIET_RESTING_FILL), (ProtocolVersion::V14, YEN_FILL)])
    .chain([(ProtocolVersion::V15, QUIET_RESTING_FILL), (ProtocolVersion::V15, YEN_FILL)])
    .chain([(ProtocolVersion::V16, QUIET_RESTING_FILL), (ProtocolVersion::V16, YEN_FILL)])
    .chain([(ProtocolVersion::V17, "fill converted to USD"), (ProtocolVersion::V17, "no FX price")])
    .chain([(ProtocolVersion::V17, QUIET_RESTING_FILL), (ProtocolVersion::V17, YEN_FILL)])
    .chain([(ProtocolVersion::V13, "stress changed")])
    .chain([(ProtocolVersion::V17, "stress changed")])
    .chain([(ProtocolVersion::V15, "reducing order rests below the minimum")])
    .chain([(ProtocolVersion::V16, "toxicity history clamped"), (ProtocolVersion::V17, "toxicity history clamped")])
    .chain([(ProtocolVersion::V16, "reducing order rests below the minimum")])
    .chain([(ProtocolVersion::V17, "reducing order rests below the minimum")])
    .chain(v14_only.iter().map(|path| (ProtocolVersion::V14, *path)))
    .filter(|(version, path)| !paths(*version).contains(path))
    .collect();
    assert!(missed.is_empty(), "generated sessions never reach {missed:?}");
    assert!(!paths(ProtocolVersion::V2).contains(QUIET_RESTING_FILL), "V2 reached {QUIET_RESTING_FILL}");
    let early: Vec<(ProtocolVersion, &str)> = [
        ProtocolVersion::V2,
        ProtocolVersion::V6,
        ProtocolVersion::V7,
        ProtocolVersion::V11,
        ProtocolVersion::V12,
        ProtocolVersion::V13,
    ]
    .into_iter()
    .flat_map(|version| v14_only.iter().map(move |path| (version, *path)))
    .filter(|(version, path)| paths(*version).contains(path))
    .collect();
    assert!(early.is_empty(), "sessions before `V14` reach {early:?}");
}

// ── Helpers ──

/// `CASES` sessions from `SEED`, without writing regression files.
fn config() -> Config {
    Config { cases: CASES, rng_seed: RngSeed::Fixed(SEED), failure_persistence: None, ..Config::default() }
}

/// `h` after its genesis, with the session's deposits for the three traders.
fn funded(mut h: Harness, session: &Session) -> Harness {
    for (n, amount) in (1..).zip(session.funding) {
        h.deposit(acct(n), &amount.to_string());
    }
    h
}

/// Fails unless client balances plus house cash equal the cash that has entered and left, and every
/// market's aggregate qty and cost equal the sum over its accounts' positions.
/// Error cases:
/// - cash is not conserved
/// - a market's aggregates differ from its positions, or a position is in an unlisted market
fn check(engine: &Engine, inflow: Fixed) -> Result<(), String> {
    let balances: Fixed = engine.accounts().values().map(|acct| acct.balance).sum();
    if balances + engine.house_cash() != inflow {
        return Err(format!("cash not conserved: balances {balances} + house {} != {inflow}", engine.house_cash()));
    }
    let mut held: BTreeMap<MarketId, (Fixed, Fixed)> = BTreeMap::new();
    for (market, pos) in engine.accounts().values().flat_map(|acct| &acct.positions) {
        let (qty, cost) = held.entry(*market).or_default();
        *qty += pos.qty;
        *cost += pos.cost();
    }
    for (id, market) in engine.markets() {
        let (qty, cost) = held.remove(id).unwrap_or_default();
        if (market.client_qty, market.client_cost) != (qty, cost) {
            return Err(format!(
                "market {id} aggregates {} / {} != positions {qty} / {cost}",
                market.client_qty, market.client_cost
            ));
        }
    }
    match held.keys().next() {
        Some(id) => Err(format!("positions in unlisted market {id}")),
        None => Ok(()),
    }
}

/// Fails unless every market's session and processed index are those at the sequenced time, every
/// market that can deal is marked at that index and FX index, and no market holds a quote stamped
/// after the sequenced time.
/// Error cases:
/// - a market's open flag differs from its schedule at the sequenced time
/// - a market's processed index differs from the index it can deal at now, or its mark or FX mark
///   from that index's and its FX market's
/// - a market holds a quote stamped after the sequenced time
fn check_processed(engine: &Engine) -> Result<(), String> {
    let now = engine.now();
    let index_of = |market: &MarketState| compute_index(&market.quotes, &market.spec.price_rules, now).ok();
    engine.markets().iter().try_for_each(|(id, market)| {
        if market.open != market.spec.schedule.is_open(now) {
            return Err(format!("market {id} open {} at {now}, against its schedule", market.open));
        }
        // The index its FX market converts it at now: `Some(None)` for a USD market, `None` without
        // an FX price. A market that converts at its own index takes its FX mark from that index.
        let currency = market.spec.quote_currency;
        let fx = match currency.fx_market() {
            None => Some(None),
            Some(fx_id) if fx_id == *id => {
                index_of(market).filter(|index| currency.converts_at(index.mid)).map(|index| Some(index.mid))
            }
            Some(fx_id) => {
                let fx = &engine.markets()[&fx_id];
                index_of(fx)
                    .filter(|index| fx.spec.schedule.is_open(now) && currency.converts_at(index.mid))
                    .filter(|_| market.fx_mark.is_some())
                    .map(|index| Some(index.mid))
            }
        };
        let dealable = market.open && engine.params().is_some() && fx.is_some();
        let live = dealable.then(|| index_of(market)).flatten();
        if market.processed_index != live {
            return Err(format!("market {id} processed {:?}, but deals at {live:?}", market.processed_index));
        }
        if let Some(index) = live
            && (market.mark != Some(index.mid) || market.fx_mark != fx.flatten())
        {
            return Err(format!(
                "market {id} marked at {:?} and {:?}, not its index {index:?}",
                market.mark, market.fx_mark
            ));
        }
        match market.quotes.values().find(|quote| quote.ts > now) {
            Some(quote) => Err(format!("market {id} holds {quote:?} stamped after {now}")),
            None => Ok(()),
        }
    })
}

/// Fails unless every fill among `events` is at its market's processed index, and converts to USD at
/// its FX market's.
/// Error cases:
/// - a fill at an index other than its market's processed index
/// - a fill converted at an FX index other than its FX market's processed index
fn check_fills(engine: &Engine, events: &[Event]) -> Result<(), String> {
    let processed = |market| engine.markets().get(&market).and_then(|m| m.processed_index).map(|index| index.mid);
    let fills = events.iter().filter_map(|event| match event {
        Event::Fill(fill) => Some(fill),
        _ => None,
    });
    fills.into_iter().try_for_each(|fill: &Fill| {
        let fx_market = engine.markets()[&fill.market].spec.quote_currency.fx_market();
        if processed(fill.market) != Some(fill.index) {
            return Err(format!("{fill:?} is not at its market's processed index"));
        }
        if fill.fx_index.is_some() && fx_market.and_then(processed) != fill.fx_index {
            return Err(format!("{fill:?} does not convert at its FX market's processed index"));
        }
        Ok(())
    })
}

/// Cash the events moved across the engine's boundary: deposits and house capital in, approved
/// withdrawals out.
fn cash_moved(events: &[Event]) -> Fixed {
    events
        .iter()
        .map(|event| match event {
            Event::Deposited { amount, .. } | Event::HouseCapitalMoved { amount, .. } => *amount,
            Event::WithdrawalApproved { amount, .. } => -*amount,
            _ => Fixed::ZERO,
        })
        .sum()
}

/// The path an event shows a session reached, for the coverage check.
fn path(event: &Event) -> &'static str {
    match event {
        Event::Deposited { .. } => "deposit",
        Event::DepositRejected { reason: Reject::InvalidAmount, .. } => "deposit out of range",
        Event::DepositRejected { .. } => "deposit rejected",
        Event::WithdrawalApproved { .. } => "withdrawal",
        Event::WithdrawalRejected { reason: Reject::InvalidAmount, .. } => "withdrawal out of range",
        Event::WithdrawalRejected { .. } => "withdrawal rejected",
        Event::HouseCapitalMoved { .. } => "house capital",
        Event::Fill(fill) if fill.liquidation => "liquidation",
        Event::Fill(fill) if fill.profit_fee.is_positive() => "fill with profit fee",
        Event::Fill(fill) if fill.fx_index.is_some() => "fill converted to USD",
        Event::Fill(_) => "fill",
        Event::BadDebt { .. } => "bad debt",
        Event::Financing { .. } => "financing",
        Event::StressChanged { .. } => "stress changed",
        Event::BenchmarkRateMissing { .. } => "benchmark rate missing",
        Event::BenchmarkRateObserved { .. } => "benchmark rate observed",
        Event::BenchmarkRateUndated { .. } => "benchmark rate undated",
        Event::OrderRejected { reason: Reject::TooManyOrders { .. }, .. } => "resting order cap",
        Event::OrderRejected { reason: Reject::InvalidOrder { reason }, .. } if reason.ends_with("out of range") => {
            "order out of range"
        }
        Event::OrderRejected { reason: Reject::NoFxPrice { .. }, .. } => "no FX price",
        Event::PricesRejected { reason, .. } if reason.starts_with("malformed") => "malformed quote",
        Event::PricesRejected { reason, .. } if reason.ends_with("ahead of the sequenced time") => {
            "quote stamped ahead"
        }
        Event::AdminApplied => "admin applied",
        Event::AdminRejected { reason } if reason.ends_with("amount out of range") => "house capital out of range",
        // House capital refusals come back as admin rejections; keep them apart from operator ones.
        Event::AdminRejected { reason } if reason.starts_with("house capital") => "house capital rejected",
        Event::AdminRejected { .. } => "admin rejected",
        _ => "other",
    }
}
