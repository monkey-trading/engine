//! The pricing side of the close-spread ceilings (`V39`): the clamp that bounds every close, the
//! exact-bound floor, the reversal refusal with the pure close still open, and the triggered
//! order that fills its closing slice and cancels the rest. The registry and its governance path
//! are in `close_spread_cap.rs`.

use engine_core::Fixed;
use engine_core::access::AccessPolicy;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{CancelReason, Event, Reject};
use engine_core::market::QuoteCurrency;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, Side};

mod common;
use common::close_spread::*;
use common::events::only_fill;
use common::{AAPL, BTC, Client, ETH, Harness, acct, f, market};

#[test]
fn the_registry_binds_from_v39_and_a_close_clamps_to_its_ceiling() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    install_expect(&mut h, ceilings());
    widen_slippage(&mut h);
    let client = Client::new(1);
    h.deposit(client.account, "1000000");
    h.price(BTC, "100000");
    // Pre-V39, a close pays the stress minimum, whatever the ceiling waiting for it.
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    let events = h.market_order(client.account, BTC, Side::Buy, "1", "100000");
    only_fill(&events);
    let events = h.market_order(client.account, BTC, Side::Sell, "1", "100000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 5, "pre-V39 the close pays the stress minimum: {events:?}");
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 0 });

    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    into_v39(&mut h);
    attest(&mut h, &client);
    // Raise the stress minimum above BTC's 20bps ceiling: the open pays it, and the close clamps.
    let mut raised = share_params();
    raised.stress_levels[0].min_spread_bps = 50;
    h.operator_admin(AdminCommand::SetParams(raised));
    let events = trade(&mut h, &client, BTC, Side::Buy, "1", "100000");
    let open = only_fill(&events);
    assert_eq!(open.spread_bps, 50, "an open pays what it prices: {events:?}");
    let events = trade(&mut h, &client, BTC, Side::Sell, "1", "100000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 20, "V39 clamps the close to the ceiling: {events:?}");
    assert_eq!(fill.price, f("99800"), "{fill:?}");
    h.assert_conserved(f("0"));
}

#[test]
fn activating_v39_refuses_a_listed_market_with_no_ceiling() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 }));
    assert!(rejected(&events).contains("without a close-spread ceiling"), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V8);
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V39 });
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V39);
}

#[test]
fn the_clamp_floors_the_adjustment_to_the_exact_bound() {
    let (mut h, client) = funded("1000000");
    let mut raised = share_params();
    raised.stress_levels[0].min_spread_bps = 50;
    h.operator_admin(AdminCommand::SetParams(raised));
    // A mid whose 20bps bound is fractional in raw units: the charged adjustment is the floor of
    // `index_raw * ceiling / 10_000`, never the unit-above value upward rounding gives the charge.
    let mid = f("0.654321987");
    h.price(BTC, "0.654321987");
    let events = trade(&mut h, &client, BTC, Side::Buy, "1", "0.654321987");
    only_fill(&events);
    let events = trade(&mut h, &client, BTC, Side::Sell, "1", "0.654321987");
    let fill = only_fill(&events);
    let bound = Fixed::from_raw(mid.raw() * 20 / 10_000);
    assert_eq!(fill.index, mid);
    assert_eq!(fill.price, mid - bound, "{fill:?}");
    // The exact bound holds: the adjustment never exceeds `index * ceiling / 10_000`.
    assert!((fill.index - fill.price).raw() * 10_000 <= mid.raw() * 20, "{fill:?}");
}

