//! `V57`'s add clamp, through `Engine::apply`: an add's averaged open time counts the units held as
//! no older than the launch curve's last hold, whatever curve is in force, so a curve extended past it
//! after its notice lends an add no more age. `V55` and `V56` clamp at the curve in force's last hold,
//! as does a log launched with no curve, and `V57`'s activation keeps every open time. A voted curve is
//! in `parameter_votes/holding_fee.rs`.

mod common;

use common::close_spread::{attest, ceilings, fund, source_key, trade};
use common::events::only_fill;
use common::session_keys::HOUR_MS;
use common::spreads::{applied, funded, genesis_at};
use common::{BTC, Client, Harness};
use engine_core::Engine;
use engine_core::access::AccessPolicy;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Fill;
use engine_core::fixed::Rounding;
use engine_core::holding_fee::{CurvePoint, DEFAULT_PROFIT_FEE_CURVE};
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};

const V54: ProtocolVersion = ProtocolVersion::V54;
const V55: ProtocolVersion = ProtocolVersion::V55;
const V56: ProtocolVersion = ProtocolVersion::V56;
const V57: ProtocolVersion = ProtocolVersion::V57;

const DAY_MS: Millis = 24 * 60 * 60 * 1_000;

#[test]
fn an_extended_curve_lends_an_add_no_more_than_the_launch_curves_last_hold() {
    // A $1M BTC long held a day, then $9M added and closed at once, after the curve is extended to 30
    // minutes with a flat 10% tail. From V57 the units held lend the add a tenth of the launch curve's
    // 15 minutes, 90 s, and the close pays the 56.28% there; V55 and V56 clamp at the extended curve's
    // 30 minutes, lending 180 s, where it pays 42.68%.
    for (version, lent, rate) in [(V57, 90_000, 5_628), (V56, 180_000, 4_268), (V55, 180_000, 4_268)] {
        let (mut h, client) = funded(version, "10000000");
        let mut extended = DEFAULT_PROFIT_FEE_CURVE.to_vec();
        extended.push(CurvePoint { hold_ms: 30 * 60_000, fee_bps: 1_000 });
        notice_curve(&mut h, extended);
        assert_eq!(h.engine.operator_bounds().launch_curve.as_deref(), Some(&DEFAULT_PROFIT_FEE_CURVE[..]));
        only_fill(&trade(&mut h, &client, BTC, Side::Buy, "10", "100000"));
        assert_eq!(add_ninefold_a_day_on(&mut h, &client), (lent, rate), "{version:?}");
    }
}

#[test]
fn activation_keeps_each_open_time_and_the_next_add_clamps_at_the_launch_curve() {
    // Under V56 with the curve extended, a long held 20 minutes keeps its open time across V57's
    // activation, and the add after it counts the units held as 15 minutes old, not 20.
    let (mut h, client) = funded(V56, "10000000");
    let mut extended = DEFAULT_PROFIT_FEE_CURVE.to_vec();
    extended.push(CurvePoint { hold_ms: 30 * 60_000, fee_bps: 1_000 });
    notice_curve(&mut h, extended);
    let opened = h.ts;
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "10", "100000"));
    h.advance(20 * 60_000);
    h.price(BTC, "100000");
    // A root before the activation, so the one after it updates that tree rather than building one.
    h.engine.state_root();
    h.admin(AdminCommand::ActivateProtocol { version: V57 });
    assert_eq!(opened_at(&h, &client), Some(opened));
    let root = h.engine.state_root();
    let mut rebuilt: Engine = serde_json::from_value(serde_json::to_value(&h.engine).unwrap()).unwrap();
    assert_eq!(rebuilt.state_root(), root);
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "90", "100000"));
    assert_eq!(opened_at(&h, &client), Some(h.ts - 90_000));
}

#[test]
fn the_clamp_is_the_last_hold_of_the_curve_the_log_launched_with() {
    // Parameters naming a curve to 30 minutes before V55 launch with it: a jump from V54 straight to
    // V57 starts the held long at that curve's last hold and records the curve as the launch curve, so
    // the units held lend an add a tenth of 30 minutes, 180 s, where that curve charges 93.80%.
    let (mut h, client) = funded(V54, "10000000");
    let launch =
        vec![CurvePoint { hold_ms: 60_000, fee_bps: 10_000 }, CurvePoint { hold_ms: 30 * 60_000, fee_bps: 1_000 }];
    assert!(applied(&h.operate(AdminCommand::SetParams(with_curve(&h, launch.clone())))));
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "10", "100000"));
    h.admin(AdminCommand::ActivateProtocol { version: V57 });
    assert_eq!(h.engine.operator_bounds().launch_curve.as_ref(), Some(&launch));
    assert_eq!(opened_at(&h, &client), Some(h.ts - 30 * 60_000));
    assert_eq!(add_ninefold_a_day_on(&mut h, &client), (180_000, 9_380));
}

