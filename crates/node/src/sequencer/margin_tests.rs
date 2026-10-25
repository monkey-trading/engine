use super::*;
use crate::sequencer::fixture_tests::replay_trusted as replay;
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use engine_core::command::{OrderKind, OrderRequest};
use engine_core::event::Reject;
use engine_core::price::{PriceError, SourceQuote};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Side};
use tracing_subscriber::fmt::MakeWriter;

use crate::clock::test_clock::TestClock;
use crate::config::Genesis;
use crate::test_support::{TRADER, deposit, dev_config, log_entries, sample_around, scratch_dir, sequencer_config};

const BTC: MarketId = 1;
const ETH: MarketId = 2;
/// An account that holds ETH while its quotes go stale.
const HOLDER: AccountId = AccountId([8; 20]);
/// Tue 20 Oct 2026 10:00 New York.
const START: Millis = 1_792_504_800_000;

/// The feature's dev scenario, on the committed dev genesis under the latest protocol version:
/// financing at the rollover takes a position held just above the close-out level below it and
/// closes it out; then, with ETH's quotes stale, a withdrawal is limited to the free cash at ETH's
/// last mark, and ETH allows no dealing. The log then replays.
#[test]
fn dev_scenario_financing_closes_out_and_a_stale_market_counts_at_its_last_mark() {
    let genesis = dev_config().genesis;
    let rollover = genesis.params.rollover.next_after(START);
    let clock = Arc::new(TestClock::at(rollover - 600_000));
    let now = || clock.now_ms();
    // Moves the clock to `ts` and takes a trusted time sample there.
    let move_to = |seq: &mut Sequencer, ts: Millis| {
        clock.advance(Duration::from_millis((ts - clock.now_ms()) as u64));
        seq.trust(sample_around(clock.as_ref())).unwrap();
    };

    let dir = scratch_dir("margin-dev-scenario");
    let (mut seq, _withdrawals) = recover_at(&dir, clock.clone(), &genesis);
    seq.activate_protocol().unwrap();
    seq.genesis(&genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();

    // 0.1 BTC bought at 100,000 and marked at 94,740: equity 474 against a close-out level of 473.7.
    sequenced(&mut seq, deposit(1000, "dep-1"));
    sequenced(&mut seq, prices(BTC, "100000", now()));
    sequenced(&mut seq, market_order(TRADER, BTC, Side::Buy, "0.1", "100000"));
    sequenced(&mut seq, prices(BTC, "94740", now()));
    // Across the 22:00 London rollover, a day's financing at 900bps a year on 9,474 is 2.336054795,
    // rounded up.
    move_to(&mut seq, rollover - 500);
    sequenced(&mut seq, prices(BTC, "94740", now()));
    move_to(&mut seq, rollover + 100);
    let events = sequenced(&mut seq, Command::Tick);
    let financing = Fixed::from_raw(2_336_054_795);
    assert!(events.contains(&Event::Financing { account: TRADER, market: BTC, amount: financing }), "{events:?}");
    let equity = fixed("474") - financing;
    assert!(events.contains(&Event::CloseOut { account: TRADER, equity, margin: fixed("947.4") }), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::Fill(fill) if fill.liquidation && fill.qty == fixed("0.1"))));

    // ETH bought at 4,000 and marked at 3,800, then left without quotes past its 1s age limit.
    let holder_deposit = Command::Deposit { account: HOLDER, amount: fixed("1000"), chain_ref: "dep-2".into() };
    sequenced(&mut seq, holder_deposit);
    sequenced(&mut seq, prices(ETH, "4000", now()));
    sequenced(&mut seq, market_order(HOLDER, ETH, Side::Buy, "1", "4000"));
    sequenced(&mut seq, prices(ETH, "3800", now()));
    move_to(&mut seq, rollover + 1_200);
    sequenced(&mut seq, prices(BTC, "94740", now()));
    // Equity 800 against 380 of margin at the last mark leaves 420 free.
    let events = sequenced(&mut seq, withdrawal(HOLDER, "420.000000001"));
    let refused = Reject::InsufficientFunds { available: fixed("420") };
    assert!(matches!(&events[..], [Event::WithdrawalRejected { reason, .. }] if *reason == refused), "{events:?}");
    let events = sequenced(&mut seq, withdrawal(HOLDER, "420"));
    assert!(matches!(&events[..], [Event::WithdrawalApproved { .. }]), "{events:?}");
    let events = sequenced(&mut seq, market_order(HOLDER, ETH, Side::Sell, "1", "3800"));
    let stale = Reject::NoPrice { error: PriceError::Stale { fresh: 0, needed: 2 } };
    assert!(matches!(&events[..], [Event::OrderRejected { reason, .. }] if *reason == stale), "{events:?}");

    seq.flush().unwrap();
    drop(seq);
    let replayed = replay(&log_entries(&dir)).unwrap();
    assert_eq!(replayed.engine.protocol_version(), ProtocolVersion::LATEST);
}

