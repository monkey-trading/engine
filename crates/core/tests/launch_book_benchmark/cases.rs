//! The launch book's cases, each timing one command of the engine's own `apply`, with its inputs
//! built beforehand.

use engine_core::access::AccessDeposit;
use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, MarketId, Side};
use engine_core::{Engine, Fixed};

use crate::common::close_spread::evidence;
use crate::common::launch_book::{Book, CLOSEOUT_MARKET, MARK, Rng, benchmark_batch, closeout_mark, price_command};
use crate::common::session_keys::trade_cmd;
use crate::common::{BTC, Client, f};
use crate::report::{noted, report, rss_mib, timed, within};

const PRICE_SAMPLES: usize = 300;
/// The FX market and the futures-priced index market the price cases use beside BTC-USD.
const EURUSD: MarketId = 10;
const US500: MarketId = 20;

fn is_trade(events: &[Event]) -> bool {
    events.iter().any(|e| matches!(e, Event::Fill(_) | Event::OrderCancelled { .. } | Event::CloseOut { .. }))
}

/// What the book holds, and the memory it takes.
pub fn summary(book: &Book, accounts: usize, rss_before: u64, rss_after: u64) {
    let engine = &book.h.engine;
    println!(
        "LAUNCH book accounts={accounts} markets={} positions={} orders={} resting_accounts={} tiers={:?} \
         encoded_mib={} assembled_s={:.1} resident_mib={rss_after} resident_added_mib={}",
        engine.markets().len(),
        book.positions,
        book.orders,
        book.resting_accounts,
        book.tiers,
        book.encoded_bytes >> 20,
        book.assembled_in.as_secs_f64(),
        rss_after.saturating_sub(rss_before),
    );
    let most = book.holders.iter().max_by_key(|(_, held)| **held).expect("holders");
    let held = |id: &MarketId| book.holders.get(id).copied().unwrap_or(0);
    println!(
        "LAUNCH book_holders most_held={}:{} btc={} eurusd={} us500={} fewest={}",
        most.0,
        most.1,
        held(&BTC),
        held(&EURUSD),
        held(&US500),
        book.holders.values().min().expect("holders")
    );
}

/// Credits `account` a dollar through the screened deposit, which changes its leaf.
fn touch(book: &mut Book, account: AccountId, tag: usize) {
    let deposit = AccessDeposit {
        account: Some(account),
        payer: Some(account),
        amount: f("1"),
        chain_ref: format!("0xtouch:{tag}:{account}"),
    };
    let ev = evidence(&book.h, eip712::deposit_screening_hash(&deposit), Some(account), Some(account));
    let events = book.apply(Command::AccessDeposit { deposit, evidence: ev });
    assert!(matches!(events[..], [Event::Deposited { .. }]), "{events:?}");
}

/// The state root after 1k, 10k and all other accounts are credited, and an account's proof. A credit
/// changes the account's leaf and adds one for its funding reference, so a round hashes twice the
/// accounts it names; the root over account changes alone is `rollover`'s.
pub fn roots_and_proofs(book: &mut Book, accounts: usize) {
    let (_, first) = timed(|| book.h.engine.state_root());
    println!("LAUNCH root_first accounts={accounts} us={first}");
    let fillers = (accounts - book.named_total()) as u64;
    for (round, changed) in [1_000u64, 10_000, 100_000].into_iter().enumerate() {
        let changed = changed.min(fillers);
        for index in 0..changed {
            let account = book.account(index);
            touch(book, account, round);
        }
        let (_, us) = timed(|| book.h.engine.state_root());
        println!(
            "LAUNCH root_after_credits changed={changed} leaves={} us={us} us_per_account={:.2}",
            2 * changed,
            us as f64 / changed as f64
        );
    }
    let holder = book.named.adders[0].account;
    let mut times: Vec<u128> = (0..100)
        .map(|_| {
            let (proof, us) = timed(|| book.h.engine.account_proof(holder));
            assert_eq!(proof.expect("the root holds the account").positions.len(), book.h.engine.markets().len());
            us
        })
        .collect();
    report("account_proof_34_markets", &mut times, "");
}

