//! The close-out index acts exactly as the full check it replaces. Three engines take the same
//! generated commands at `ProtocolVersion::LATEST` on the production genesis's markets, with accounts
//! holding up to all of thirteen of them, crypto, FX, US and Japanese index futures, metals, oil and
//! shares, long and short, at leverage from far above to just over their close-out level: one with
//! the index, checked against a full assessment at every refresh, check site and close-out; one with
//! today's full check; and one encoded and restored at random points, so its index is rebuilt. The
//! commands walk prices with jumps in every market and FX market, let quotes age out, cross market
//! hours, trade, rest stops and limits, deposit and withdraw, roll over with financing, and cross the
//! weekend gap of oil, silver, US Tech 100 and Japan 225, listed with `WEEKEND_MARGINS`, whose close-out
//! the index also serves. The production genesis names its offset grid, so from `V81` the books hold
//! grouped FX, index and metals positions, every check reads the portfolio requirement, and after every
//! command each account's lies between the lowest floor share of its per-position requirement and all of it.
//! Every command's events are equal across the three, and so are their state roots at each restore
//! and at the end. The generated sessions must close accounts out on a price and at the gap, and
//! the index must cross ranges on prices and FX indexes, move headrooms, key accounts, carry its
//! keying queue from one command to the next (the indexed engine keys four positions a command),
//! value a gap's holders at marks other than its keys' valuation, and skip both check sites' calls,
//! so the comparison cannot pass vacuously.

use std::cell::RefCell;
use std::collections::BTreeMap;

use engine_core::access::{AccessDeposit, AccessPolicy};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced, WithdrawalRequest};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Side};
use engine_core::{CloseoutCoverage, CloseoutOracle, Engine, Fixed};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};

use crate::common::close_spread::{attest, evidence, fund, source_key};
use crate::common::launch_book::{BOOK_TIME, WEEKEND_MARGINS, benchmark_batch, price_command, production_harness_at};
use crate::common::session_keys::{HOUR_MS, trade_cmd};
use crate::common::{BTC, Client, Harness, OPERATOR_SEED};

const SEED: u64 = 0x5eed_0912;
const CASES: u32 = 12;
const STEPS: usize = 90;
const ACCOUNTS: usize = 18;
const OIL_ACCOUNTS: usize = 6;
const WTI: MarketId = 32;
/// The markets accounts hold, each with its opening price and the FX market its quote converts at.
const MARKETS: [(MarketId, &str, Option<MarketId>); 13] = [
    (1, "50000", None),
    (2, "3000", None),
    (3, "150", None),
    (10, "1.1", None),
    (11, "1.27", None),
    (12, "150", Some(12)),
    (20, "5000", None),
    (21, "18000", None),
    (25, "38000", Some(12)),
    (31, "30", None),
    (32, "70", None),
    (40, "200", None),
    (46, "250", None),
];

#[derive(Clone, Debug)]
enum Step {
    /// `market`'s price moved by `bps` from where it is.
    Price {
        market: usize,
        bps: i64,
    },
    /// A market order for `usd` of notional.
    Trade {
        account: usize,
        market: usize,
        buy: bool,
        usd: i64,
    },
    /// A reduce-only order closing the account's whole position.
    Close {
        account: usize,
        market: usize,
    },
    /// A resting stop or limit `bps` from the price.
    Rest {
        account: usize,
        market: usize,
        buy: bool,
        usd: i64,
        stop: bool,
        bps: i64,
    },
    Deposit {
        account: usize,
        usd: i64,
    },
    Withdraw {
        account: usize,
        usd: i64,
    },
    /// `ms` pass, then every held market is quoted again at its price.
    Wait {
        ms: i64,
    },
    /// The next rollover runs, after the day's benchmark rates.
    Rollover,
    /// The third engine is encoded and restored.
    Restore,
}

/// An account's cash and the positions it opens, each a market, a side and a notional in USD.
type Opening = (i64, Vec<(usize, bool, i64)>);

/// The three engines and what the session reached.
struct Run {
    h: Harness,
    full: Engine,
    restored: Engine,
    clients: Vec<Client>,
    mids: BTreeMap<MarketId, Fixed>,
    closeouts: usize,
}