/// The dev genesis with no house capital boots and completes once; an opening trade fills within
/// BTC's floor at zero NAV; and house NAV falling below zero alerts, holds the alert back when NAV
/// crosses zero again within the window, and alerts again when a restart finds the log in deficit.
#[test]
fn a_house_with_no_capital_opens_trading_and_alerts_on_a_deficit() {
    let clock = Arc::new(TestClock::at(START));
    let mut genesis = dev_config().genesis;
    genesis.house_capital = Fixed::ZERO;
    let dir = scratch_dir("zero-capital-house");
    let (mut seq, _withdrawals) = recover_at(&dir, clock.clone(), &genesis);
    seq.activate_protocol().unwrap();
    seq.genesis(&genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    assert!(!seq.needs_genesis());
    assert_eq!(seq.engine.house_nav(), Fixed::ZERO);

    sequenced(&mut seq, deposit(100_000, "dep-1"));
    sequenced(&mut seq, prices(BTC, "100000", START));
    // 200,000 of BTC fits its 300,000 floor.
    let events = sequenced(&mut seq, market_order(TRADER, BTC, Side::Buy, "2", "100000"));
    assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");

    // At 110,000 the house owes the trader 20,000; 111,000 deepens the deficit, 99,000 ends it, and
    // 110,000 opens another within the same minute, which the throttle holds back.
    let ((), logs) = captured(|| {
        for mid in ["110000", "111000", "99000", "110000"] {
            sequenced(&mut seq, prices(BTC, mid, START));
        }
    });
    assert_eq!(logs.matches(r#"alert="house_deficit""#).count(), 1, "{logs}");
    assert!(logs.contains("(0 more since the last alert)"), "{logs}");
    seq.flush().unwrap();
    drop(seq);

    let ((seq, _withdrawals), logs) = captured(|| recover_at(&dir, clock.clone(), &genesis));
    assert!(!seq.needs_genesis());
    assert_eq!(logs.matches(r#"alert="house_deficit""#).count(), 1, "{logs}");
}

// ── Helpers ──

/// A sequencer recovered from the log in `dir` under `genesis`, stamping commands with `clock`, and
/// what custody would receive from it.
fn recover_at(
    dir: &Path,
    clock: Arc<TestClock>,
    genesis: &Genesis,
) -> (Sequencer, mpsc::UnboundedReceiver<SignedWithdrawal>) {
    let (stream, _) = broadcast::channel(16);
    let (withdrawals_tx, withdrawals) = mpsc::unbounded_channel();
    let (manifest, _) = watch::channel(Manifest::default());
    let cfg = SequencerConfig { clock, genesis: genesis_commands(genesis), ..sequencer_config(dir, manifest) };
    (Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, withdrawals_tx).unwrap(), withdrawals)
}

/// The events `cmd` produced; panics if trusted time refused its stamp.
fn sequenced(seq: &mut Sequencer, cmd: Command) -> Vec<Event> {
    seq.sequence(cmd).unwrap().expect("trusted time admits the stamp")
}

/// What `f` returns, and the log lines it wrote.
fn captured<T>(f: impl FnOnce() -> T) -> (T, String) {
    let lines = Lines::default();
    let subscriber = tracing_subscriber::fmt().with_writer(lines.clone()).with_ansi(false).finish();
    let out = tracing::subscriber::with_default(subscriber, f);
    let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    (out, text)
}

/// A log sink the test reads back.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Write for Lines {
    /// Appends to the buffer.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    /// Nothing is buffered.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Lines {
    type Writer = Lines;

    /// A handle onto the same buffer.
    fn make_writer(&'a self) -> Lines {
        self.clone()
    }
}

/// Parses a decimal.
fn fixed(s: &str) -> Fixed {
    s.parse().unwrap()
}

/// Two venues quoting `mid` for `market`, stamped `ts`.
fn prices(market: MarketId, mid: &str, ts: Millis) -> Command {
    let quotes =
        ["venue-a", "venue-b"].map(|source| SourceQuote { source: source.into(), mid: fixed(mid), ts, contract: None });
    Command::Prices { market, quotes: quotes.to_vec() }
}

/// An unsigned market order quoted at `quoted`, with the market's slippage bound.
fn market_order(account: AccountId, market: MarketId, side: Side, qty: &str, quoted: &str) -> Command {
    Command::PlaceOrder(OrderRequest {
        account,
        client_id: 1,
        market,
        side,
        qty: fixed(qty),
        kind: OrderKind::Market { quoted: fixed(quoted), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    })
}

/// An unsigned withdrawal of `amount`.
fn withdrawal(account: AccountId, amount: &str) -> Command {
    Command::Withdraw { account, amount: fixed(amount), auth: None }
}
