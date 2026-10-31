//! Close-spread ceilings under `V40`: the registry's adoption, the governance path, and the
//! listing guards. Ceilings are unequal test values at or above the harness launch close charge
//! (8bps: a 5bps stress minimum plus the 3bps top toxicity add-on); the pricing-side behaviour —
//! the clamp, the reversal refusal, and the triggered order's closing slice — is in
//! `close_spread_pricing.rs`.

use engine_core::auth::ClientAuth;
use engine_core::close_spread::{CloseSpreadCeiling, MAX_CLOSE_SPREAD_BPS};
use engine_core::command::{AdminCommand, Command};
use engine_core::market::QuoteCurrency;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, Side};

mod common;
use common::close_spread::*;
use common::events::only_fill;
use common::{BTC, ETH, Harness, acct, market};

#[test]
fn install_adopts_every_listed_market_once_and_refuses_a_flawed_registry() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    let ok = ceilings();
    // A listed market with no entry, an entry for an unlisted market, a duplicate, an out-of-range
    // value, and a nonzero value below the launch close charge: each refuses, the registry stays
    // uninstalled.
    let cases: Vec<(Vec<CloseSpreadCeiling>, &str)> = vec![
        (ok[..ok.len() - 1].to_vec(), "listed without"),
        (duplicating(&ok, ETH, 25), "twice"),
        (with(&ok, BTC, MAX_CLOSE_SPREAD_BPS + 1), "0..=9999"),
        (with(&ok, BTC, 7), "largest close charge"),
    ];
    for (ceilings, why) in cases {
        let events = install(&mut h, ceilings);
        assert!(rejected(&events).contains(why), "{why}: {events:?}");
        assert!(h.engine.close_spread_ceilings().is_none());
    }
    // Zero is a legal ceiling: free closes.
    install_expect(&mut h, with(&ok, BTC, 0));
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&BTC], 0);
    // A log records the registry once.
    let events = install(&mut h, ok);
    assert!(rejected(&events).contains("already installed"));
}

#[test]
fn adoption_adds_a_not_yet_listed_market_and_lowering_only_ever_moves_entries() {
    let mut h = harness();
    into_v40(&mut h);
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
fn a_ceilinged_market_cannot_change_instrument_identity() {
    let mut h = harness();
    into_v40(&mut h);
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
fn pre_v40_closes_pay_what_they_priced_and_the_registry_is_inert() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    // A zero ceiling on ETH: the close below pays 5bps anyway, which fails if the cap applies.
    install_expect(&mut h, with(&ceilings(), ETH, 0));
    let a = acct(1);
    h.deposit(a, "1000000");
    h.price(ETH, "4000");
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(ETH), level: 1 });
    let events = h.market_order(a, ETH, Side::Buy, "1", "4000");
    only_fill(&events);
    let events = h.market_order(a, ETH, Side::Sell, "1", "4000");
    let fill = only_fill(&events);
    assert_eq!(fill.spread_bps, 5, "pre-V40 a close pays the stress minimum: {events:?}");
    // The registry is installed but inert: no ceiling applies before `V40`.
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V8);
}

#[test]
fn install_adopts_a_ceiling_for_a_market_no_genesis_lists() {
    let mut h = Harness::v8(ClientAuth::Unsigned);
    // An entry for a market no genesis lists stays inert until the market is listed: an upgrade's
    // ceilings can cover markets a legacy log listed after its genesis.
    install_expect(&mut h, with(&ceilings(), 7, 20));
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&7], 20);
    into_v40(&mut h);
    h.operator_admin(AdminCommand::UpsertMarket(market(
        7,
        "XAU-USD",
        AssetClass::Commodity,
        Schedule::AlwaysOpen,
        500,
    )));
}

#[test]
fn the_registry_and_pending_adoption_round_trip_through_serialization() {
    let mut h = harness();
    into_v40(&mut h);
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