#[test]
fn a_log_launched_with_no_curve_clamps_an_add_at_the_curve_in_force() {
    // A fresh log whose genesis names no curve launches with none, so it has no hold to pin and its
    // bounds admit any curve later. Once the default curve is noticed in, an add still averages: the
    // units held lend it the curve in force's 15 minutes over 10, 90 s, rather than restarting the clock.
    let (mut h, client) = launched_without_a_curve("10000000");
    assert_eq!(h.engine.operator_bounds().launch_curve.as_deref(), Some(&[][..]));
    notice_curve(&mut h, DEFAULT_PROFIT_FEE_CURVE.to_vec());
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "10", "100000"));
    assert_eq!(add_ninefold_a_day_on(&mut h, &client), (90_000, 5_628));
}

// ── Helpers ──

/// Moves the profit fee curve to `curve` after its notice, and prices BTC at 100000.
fn notice_curve(h: &mut Harness, curve: Vec<CurvePoint>) {
    let effective_at = h.ts + NOTICE_MS;
    let notice = ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        score_tiers: None,
        profit_fee_curve: Some(curve.clone()),
        volume_tiers: None,
        effective_at,
    };
    assert!(applied(&h.operate(AdminCommand::AnnounceParams(notice))));
    h.advance_to(effective_at);
    assert!(applied(&h.operate(AdminCommand::SetParams(with_curve(h, curve.clone())))));
    assert_eq!(h.engine.params().unwrap().profit_fee_curve, curve);
    h.price(BTC, "100000");
}

/// With `client` holding 10 BTC, adds 90 a day later and closes the 90 at once at 101000. Returns how
/// far back of the add the open time moved, and the rate the close's gain paid, which matches its hold.
fn add_ninefold_a_day_on(h: &mut Harness, client: &Client) -> (Millis, i64) {
    h.advance(DAY_MS);
    h.price(BTC, "100000");
    only_fill(&trade(h, client, BTC, Side::Buy, "90", "100000"));
    let lent = h.ts - opened_at(h, client).expect("a held position has an open time");
    h.price(BTC, "101000");
    let close: Fill = only_fill(&trade(h, client, BTC, Side::Sell, "90", "101000")).clone();
    let rate = close.profit_fee_bps.expect("a close names its rate");
    assert_eq!(close.hold_ms, Some(lent), "{close:?}");
    assert!(close.realised_pnl.is_positive(), "{close:?}");
    assert_eq!(close.profit_fee, close.realised_pnl.mul_bps_rounded(rate, Rounding::Up), "{close:?}");
    (lent, rate)
}

/// A fresh log that activates `V57` before its genesis, as a node's does, with genesis parameters that
/// name no curve, as `config/anvil.toml`'s (`genesis_at`), then the access policy, the ceiling registry
/// and house capital, client 1 declared and holding `amount`, and BTC priced at 100000.
fn launched_without_a_curve(amount: &str) -> (Harness, Client) {
    let mut h = genesis_at(V57);
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 10 * HOUR_MS }));
    h.admin(AdminCommand::SetCloseSpreadCeilings { ceilings: ceilings() });
    h.cmd(Command::HouseCapital { amount: h.house_capital, chain_ref: "genesis".into() });
    let client = Client::new(1);
    attest(&mut h, &client);
    fund(&mut h, &client, amount);
    h.price(BTC, "100000");
    (h, client)
}

/// The parameters in force with `curve` as the profit fee curve.
fn with_curve(h: &Harness, curve: Vec<CurvePoint>) -> ProtocolParams {
    ProtocolParams { profit_fee_curve: curve, ..h.engine.params().expect("the harness sets parameters").clone() }
}

/// `client`'s BTC open time.
fn opened_at(h: &Harness, client: &Client) -> Option<Millis> {
    h.engine.accounts()[&client.account].opened_at.get(&BTC).copied()
}