/// A price in BTC-USD, in an FX market and in a futures-priced index market.
pub fn prices(book: &mut Book, budget_p99_us: u128, single_us: u128) {
    for (name, market) in [("price_btc_usd", BTC), ("price_fx_eur_usd", EURUSD), ("price_index_us500", US500)] {
        let mut times = Vec::with_capacity(PRICE_SAMPLES);
        for sample in 0..PRICE_SAMPLES {
            book.h.advance(1);
            let mid = if sample % 2 == 0 { "100.0001" } else { "100" };
            let cmd = price_command(&book.h, market, mid, book.h.ts);
            let (events, us) = timed(|| book.apply(cmd));
            assert!(!is_trade(&events), "{name}: a price that only moves the mark trades: {events:?}");
            times.push(us);
        }
        let holders = book.holders.get(&market).copied().unwrap_or(0);
        let (p50, p99, max) = report(name, &mut times, "");
        within(&format!("{name}_max"), max, single_us);
        println!(
            "LAUNCH {name}_rate holders={holders} us_per_holder={:.3} prices_per_second={:.1}",
            p50 as f64 / holders.max(1) as f64,
            1e6 / p50 as f64
        );
        within(&format!("{name}_p99"), p99, budget_p99_us);
    }
}

/// One price in every market, in turn, `ROUNDS` times: a round is what the sequencer's thread does
/// if every market is priced once, whose time sets how many such rounds a second it could take.
pub fn price_rounds(book: &mut Book, budget_us: u128) {
    const ROUNDS: usize = 15;
    let ids: Vec<MarketId> = book.h.engine.markets().keys().copied().collect();
    let mut rounds = Vec::with_capacity(ROUNDS);
    let mut by_market: Vec<Vec<u128>> = vec![Vec::with_capacity(ROUNDS); ids.len()];
    for round in 0..ROUNDS {
        let mut total = 0;
        for (slot, id) in ids.iter().enumerate() {
            book.h.advance(1);
            let mid = if round % 2 == 0 { "100.0001" } else { "100" };
            let cmd = price_command(&book.h, *id, mid, book.h.ts);
            let (events, us) = timed(|| book.apply(cmd));
            assert!(!is_trade(&events), "{events:?}");
            by_market[slot].push(us);
            total += us;
        }
        rounds.push(total);
    }
    // Every market's slowest price of the rounds is held to the budget, not only the three above.
    let per_market: Vec<String> = ids
        .iter()
        .zip(&mut by_market)
        .map(|(id, times)| {
            times.sort_unstable();
            within(&format!("price_round_market_{id}_max"), times[times.len() - 1], budget_us);
            format!("{id}:{}", times[times.len() / 2])
        })
        .collect();
    let (p50, ..) = report("price_round_all_markets", &mut rounds, &format!("markets={}", ids.len()));
    println!(
        "LAUNCH price_round_rate rounds_per_second={:.2} p50_us_by_market={}",
        1e6 / p50 as f64,
        per_market.join(",")
    );
}

/// `prices` prices of BTC-USD walking from where it is about a basis point a price, with a jump of a
/// fifth of a percent to a percent one price in fifty, as a price at launch moves, printed under
/// `name` with the accounts they close out; returns the p99.
pub fn price_walk(book: &mut Book, name: &str, prices: usize) -> u128 {
    let mut draws = Rng::new(0x7761_6c6b);
    let mut mid = Fixed::from_int(MARK);
    let (mut times, mut closed) = (Vec::with_capacity(prices), 0);
    for _ in 0..prices {
        book.h.advance(1);
        let step = if draws.below(50) == 0 { 20 + draws.below(81) as i64 } else { draws.below(3) as i64 };
        mid = mid.mul_bps(if draws.below(2) == 0 { 10_000 + step } else { 10_000 - step });
        let cmd = price_command(&book.h, BTC, &mid.to_string(), book.h.ts);
        let (events, us) = timed(|| book.apply(cmd));
        closed += events.iter().filter(|e| matches!(e, Event::CloseOut { .. })).count();
        times.push(us);
    }
    let holders = book.holders[&BTC];
    let (_, p99, _) = report(name, &mut times, &format!("holders={holders} closeouts={closed} end_mid={mid}"));
    p99
}

/// A price in an FX market and in BTC-USD on `book`, printed under `variant`.
pub fn price_attribution(book: &mut Book, variant: &str) {
    for (name, market) in [("fx_eur_usd", EURUSD), ("btc_usd", BTC)] {
        let mut times = Vec::with_capacity(100);
        for sample in 0..100 {
            book.h.advance(1);
            let mid = if sample % 2 == 0 { "100.0001" } else { "100" };
            let cmd = price_command(&book.h, market, mid, book.h.ts);
            let (events, us) = timed(|| book.apply(cmd));
            assert!(!is_trade(&events), "{events:?}");
            times.push(us);
        }
        let holders = book.holders[&market];
        report(
            &format!("attribution_{variant}_{name}"),
            &mut times,
            &format!("holders={holders} orders={}", book.orders),
        );
    }
}

