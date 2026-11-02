//! Which losses earn MONKEY from `V19`, through `Engine::apply`: netting within a period before
//! `V43` and gross loss magnitudes from `V43` on, fees and financing left out, liquidations counted
//! less written-off debt, negative cash at the cutoff, and the signed one-time initialisation that
//! starts the clock.

mod common;

use common::close_spread::{attest, fund, order};
use common::emissions::{allocations, funded, next_period, round_trip, schedule};
use common::events::fills;
use common::{BTC, Client, ETH, Harness, acct, f};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::{EmissionSchedule, MAX_PERIOD_BUDGET, PERIOD_MS, TokenUnits};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MAX_STAMP_MS, Side};

const BUDGET: u128 = 1_000_000;

#[test]
fn a_win_offsets_a_loss_in_the_same_period_before_the_profit_fee() {
    let mut h = Harness::emitting(BUDGET);
    let [a, b] = funded(&mut h);
    round_trip(&mut h, &[(a, "1")], "100000", "99900");
    round_trip(&mut h, &[(a, "1")], "100000", "100080");
    round_trip(&mut h, &[(b, "0.6")], "100000", "99900");
    let result = h.engine.period_results()[&a];
    assert_eq!((result.realised, result.written_off), (f("-20"), Fixed::ZERO));
    // The 8 of profit fee a paid on its win does not count as loss: a's weight is 20, not 28.
    assert_eq!(allocations(&next_period(&mut h)), [(a, 0, f("20"), 250_000), (b, 0, f("60"), 750_000)]);
}

#[test]
fn a_losing_fill_earns_weight_in_a_period_the_account_ends_net_positive() {
    // The owner's scenario, from `V43`: an account with prior gains takes a losing fill inside a
    // period its wins leave net positive, and that fill still earns its own weight.
    let mut h = Harness::emitting_gross(BUDGET);
    let (a, b) = (Client::new(1), Client::new(2));
    attest_and_fund(&mut h, &a);
    attest_and_fund(&mut h, &b);
    // Period 0: a's gain is prior profit, not weight.
    screened_round_trip(&mut h, &[(&a, "1")], "100000", "100100");
    assert!(allocations(&next_period(&mut h)).is_empty());

    // Period 1: a loses 100 on one fill and wins 150 on another, ending the period net positive
    // 50; b loses 60. The win does not offset: a's weight is its fill's full loss magnitude.
    screened_round_trip(&mut h, &[(&a, "1")], "100000", "99900");
    screened_round_trip(&mut h, &[(&a, "1")], "99900", "100050");
    screened_round_trip(&mut h, &[(&b, "0.6")], "100000", "99900");
    let result = h.engine.period_results()[&a.account];
    assert_eq!((result.realised, result.losses), (f("50"), f("100")));
    assert_eq!(
        allocations(&next_period(&mut h)),
        [(a.account, 1, f("100"), 625_000), (b.account, 1, f("60"), 375_000)]
    );
}

#[test]
fn wins_never_offset_losses_in_the_same_period_from_v43() {
    let mut h = Harness::emitting_gross(BUDGET);
    let a = Client::new(1);
    attest_and_fund(&mut h, &a);
    screened_round_trip(&mut h, &[(&a, "1")], "100000", "99900");
    screened_round_trip(&mut h, &[(&a, "1")], "99900", "99960");
    screened_round_trip(&mut h, &[(&a, "1")], "99960", "99920");
    let result = h.engine.period_results()[&a.account];
    assert_eq!((result.realised, result.losses), (f("-80"), f("140")));
    assert_eq!(allocations(&next_period(&mut h)), [(a.account, 0, f("140"), BUDGET)]);
}

#[test]
fn a_v43_activation_crossing_a_period_settles_its_earlier_fills_gross() {
    // Losses accrued before the activation keep their weight: the period a `V43` activation
    // crosses finalises under `V43`, and the gross losses accumulate under every version for
    // exactly this settlement.
    let mut h = Harness::emitting_at(ProtocolVersion::V41, BUDGET);
    let (a, b) = (Client::new(1), Client::new(2));
    attest_and_fund(&mut h, &a);
    attest_and_fund(&mut h, &b);
    // One period, all under `V41`: a loses 100 on one fill and wins 20 on another; b loses 60.
    screened_round_trip(&mut h, &[(&a, "1")], "100000", "99900");
    screened_round_trip(&mut h, &[(&a, "1")], "99900", "99920");
    screened_round_trip(&mut h, &[(&b, "0.6")], "100000", "99900");
    h.engine.state_root();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V43 });
    // Activation rehashes every leaf under the V43 encodings: the incremental root, including the
    // open period results, equals a rebuild's.
    let incremental = h.engine.state_root();
    let mut rebuilt: engine_core::Engine = serde_json::from_slice(&serde_json::to_vec(&h.engine).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), incremental);
    assert_eq!(
        allocations(&next_period(&mut h)),
        [(a.account, 0, f("100"), 625_000), (b.account, 0, f("60"), 375_000)]
    );
}