#[test]
fn a_reversal_over_the_ceiling_is_refused_and_the_pure_close_stays_open() {
    let (mut h, client) = funded("10000000");
    h.price(BTC, "100000");
    // A 100-unit long (10m notional) pays the top size tier's 5bps. Selling 150 reverses through
    // it: the blended charge is the opening slice's size cost per unit — 800 USD over 150 units,
    // 0.53bps of a 100000 mid, its truncated `spread_bps` 0 — well under the 20bps ceiling, so the
    // reversal passes and pays what it priced.
    let events = trade(&mut h, &client, BTC, Side::Buy, "100", "100000");
    only_fill(&events);
    let events = trade(&mut h, &client, BTC, Side::Sell, "150", "100000");
    let fill = only_fill(&events);
    assert_eq!(fill.position, f("-50"));
    assert_eq!(fill.price, f("99994.666666666"), "{fill:?}");
    let events = trade(&mut h, &client, BTC, Side::Buy, "200", "100000");
    only_fill(&events);
    assert_eq!(h.engine.accounts()[&client.account].position(BTC).qty, f("150"));
    // A 50bps stress minimum puts every close charge above the ceiling: the reversal is refused
    // and the position is untouched, and the reduce-only close still fills at the ceiling.
    let mut raised = share_params();
    raised.stress_levels[0].min_spread_bps = 50;
    h.operator_admin(AdminCommand::SetParams(raised));
    let events = trade(&mut h, &client, BTC, Side::Sell, "400", "100000");
    assert!(matches!(common::events::rejection(&events), Reject::CloseSpreadCeiling { ceiling_bps: 20 }), "{events:?}");
    assert_eq!(h.engine.accounts()[&client.account].position(BTC).qty, f("150"));
    let events = reduce(&mut h, &client, BTC, Side::Sell, "150", "100000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 20, "the pure close clamps: {events:?}");
    assert!(fill.position.is_zero());
    h.assert_conserved(f("0"));
}

#[test]
fn a_triggered_order_refused_as_a_reversal_fills_its_closing_slice() {
    let (mut h, client) = funded("10000000");
    h.price(BTC, "100000");
    let events = trade(&mut h, &client, BTC, Side::Buy, "100", "100000");
    only_fill(&events);
    let mut raised = share_params();
    raised.stress_levels[0].min_spread_bps = 50;
    h.operator_admin(AdminCommand::SetParams(raised));
    let events = rest(&mut h, &client, BTC, Side::Sell, "150", OrderKind::Stop { price: f("99999") });
    assert!(
        events.contains(&Event::OrderAccepted { account: client.account, client_id: 7, order_id: 1 }),
        "{events:?}"
    );
    // The drop through the stop's price triggers it: the reversal is over BTC's 20bps ceiling, so
    // the engine fills the 100-unit closing slice at the clamped close and cancels the rest.
    let events = h.price(BTC, "99999");
    let fill = only_fill(&events);
    assert_eq!(fill.qty, f("100"), "{events:?}");
    assert_eq!(fill.position, Fixed::ZERO, "{fill:?}");
    assert_eq!(fill.price, f("99799.002"), "{fill:?}");
    assert!(
        events.contains(&Event::OrderCancelled {
            account: client.account,
            order_id: 1,
            reason: CancelReason::Unfillable
        }),
        "{events:?}"
    );
    assert_eq!(h.engine.accounts()[&client.account].position(BTC).qty, Fixed::ZERO);
    h.assert_conserved(f("0"));
}

#[test]
fn adoption_adds_a_not_yet_listed_market_and_lowering_only_ever_moves_entries() {
    let mut h = harness();
    into_v39(&mut h);
    let due = h.ts + NOTICE_MS;
    h.operator_admin(AdminCommand::AdoptCloseSpreadCeiling { market: 7, max_close_spread_bps: 25, effective_at: due });
    // Listing waits for the adoption: no fallback ceiling exists.
    let gold = market(7, "XAU-USD", AssetClass::Commodity, Schedule::AlwaysOpen, 500);
    let events = h.operate(AdminCommand::UpsertMarket(gold.clone()));
    assert!(rejected(&events).contains("no adopted close-spread ceiling"), "{events:?}");
    h.advance_to(due);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&7], 25);
    h.operator_admin(AdminCommand::UpsertMarket(gold));
    // Raising — or re-adopting the same value — is refused; lowering is not; the entry stays put
    // until the notice runs.
    let later = h.ts + NOTICE_MS;
    for bps in [26, 25] {
        let events = h.operate(AdminCommand::AdoptCloseSpreadCeiling {
            market: 7,
            max_close_spread_bps: bps,
            effective_at: later,
        });
        assert!(rejected(&events).contains("can only be lowered"), "{events:?}");
    }
    h.operator_admin(AdminCommand::AdoptCloseSpreadCeiling {
        market: 7,
        max_close_spread_bps: 18,
        effective_at: later,
    });
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&7], 25);
    h.advance_to(later);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&7], 18);
    // An adoption effective inside the notice is refused.
    let events = h.operate(AdminCommand::AdoptCloseSpreadCeiling {
        market: 7,
        max_close_spread_bps: 10,
        effective_at: h.ts + 1,
    });
    assert!(rejected(&events).contains("no sooner"), "{events:?}");
}