#[test]
fn the_index_closes_out_as_the_full_check_does() {
    let config = Config {
        cases: CASES,
        rng_seed: RngSeed::Fixed(SEED),
        failure_persistence: None,
        max_shrink_iters: 16,
        ..Config::default()
    };
    let reached = RefCell::new((CloseoutCoverage::default(), 0));
    let openings = vec((5_000..120_000i64, vec((0..MARKETS.len(), any::<bool>(), 1_000..400_000i64), 1..8)), ACCOUNTS);
    // Oil held from 10x to 19x its cash: below its weekend margin of 7.5% at the gap or above it.
    let oil = vec((5_000..50_000i64, 100..190i64), OIL_ACCOUNTS);
    TestRunner::new(config)
        .run(&(openings, oil, steps(), steps()), |(mut openings, oil, before, after)| {
            openings.extend(oil.iter().map(|(cash, _)| (*cash, Vec::new())));
            let mut run = Run::open(&openings);
            for step in &before {
                run.step(step);
            }
            run.cross_weekend(&oil);
            for step in &after {
                run.step(step);
            }
            run.compare_roots();
            let mut reached = reached.borrow_mut();
            add(&mut reached.0, run.h.engine.closeout_coverage());
            add(&mut reached.0, run.restored.closeout_coverage());
            reached.1 += run.closeouts;
            Ok(())
        })
        .unwrap();
    let (coverage, closeouts) = reached.into_inner();
    assert!(closeouts > 0, "no close-outs: {coverage:?}");
    let CloseoutCoverage {
        price_crossings,
        fx_crossings,
        headroom_moves,
        keys,
        weekend_skips,
        weekend_revaluations,
        weekend_closeouts,
        carried_over,
        skipped_calls,
        ..
    } = coverage;
    for (name, count) in [
        ("price crossings", price_crossings),
        ("FX crossings", fx_crossings),
        ("headroom moves", headroom_moves),
        ("keys", keys),
        ("weekend skips", weekend_skips),
        ("weekend valuations at marks off the keys'", weekend_revaluations),
        ("weekend close-outs", weekend_closeouts),
        ("queues carried to a later command", carried_over),
        ("skipped calls", skipped_calls),
    ] {
        assert!(count > 0, "no {name}: {coverage:?}");
    }
}

/// `STEPS` generated steps.
fn steps() -> impl Strategy<Value = Vec<Step>> {
    let walk = prop_oneof![9 => -80..80i64, 1 => prop_oneof![-1_500..-200i64, 200..1_500i64]];
    let market = 0..MARKETS.len();
    let account = 0..ACCOUNTS + OIL_ACCOUNTS;
    let step = prop_oneof![
        30 => (market.clone(), walk).prop_map(|(market, bps)| Step::Price { market, bps }),
        14 => (account.clone(), market.clone(), any::<bool>(), 1_000..300_000i64)
            .prop_map(|(account, market, buy, usd)| Step::Trade { account, market, buy, usd }),
        5 => (account.clone(), market.clone()).prop_map(|(account, market)| Step::Close { account, market }),
        5 => (account.clone(), market.clone(), any::<bool>(), 1_000..100_000i64, any::<bool>(), 1..300i64)
            .prop_map(|(account, market, buy, usd, stop, bps)| Step::Rest { account, market, buy, usd, stop, bps }),
        4 => (account.clone(), 100..30_000i64).prop_map(|(account, usd)| Step::Deposit { account, usd }),
        4 => (account, 100..30_000i64).prop_map(|(account, usd)| Step::Withdraw { account, usd }),
        6 => (1_000..900_000i64).prop_map(|ms| Step::Wait { ms }),
        2 => Just(Step::Rollover),
        3 => Just(Step::Restore),
    ];
    vec(step, STEPS)
}

