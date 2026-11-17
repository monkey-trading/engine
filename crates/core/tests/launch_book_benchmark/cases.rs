//! The launch book's cases, each timing one command of the engine's own `apply`, with its inputs
//! built beforehand.

use engine_core::Engine;
use engine_core::access::AccessDeposit;
use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::types::{AccountId, MarketId, Side};

use super::common::close_spread::evidence;
use super::common::launch_book::{Book, CLOSEOUT_MARKET, MARK, benchmark_batch, closeout_mark, price_command};
use super::common::session_keys::trade_cmd;
use super::common::{BTC, Client, f};
use super::report::{report, rss_mib, timed, within};

const PRICE_SAMPLES: usize = 300;
/// A BTC-USD, an FX and a futures-priced index market.
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

/// The state root over 1k, 10k and 100k changed accounts, and an account's proof.
pub fn roots_and_proofs(book: &mut Book, accounts: usize, budget_100k_us: u128) {
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
        println!("LAUNCH root changed={changed} us={us} us_per_account={:.2}", us as f64 / changed as f64);
        if changed >= 100_000 || changed == fillers {
            within(&format!("root_over_{changed}_changed"), us, budget_100k_us);
        }
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
pub fn prices(book: &mut Book, budget_p99_us: u128) {
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
        let holders = book.holders[&market];
        let (p50, p99, _) = report(name, &mut times, "");
        println!(
            "LAUNCH {name}_rate holders={holders} us_per_holder={:.3} prices_per_second={:.1}",
            p50 as f64 / holders as f64,
            1e6 / p50 as f64
        );
        within(&format!("{name}_p99"), p99, budget_p99_us);
    }
}

/// One price in every market, in turn, `ROUNDS` times: a round is what the sequencer's thread does
/// if every market is priced once, whose time sets how many such rounds a second it could take.
pub fn price_rounds(book: &mut Book) {
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
    let per_market: Vec<String> = ids
        .iter()
        .zip(&mut by_market)
        .map(|(id, times)| {
            times.sort_unstable();
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
pub fn signed_orders(book: &mut Book, budget_p99_us: u128) {
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
        let (_, p99, _) = report(name, &mut times, "");
        within(&format!("{name}_p99"), p99, budget_p99_us);
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
    let holders = book.holders[&CLOSEOUT_MARKET];
    let (_, p99, _) =
        report("close_out_all_34_price", &mut times, &format!("market={CLOSEOUT_MARKET} holders={holders}"));
    within("close_out_all_34_p99", p99, budget_us);
}

/// The daily rollover, which charges every position its financing, with each benchmark's rate
/// published first. The weekend's gap follows the fourth.
pub fn rollover(book: &mut Book, budget_us: u128) {
    let total = |book: &Book| -> engine_core::Fixed { book.h.engine.accounts().values().map(|a| a.balance).sum() };
    let mut times = Vec::new();
    for _ in 0..4 {
        let due = book.h.engine.next_rollover().expect("a rollover is due");
        book.h.advance_to(due - 1);
        let batch = benchmark_batch(&book.h);
        let events = book.operate(batch);
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
        book.h.advance_to(due);
        let before = total(book);
        let (events, us) = timed(|| book.apply(Command::Tick));
        assert!(total(book) < before, "the rollover charges financing: {events:?}");
        times.push(us);
    }
    let positions: usize = book.h.engine.accounts().values().map(|a| a.positions.len()).sum();
    let extra = format!("positions={positions} us_per_position_p50={:.3}", times_p50(&times) as f64 / positions as f64);
    let (_, p99, _) = report("rollover", &mut times, &extra);
    within("rollover_max", p99, budget_us);
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
    let (text, us) = timed(|| serde_json::to_string(&book.h.engine).expect("encoding"));
    println!("LAUNCH snapshot_encoding us={us} bytes={}", text.len());
    let before = rss_mib();
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