#[test]
fn before_v43_netting_still_applies() {
    // The gross rule activates at `V43`: one version earlier a win still offsets the period's
    // losses, exactly as every pre-`V43` log records it.
    let mut h = Harness::emitting_at(ProtocolVersion::V41, BUDGET);
    let (a, b) = (Client::new(1), Client::new(2));
    attest_and_fund(&mut h, &a);
    attest_and_fund(&mut h, &b);
    screened_round_trip(&mut h, &[(&a, "1")], "100000", "99900");
    screened_round_trip(&mut h, &[(&a, "1")], "99900", "99980");
    screened_round_trip(&mut h, &[(&b, "0.6")], "100000", "99900");
    assert_eq!(allocations(&next_period(&mut h)), [(a.account, 0, f("20"), 250_000), (b.account, 0, f("60"), 750_000)]);
}

/// Attests `client` eligible and funds it with a screened deposit, as the access rules a `V43`
/// harness keeps require.
fn attest_and_fund(h: &mut Harness, client: &Client) {
    attest(h, client);
    fund(h, client, "1000000");
}

/// Each client buys its quantity of BTC at `from` and sells it at `to`, through screened orders.
fn screened_round_trip(h: &mut Harness, trades: &[(&Client, &str)], from: &str, to: &str) {
    for (price, side) in [(from, Side::Buy), (to, Side::Sell)] {
        h.price(BTC, price);
        for (client, qty) in trades {
            let events = order(h, client, BTC, side, qty, price, false);
            assert_eq!(fills(&events).len(), 1, "{events:?}");
        }
    }
}

#[test]
fn financing_paid_in_a_period_adds_no_weight() {
    let mut h = Harness::emitting(BUDGET);
    h.operator_admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 500 });
    let [a, b] = funded(&mut h);
    h.price(BTC, "100000");
    assert_eq!(fills(&h.market_order(a, BTC, Side::Buy, "1", "100000")).len(), 1);
    h.advance_to(h.engine.next_rollover().unwrap());
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|e| matches!(e, Event::Financing { account, .. } if *account == a)), "{events:?}");
    // Closed at its entry price, the position realises nothing: the financing it paid is no loss.
    round_trip(&mut h, &[(b, "1")], "100000", "99900");
    h.price(BTC, "100000");
    assert_eq!(fills(&h.market_order(a, BTC, Side::Sell, "1", "100000")).len(), 1);
    assert!(!h.engine.period_results().contains_key(&a));
    assert_eq!(allocations(&next_period(&mut h)).into_iter().map(|(account, ..)| account).collect::<Vec<_>>(), [b]);
}

#[test]
fn a_liquidation_counts_less_the_debt_the_house_writes_off() {
    let mut h = Harness::emitting(BUDGET);
    let a = acct(1);
    h.deposit(a, "10001");
    h.price(BTC, "100000");
    assert_eq!(fills(&h.market_order(a, BTC, Side::Buy, "1", "100000")).len(), 1);
    let events = h.price(BTC, "85000");
    let liquidation = fills(&events).into_iter().find(|fill| fill.liquidation).expect("a close-out fill").clone();
    let written_off = events
        .iter()
        .find_map(|e| match e {
            Event::BadDebt { account, amount } if *account == a => Some(*amount),
            _ => None,
        })
        .expect("the loss passes the balance");
    let result = h.engine.period_results()[&a];
    assert_eq!((result.realised, result.written_off), (liquidation.realised_pnl, written_off));
    // The weight is the loss the account funded: its deposit, whatever the close-out's spread.
    assert_eq!(allocations(&next_period(&mut h)), [(a, 0, f("10001"), BUDGET)]);
}