/// A signed, screened market order for `qty` of BTC-USD from `client`.
fn order(book: &Book, client: &Client, qty: &str) -> Command {
    let request = OrderRequest {
        account: client.account,
        client_id: 7,
        market: BTC,
        side: Side::Buy,
        qty: f(qty),
        kind: OrderKind::Market { quoted: f(&MARK.to_string()), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    trade_cmd(&book.h, client, book.h.engine.next_request_nonce(client.account), request)
}

/// A signed market order that opens a position, and one that adds to a position of an account that
/// holds every market. The commands are signed first, so the timing is the engine's apply of each.
pub fn signed_orders(book: &mut Book, budget_p99_us: u128, single_us: u128) {
    let qty = "50".to_string();
    for (name, accounts) in
        [("signed_open_btc", book.named.openers.len()), ("signed_add_all_34", book.named.adders.len())]
    {
        let clients = if name == "signed_open_btc" { &book.named.openers } else { &book.named.adders };
        let commands: Vec<Command> = clients.iter().map(|client| order(book, client, &qty)).collect();
        assert_eq!(commands.len(), accounts);
        let mut times = Vec::with_capacity(accounts);
        for cmd in commands {
            book.h.advance(1);
            let (events, us) = timed(|| book.apply(cmd));
            assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{name}: {events:?}");
            times.push(us);
        }
        let (_, p99, max) = report(name, &mut times, "");
        within(&format!("{name}_p99"), p99, budget_p99_us);
        within(&format!("{name}_max"), max, single_us);
    }
}

/// A price of `CLOSEOUT_MARKET` that closes out one account holding every market, each position at
/// its own mark: the accounts' cash sets them to fall below the close-out level one step of the price
/// after another.
pub fn close_outs(book: &mut Book, budget_us: u128) {
    let others: Vec<MarketId> = book.h.engine.markets().keys().copied().filter(|id| *id != CLOSEOUT_MARKET).collect();
    let markets = book.h.engine.markets().len();
    let mut times = Vec::new();
    let closeouts: Vec<AccountId> = book.named.closeouts.iter().map(|client| client.account).collect();
    for (step, account) in closeouts.iter().enumerate() {
        // A new move window, and every market quoted now, so every position can close.
        book.h.advance(70_000);
        for id in &others {
            let cmd = price_command(&book.h, *id, "100", book.h.ts);
            let events = book.apply(cmd);
            assert!(!is_trade(&events), "{events:?}");
        }
        let cmd = price_command(&book.h, CLOSEOUT_MARKET, &closeout_mark(step + 1), book.h.ts);
        let (events, us) = timed(|| book.apply(cmd));
        let closed: Vec<AccountId> = events
            .iter()
            .filter_map(|e| if let Event::CloseOut { account, .. } = e { Some(*account) } else { None })
            .collect();
        assert_eq!(closed, [*account], "step {step} closes out its account alone");
        let fills = events.iter().filter(|e| matches!(e, Event::Fill(_))).count();
        assert_eq!(fills, markets, "the account's position in every market closes");
        times.push(us);
    }
    let holders = book.holders.get(&CLOSEOUT_MARKET).copied().unwrap_or(0);
    let (_, p99, _) =
        report("close_out_all_34_price", &mut times, &format!("market={CLOSEOUT_MARKET} holders={holders}"));
    within("close_out_all_34_p99", p99, budget_us);
}

/// The daily rollover, which charges every position its financing, with each benchmark's rate
/// published first, and the state root after the last, which hashes every account it changed and
/// nothing else. The weekend's gap follows the fourth rollover.
pub fn rollover(book: &mut Book, bar: Bar, root: Bar) {
    const ROLLOVERS: usize = 4;
    let total = |book: &Book| -> engine_core::Fixed { book.h.engine.accounts().values().map(|a| a.balance).sum() };
    let positions: usize = book.h.engine.accounts().values().map(|a| a.positions.len()).sum();
    let mut times = Vec::new();
    let mut financed = (0, 0);
    for rollover in 0..ROLLOVERS {
        let due = book.h.engine.next_rollover().expect("a rollover is due");
        book.h.advance_to(due - 1);
        let batch = benchmark_batch(&book.h);
        let events = book.operate(batch);
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        reprice_open_markets(book, due);
        if rollover == ROLLOVERS - 1 {
            // The root timed below hashes what this rollover changed, not what the cases before it did.
            book.h.engine.state_root();
        }
        book.h.advance_to(due);
        let before = total(book);
        let (events, us) = timed(|| book.apply(Command::Tick));
        assert!(total(book) < before, "the rollover charges financing: {events:?}");
        times.push(us);
        let charged: Vec<AccountId> = events
            .iter()
            .filter_map(|e| if let Event::Financing { account, .. } = e { Some(*account) } else { None })
            .collect();
        financed = (charged.len(), charged.iter().collect::<std::collections::BTreeSet<_>>().len());
    }
    // Every position pays or earns financing at the published rate, so no account is skipped.
    assert!(financed.0 * 100 >= positions * 99, "{} of {positions} positions financed", financed.0);
    let extra = format!(
        "positions={positions} financed={} us_per_position_p50={:.3}",
        financed.0,
        times_p50(&times) as f64 / positions as f64
    );
    let (_, _, max) = report("rollover", &mut times, &extra);
    noted("rollover_max_bar", max, bar.bar_us);
    within("rollover_max_guard", max, bar.guard_us);
    // The root after the last rollover hashes the leaf of each account it financed, and no other
    // account's.
    let (_, us) = timed(|| book.h.engine.state_root());
    println!(
        "LAUNCH root_after_rollover changed={} us={us} us_per_account={:.2}",
        financed.1,
        us as f64 / financed.1 as f64
    );
    noted("root_after_rollover_bar", us, root.bar_us);
    within("root_after_rollover_guard", us, root.guard_us);
}

/// Quotes every market at its own mark, so no price moves, one millisecond before `due` and outside the timed rollover, as a
/// launch node's feeds keep the open markets priced; a market outside its session, or whose price
/// rules keep a quote for less than a millisecond, takes no price from it. Asserts the always-open
/// markets, which the rollover's close-out sweep reads, have an index when the rollover runs.
fn reprice_open_markets(book: &mut Book, due: i64) {
    let marks: Vec<(MarketId, String)> =
        book.h.engine.markets().iter().filter_map(|(id, m)| m.mark.map(|mark| (*id, mark.to_string()))).collect();
    for (id, mark) in marks {
        let cmd = price_command(&book.h, id, &mark, book.h.ts);
        let events = book.apply(cmd);
        assert!(!is_trade(&events), "{id}: {events:?}");
    }
    let unpriced: Vec<MarketId> = book
        .h
        .engine
        .markets()
        .iter()
        .filter(|(_, market)| matches!(market.spec.schedule, Schedule::AlwaysOpen) && market.index(due).is_err())
        .map(|(id, _)| *id)
        .collect();
    assert!(unpriced.is_empty(), "always-open markets with no index at the rollover: {unpriced:?}");
}

/// The budgets of a case the book does not meet: #904's bar, which is only noted, and the guard CI
/// holds it to until the bar is met.
#[derive(Clone, Copy)]
pub struct Bar {
    pub bar_us: u128,
    pub guard_us: u128,
}

/// The median of `times`, which need not be sorted.
fn times_p50(times: &[u128]) -> u128 {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

/// The engine's encoding for a snapshot, and the recovery of it: decoding, rebuilding the
/// transient indexes, and the first command after.
pub fn encoding_and_recovery(book: &mut Book) {
    let before = rss_mib();
    let (text, us) = timed(|| serde_json::to_string(&book.h.engine).expect("encoding"));
    println!("LAUNCH snapshot_encoding us={us} bytes={}", text.len());
    let (mut copy, decode) = timed(|| serde_json::from_str::<Engine>(&text).expect("decoding"));
    drop(text);
    let (_, restore) = timed(|| copy.restore_indexes());
    let seq = copy.last_seq().map_or(0, |seq| seq + 1);
    let ts = copy.now() + 1;
    let (_, first) = timed(|| copy.apply(&engine_core::command::Sequenced { seq, ts, cmd: Command::Tick }));
    let (_, second) =
        timed(|| copy.apply(&engine_core::command::Sequenced { seq: seq + 1, ts: ts + 1, cmd: Command::Tick }));
    println!(
        "LAUNCH recovery decode_us={decode} restore_indexes_us={restore} first_command_us={first} \
         second_command_us={second} copy_resident_mib={}",
        rss_mib().saturating_sub(before)
    );
}