impl Run {
    /// The production genesis with `WEEKEND_MARGINS` and every market priced, `openings`' accounts
    /// declared, funded and trading, and the engine copied into the full-check and restored engines.
    fn open(openings: &[Opening]) -> Self {
        let mut h = production_harness_at(ProtocolVersion::LATEST, None, BOOK_TIME, false, &WEEKEND_MARGINS);
        h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 400 * HOUR_MS }));
        let mids: BTreeMap<MarketId, Fixed> = MARKETS.iter().map(|(id, mid, _)| (*id, mid.parse().unwrap())).collect();
        let others: Vec<MarketId> = h.engine.markets().keys().copied().filter(|id| !mids.contains_key(id)).collect();
        for id in others {
            let cmd = price_command(&h, id, "100", h.ts);
            h.cmd(cmd);
        }
        let events = h.operate(benchmark_batch(&h));
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        for (id, mid) in &mids {
            let cmd = price_command(&h, *id, &mid.to_string(), h.ts);
            h.cmd(cmd);
        }
        let mut run = Run { h, full: Engine::new(), restored: Engine::new(), clients: Vec::new(), mids, closeouts: 0 };
        // The operator's key is no client's: from `V88` it signs no client request.
        for (seed, (cash, positions)) in (0x31u8..).filter(|seed| *seed != OPERATOR_SEED).zip(openings) {
            let client = Client::new(seed);
            attest(&mut run.h, &client);
            fund(&mut run.h, &client, &cash.to_string());
            run.clients.push(client);
            for (market, buy, usd) in positions {
                let cmd = run.order(run.clients.len() - 1, *market, *buy, *usd, None);
                run.h.cmd(cmd);
            }
        }
        // A few positions a command, so the queue carries from command to command under the oracle.
        run.h.engine.set_closeout_oracle(CloseoutOracle {
            verify: true,
            keying_allowance: Some(4),
            ..Default::default()
        });
        run.full = copy(&run.h.engine);
        run.full.set_closeout_oracle(CloseoutOracle { full_check: true, ..CloseoutOracle::default() });
        run.restored = copy(&run.h.engine);
        run.restored.set_closeout_oracle(CloseoutOracle { verify: true, ..CloseoutOracle::default() });
        run
    }

    /// Applies one step.
    fn step(&mut self, step: &Step) {
        match *step {
            Step::Price { market, bps } => {
                let (id, ..) = MARKETS[market];
                let mid = self.mids[&id].mul_bps(10_000 + bps).max(Fixed::from_raw(1_000_000));
                self.mids.insert(id, mid);
                self.h.advance(50);
                self.quote(id);
            }
            Step::Trade { account, market, buy, usd } => {
                let cmd = self.order(account, market, buy, usd, None);
                self.apply(cmd);
            }
            Step::Close { account, market } => {
                let (id, ..) = MARKETS[market];
                let held = self.h.engine.accounts()[&self.clients[account].account].position(id).qty;
                if !held.is_zero() {
                    let cmd = self.order(account, market, held.is_negative(), 0, Some(held.abs()));
                    self.apply(cmd);
                }
            }
            Step::Rest { account, market, buy, usd, stop, bps } => {
                let (id, ..) = MARKETS[market];
                let away = if buy == stop { 10_000 + bps } else { 10_000 - bps };
                let at = self.mids[&id].mul_bps(away);
                let kind = if stop { OrderKind::Stop { price: at } } else { OrderKind::Limit { price: at } };
                let cmd = self.order(account, market, buy, usd, None);
                let Command::AccessOrder { request, .. } = cmd else { unreachable!("an order") };
                let request = OrderRequest { kind, auth: None, ..request };
                let cmd = self.sign(account, request);
                self.apply(cmd);
            }
            Step::Deposit { account, usd } => {
                let deposit = AccessDeposit {
                    account: Some(self.clients[account].account),
                    payer: Some(self.clients[account].account),
                    amount: Fixed::from_int(usd),
                    chain_ref: format!("0xtopup:{}", self.h.log.len()),
                };
                let ev = evidence(&self.h, eip712::deposit_screening_hash(&deposit), deposit.account, deposit.payer);
                self.apply(Command::AccessDeposit { deposit, evidence: ev });
            }
            Step::Withdraw { account, usd } => {
                let client = self.client(account);
                let nonce = self.h.engine.next_request_nonce(client.account);
                let Command::Withdraw { account, amount, auth } = client.withdraw(nonce, &usd.to_string()) else {
                    unreachable!("a withdrawal")
                };
                let request = WithdrawalRequest { account, amount, auth };
                let signed = request.auth.as_ref().expect("signed");
                let hash = eip712::withdrawal_hash(&client.domain, account, amount, signed.nonce, signed.expires_at);
                let ev = evidence(&self.h, hash, Some(account), None);
                self.apply(Command::AccessWithdraw { request, evidence: ev, payout_available: true });
            }
            Step::Wait { ms } => {
                self.h.advance(ms);
                self.apply(Command::Tick);
                self.quote_all();
            }
            Step::Rollover => self.roll_over(),
            Step::Restore => {
                self.restored = copy(&self.restored);
                self.restored.set_closeout_oracle(CloseoutOracle { verify: true, ..CloseoutOracle::default() });
                self.compare_roots();
            }
        }
    }

    /// Rolls over day by day, with the day's rates, to the day oil's weekend gap starts; opens each
    /// oil account's position, `oil`'s cash times its leverage in tenths, before the gap's margin
    /// applies; publishes the gap's day's rates; quotes BTC-USD so that one of its venues ages out at
    /// the gap's start, where its index is then other than its mark; and runs the gap's close-out.
    fn cross_weekend(&mut self, oil: &[(i64, i64)]) {
        let window = self.h.engine.markets()[&WTI].weekend_window().expect("oil keeps a weekend gap");
        while self.h.engine.next_rollover().is_some_and(|due| due < window.from) {
            self.roll_over();
        }
        self.h.advance_to((window.from - HOUR_MS).max(self.h.ts));
        self.quote_all();
        let market = MARKETS.iter().position(|(id, ..)| *id == WTI).expect("oil is held");
        for (slot, (cash, tenths)) in oil.iter().enumerate() {
            let cmd = self.order(ACCOUNTS + slot, market, true, cash * tenths / 10, None);
            self.apply(cmd);
        }
        while self.h.engine.next_rollover().is_some_and(|due| due < window.close) {
            self.roll_over();
        }
        self.h.advance_to((window.close - 1_000).max(self.h.ts));
        self.publish_benchmarks();
        self.h.advance_to((window.close - 400).max(self.h.ts));
        let mid = self.mids[&BTC];
        let quote = |source: &str, bps: i64, ts| SourceQuote {
            source: source.into(),
            mid: mid.mul_bps(bps),
            ts,
            contract: None,
        };
        let quotes = vec![
            quote("venue0", 9_995, self.h.ts),
            quote("venue1", 10_015, self.h.ts),
            quote("venue2", 10_000, self.h.ts - 700),
        ];
        self.apply(Command::Prices { market: BTC, quotes });
        self.h.advance_to(window.close.max(self.h.ts));
        self.apply(Command::Tick);
        self.quote_all();
    }

    /// Publishes the day's benchmark rates just before the next rollover, runs it, and quotes again.
    fn roll_over(&mut self) {
        let due = self.h.engine.next_rollover().expect("a rollover is scheduled");
        self.h.advance_to((due - 1).max(self.h.ts));
        self.publish_benchmarks();
        self.h.advance_to(due.max(self.h.ts));
        self.apply(Command::Tick);
        self.quote_all();
    }

    /// The day's benchmark rates, signed by the operator.
    fn publish_benchmarks(&mut self) {
        let operator = Client { expires_at: self.h.ts + 60_000, ..Client::operator() };
        let nonce = self.h.engine.next_request_nonce(operator.account);
        let cmd = operator.admin(nonce, benchmark_batch(&self.h));
        self.apply(cmd);
    }

    /// Quotes every held market at its price, a second apart.
    fn quote_all(&mut self) {
        for (id, ..) in MARKETS {
            self.h.advance(1);
            self.quote(id);
        }
    }

    /// Quotes `id` at its price now.
    fn quote(&mut self, id: MarketId) {
        let cmd = price_command(&self.h, id, &self.mids[&id].to_string(), self.h.ts);
        self.apply(cmd);
    }

    /// A signed market order of `account` for `usd` of notional, or `close` units reduce-only.
    fn order(&self, account: usize, market: usize, buy: bool, usd: i64, close: Option<Fixed>) -> Command {
        let (id, _, fx) = MARKETS[market];
        let mid = self.mids[&id];
        let usd_per_unit = match fx {
            Some(fx) => mid.div(self.mids[&fx]),
            None => mid,
        };
        let qty = close.unwrap_or_else(|| Fixed::from_int(usd).div(usd_per_unit));
        let request = OrderRequest {
            account: self.clients[account].account,
            client_id: 7,
            market: id,
            side: if buy { Side::Buy } else { Side::Sell },
            qty,
            kind: OrderKind::Market { quoted: mid, max_slippage_bps: None },
            reduce_only: close.is_some(),
            take_profit: None,
            stop_loss: None,
            auth: None,
        };
        self.sign(account, request)
    }

    /// `request` signed by its account with its next nonce.
    fn sign(&self, account: usize, request: OrderRequest) -> Command {
        let client = self.client(account);
        trade_cmd(&self.h, &client, self.h.engine.next_request_nonce(client.account), request)
    }

    /// `account`'s client, signing requests that expire a minute from now.
    fn client(&self, account: usize) -> Client {
        let client = &self.clients[account];
        Client {
            key: client.key.clone(),
            account: client.account,
            domain: client.domain.clone(),
            expires_at: self.h.ts + 60_000,
        }
    }

    /// Applies `cmd` to the three engines, which must give the same events.
    fn apply(&mut self, cmd: Command) -> Vec<Event> {
        let sequenced = Sequenced { seq: self.h.log.len() as u64, ts: self.h.ts, cmd };
        let events = self.h.engine.apply(&sequenced).expect("in sequence");
        assert_eq!(
            self.full.apply(&sequenced).expect("in sequence"),
            events,
            "the full check differs at {}",
            sequenced.seq
        );
        assert_eq!(
            self.restored.apply(&sequenced).expect("in sequence"),
            events,
            "the restored engine differs at {}",
            sequenced.seq
        );
        self.closeouts += events.iter().filter(|e| matches!(e, Event::CloseOut { .. })).count();
        self.h.log.push(sequenced);
        self.check_requirements();
        events
    }

    /// Checks every account's portfolio requirement at the marks is at most its per-position one and at
    /// least the lowest floor share of it, which the production grid's groups make a test on grouped books.
    fn check_requirements(&self) {
        let engine = &self.h.engine;
        let grid = engine.portfolio_rules().expect("the production genesis names a grid");
        let least = grid.groups.iter().map(|group| group.floor_share_bps).min().expect("groups");
        for (account, acct) in engine.accounts().iter().filter(|(_, acct)| !acct.positions.is_empty()) {
            let (_, portfolio) = engine.equity_and_closeout_requirement(acct);
            let (_, bands) = engine.equity_and_margin(acct, None);
            assert!(portfolio <= bands && portfolio >= bands.mul_bps(least), "{account:?}: {portfolio} of {bands}");
        }
    }

    /// The three engines' state roots, which must be equal.
    fn compare_roots(&mut self) {
        let root = self.h.engine.state_root();
        assert_eq!(self.full.state_root(), root, "the full check's root differs");
        assert_eq!(self.restored.state_root(), root, "the restored engine's root differs");
    }
}

/// `engine` encoded and decoded, with its derived indexes restored.
fn copy(engine: &Engine) -> Engine {
    let mut copy: Engine = serde_json::from_str(&serde_json::to_string(engine).unwrap()).unwrap();
    copy.restore_indexes();
    copy
}

/// Adds `seen` to `total`.
fn add(total: &mut CloseoutCoverage, seen: CloseoutCoverage) {
    total.price_crossings += seen.price_crossings;
    total.fx_crossings += seen.fx_crossings;
    total.headroom_moves += seen.headroom_moves;
    total.keys += seen.keys;
    total.weekend_skips += seen.weekend_skips;
    total.weekend_closeouts += seen.weekend_closeouts;
    total.weekend_revaluations += seen.weekend_revaluations;
    total.carried_over += seen.carried_over;
    total.skipped_calls += seen.skipped_calls;
}