#[test]
fn negative_cash_at_the_cutoff_earns_nothing_for_that_period_and_nothing_later() {
    let mut h = Harness::emitting(BUDGET);
    let (a, b) = (acct(1), acct(2));
    h.deposit(a, "12000");
    h.deposit(b, "1000000");
    h.price(ETH, "4000");
    assert_eq!(fills(&h.market_order(a, ETH, Side::Buy, "10", "4000")).len(), 1);
    h.price(ETH, "6000");
    h.price(BTC, "100000");
    assert_eq!(fills(&h.market_order(a, BTC, Side::Buy, "1", "100000")).len(), 1);
    // Selling BTC 15% down realises more than a's cash while its ETH gain keeps it above close-out.
    h.price(BTC, "85000");
    assert_eq!(fills(&h.market_order(a, BTC, Side::Sell, "1", "85000")).len(), 1);
    round_trip(&mut h, &[(b, "1")], "85000", "84900");
    assert!(h.balance(a).is_negative() && h.engine.period_results()[&a].realised < f("-15000"));
    let paid: Vec<_> =
        allocations(&next_period(&mut h)).into_iter().map(|(account, .., amount)| (account, amount)).collect();
    assert_eq!(paid, [(b, BUDGET)], "a's larger loss is left out");
    assert!(!h.engine.period_results().contains_key(&a), "the excluded loss is not carried forward");

    // Its cash recovers in the next period, but that period's gain earns nothing and period 0 stays
    // unpaid.
    h.price(ETH, "6000");
    assert_eq!(fills(&h.market_order(a, ETH, Side::Sell, "10", "6000")).len(), 1);
    assert!(h.balance(a).is_positive());
    assert!(allocations(&next_period(&mut h)).is_empty());
    assert_eq!(h.engine.entitlement(a), TokenUnits(0));
    // A later loss weighs only itself.
    round_trip(&mut h, &[(a, "1")], "84900", "84800");
    assert_eq!(allocations(&next_period(&mut h)), [(a, 2, f("100"), BUDGET)]);
}

#[test]
fn the_schedule_is_initialised_once_by_the_operator_and_never_backdated() {
    let mut h = Harness::v17();
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V18 });
    let now = h.ts;
    let init = |start_ms, budget| {
        AdminCommand::InitEmissions(EmissionSchedule { start_ms, period_budget: TokenUnits(budget) })
    };
    let refused = |h: &mut Harness, command: AdminCommand, why: &str| {
        let events = h.operate(command);
        assert!(
            events.iter().any(|e| matches!(e, Event::AdminRejected { reason } if reason.contains(why))),
            "{why}: {events:?}"
        );
    };
    refused(&mut h, init(now, BUDGET), "V19");
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V19 });
    // Once anything is credited only the operator's signature initialises it.
    let events = h.cmd(Command::Admin(init(now, BUDGET)));
    assert!(events.iter().any(|e| matches!(e, Event::AdminRejected { reason } if reason.contains("operator signs"))));
    refused(&mut h, init(now - 1, BUDGET), "before the sequenced time");
    refused(&mut h, init(MAX_STAMP_MS + 1, BUDGET), "past the latest stamp");
    refused(&mut h, init(now, 0), "outside 1 to");
    refused(&mut h, init(now, MAX_PERIOD_BUDGET + 1), "outside 1 to");
    assert_eq!(h.engine.emissions(), None);

    let start = now + PERIOD_MS;
    h.operator_admin(init(start, MAX_PERIOD_BUDGET));
    refused(&mut h, init(start, BUDGET), "already initialised");
    assert_eq!(h.engine.emissions().unwrap().schedule.period_budget, TokenUnits(MAX_PERIOD_BUDGET));

    // Losses before the start earn nothing; from the start they count.
    let [a] = funded(&mut h);
    round_trip(&mut h, &[(a, "1")], "100000", "99900");
    assert!(h.engine.period_results().is_empty());
    h.advance_to(start);
    round_trip(&mut h, &[(a, "1")], "100000", "99900");
    assert_eq!(h.engine.period_results()[&a].realised, f("-100"));
    assert_eq!(schedule(&h).start_ms, start);
}

#[test]
fn the_schedule_is_initialised_only_signed_even_before_any_credit() {
    // No house capital, so nothing is credited and other operator commands may still go unsigned.
    let mut h = Harness::zero_house(ProtocolVersion::V7);
    h.admin(AdminCommand::SetOperator(common::operator()));
    for version in [ProtocolVersion::V18, ProtocolVersion::V19] {
        h.admin(AdminCommand::ActivateProtocol { version });
    }
    let schedule = EmissionSchedule { start_ms: h.ts, period_budget: TokenUnits(BUDGET) };
    let events = h.cmd(Command::Admin(AdminCommand::InitEmissions(schedule)));
    assert!(
        events.iter().any(|e| matches!(e, Event::AdminRejected { reason } if reason.contains("operator signs"))),
        "{events:?}"
    );
    assert_eq!(h.engine.emissions(), None);
    h.operator_admin(AdminCommand::InitEmissions(schedule));
    assert_eq!(h.engine.emissions().unwrap().schedule, schedule);
}