#[test]
fn no_operator_command_raises_a_close_above_its_ceiling() {
    let (mut h, client) = funded("10000000");
    // The widest parameters the engine admits: a 100bps baseline stress minimum. AAPL's closes
    // still pay at most their 15bps ceiling; the open pays what it prices.
    let mut raised = share_params();
    raised.stress_levels[0].min_spread_bps = 100;
    h.operator_admin(AdminCommand::SetParams(raised));
    h.price(AAPL, "230");
    let events = trade(&mut h, &client, AAPL, Side::Buy, "100", "230");
    let open = only_fill(&events);
    assert_eq!(open.spread_bps, 100, "an open pays what it prices: {events:?}");
    let events = trade(&mut h, &client, AAPL, Side::Sell, "100", "230");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 15, "{events:?}");
    assert_eq!(fill.price, f("229.655"), "{fill:?}");
    h.assert_conserved(f("0"));
}

#[test]
fn a_ceilinged_market_cannot_change_instrument_identity() {
    let mut h = harness();
    into_v39(&mut h);
    let mut other_class = market(ETH, "ETH-EUR", AssetClass::Fx, Schedule::AlwaysOpen, 1000);
    other_class.quote_currency = QuoteCurrency::Usd;
    let events = h.operate(AdminCommand::UpsertMarket(other_class));
    assert!(rejected(&events).contains("asset class"), "{events:?}");
    let mut other_currency = market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1000);
    other_currency.quote_currency = QuoteCurrency::Eur { fx_market: 4 };
    let events = h.operate(AdminCommand::UpsertMarket(other_currency));
    assert!(rejected(&events).contains("quote currency"), "{events:?}");
    // A relist that keeps the instrument's identity applies.
    let mut relisted = market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1500);
    relisted.symbol = "ETH2-USD".into();
    h.operator_admin(AdminCommand::UpsertMarket(relisted));
}

#[test]
fn a_zero_ceiling_means_free_closes() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    install_expect(&mut h, with(&ceilings(), BTC, 0));
    let client = Client::new(1);
    h.deposit(client.account, "10000000");
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    into_v39(&mut h);
    attest(&mut h, &client);
    h.price(BTC, "100000");
    let events = trade(&mut h, &client, BTC, Side::Buy, "1", "100000");
    only_fill(&events);
    // A reversal whose opening slice pays no size spread fills free.
    let events = trade(&mut h, &client, BTC, Side::Sell, "2", "100000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 0, "{events:?}");
    assert_eq!(fill.price, f("100000"));
    // Back to flat, also free, then a long position big enough to pay a size spread on its next
    // reversal's opening slice.
    let events = trade(&mut h, &client, BTC, Side::Buy, "1", "100000");
    only_fill(&events);
    let events = trade(&mut h, &client, BTC, Side::Buy, "100", "100000");
    only_fill(&events);
    // A reversal whose opening slice pays anything is refused.
    let events = trade(&mut h, &client, BTC, Side::Sell, "200", "100000");
    assert!(matches!(common::events::rejection(&events), Reject::CloseSpreadCeiling { ceiling_bps: 0 }), "{events:?}");
}

#[test]
fn pre_v39_closes_pay_what_they_priced_and_the_registry_is_inert() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    install_expect(&mut h, ceilings());
    let a = acct(1);
    h.deposit(a, "1000000");
    h.price(ETH, "4000");
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(ETH), level: 1 });
    let events = h.market_order(a, ETH, Side::Buy, "1", "4000");
    only_fill(&events);
    let events = h.market_order(a, ETH, Side::Sell, "1", "4000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 5, "pre-V39 a close pays the stress minimum: {events:?}");
    // The registry is installed but inert: no ceiling applies before `V39`.
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V8);
}

#[test]
fn the_registry_and_pending_adoption_round_trip_through_serialization() {
    let mut h = harness();
    into_v39(&mut h);
    let due = h.ts + NOTICE_MS;
    h.operator_admin(AdminCommand::AdoptCloseSpreadCeiling {
        market: ETH,
        max_close_spread_bps: 10,
        effective_at: due,
    });
    let encoded = serde_json::to_vec(&h.engine).expect("engine serialises");
    let mut decoded: engine_core::Engine = serde_json::from_slice(&encoded).expect("engine deserialises");
    assert_eq!(decoded.close_spread_ceilings(), h.engine.close_spread_ceilings());
    // The pending adoption applies at its time on the deserialized engine too.
    let seq = h.log.len() as u64;
    let events = decoded.apply(&engine_core::command::Sequenced { seq, ts: due, cmd: Command::Tick });
    assert!(events.is_ok());
    assert_eq!(decoded.close_spread_ceilings().unwrap()[&ETH], 10);
}
