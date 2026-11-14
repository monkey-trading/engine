//! The operator's bounds on the holding-time profit fee from `V55`, through `Engine::apply`: the base
//! moves only from its launch value up to +50%, and the hold curve only to one at or above its launch
//! curve at every hold, reaching at most 30 minutes, each after a seven-day notice; under those
//! bounds no hold pays less than at launch, and a longer hold never pays more.

mod common;

use common::close_spread::trade;
use common::events::only_fill;
use common::spreads::{applied, funded, refused};
use common::toxicity::{fresh_at, v54_params};
use common::{BTC, Harness};
use engine_core::command::AdminCommand;
use engine_core::holding_fee::{CurvePoint, DEFAULT_PROFIT_FEE_CURVE, fee_at};
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{Millis, Side};

const V54: ProtocolVersion = ProtocolVersion::V54;
const V55: ProtocolVersion = ProtocolVersion::V55;

#[test]
fn a_profit_fee_cut_below_launch_is_refused_and_a_raise_applies_from_its_notice() {
    let (mut h, _) = funded(V55, "1000000");
    let due = h.ts + NOTICE_MS;
    // A cut is refused as a notice and as parameters, inside V35's ±50% though it is.
    for fee in [500, 999] {
        assert_eq!(
            refused(&h.operate(notice(Some(fee), None, due))),
            format!("profit_fee_bps {fee} is below its launch value 1000")
        );
        let cut = ProtocolParams { profit_fee_bps: fee, ..params(&h) };
        assert!(refused(&h.operate(AdminCommand::SetParams(cut))).contains("below its launch value 1000"));
    }
    // Above +50% is refused as before; a raise to 15% applies once its notice has run.
    assert!(refused(&h.operate(notice(Some(1_501), None, due))).contains("outside ±50%"));
    assert!(applied(&h.operate(notice(Some(1_500), None, due))));
    let raised = ProtocolParams { profit_fee_bps: 1_500, ..params(&h) };
    h.advance_to(due - 1);
    assert!(refused(&h.operate(AdminCommand::SetParams(raised.clone()))).contains(&format!("takes effect from {due}")));
    h.advance_to(due);
    assert!(applied(&h.operate(AdminCommand::SetParams(raised))));
    // Back down to launch is a change like any other: it waits for a notice, and launch is its floor.
    let launch = ProtocolParams { profit_fee_bps: 1_000, ..params(&h) };
    assert!(refused(&h.operate(AdminCommand::SetParams(launch))).contains("needs a notice"));
}

#[test]
fn a_raised_base_charges_the_curve_where_it_is_higher_and_the_base_past_it() {
    let (mut h, client) = funded(V55, "1000000");
    let due = h.ts + NOTICE_MS;
    h.operator_admin(notice(Some(1_500), None, due));
    h.advance_to(due);
    h.operator_admin(AdminCommand::SetParams(ProtocolParams { profit_fee_bps: 1_500, ..params(&h) }));
    // (hold, rate): the curve's 18.28% at 10 minutes, and the raised base at 30.
    for (hold, rate) in [(600_000, 1_828), (1_800_000, 1_500)] {
        h.price(BTC, "100000");
        only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
        h.advance(hold);
        h.price(BTC, "101000");
        let close = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "101000")).clone();
        assert_eq!((close.hold_ms, close.profit_fee_bps), (Some(hold), Some(rate)));
    }
}

#[test]
fn a_curve_change_needs_a_notice_and_stays_at_or_above_launch_to_at_most_thirty_minutes() {
    let (mut h, _) = funded(V55, "1000000");
    let due = h.ts + NOTICE_MS;
    let mut lowered = DEFAULT_PROFIT_FEE_CURVE.to_vec();
    lowered[4].fee_bps -= 1;
    let shorter = DEFAULT_PROFIT_FEE_CURVE[..10].to_vec();
    let too_long = vec![CurvePoint { hold_ms: 30 * 60_000 + 1, fee_bps: 10_000 }];
    for (curve, reason) in [
        (lowered, "profit_fee_curve is below its launch curve at a hold of 60492 ms"),
        (shorter, "profit_fee_curve is below its launch curve at a hold of 573877 ms"),
        (vec![], "profit_fee_curve is below its launch curve at a hold of 0 ms"),
        (too_long, "profit_fee_curve reaches at most 1800000 ms"),
    ] {
        assert_eq!(refused(&h.operate(notice(None, Some(curve.clone()), due))), reason);
        let set = ProtocolParams { profit_fee_curve: curve, ..params(&h) };
        assert!(refused(&h.operate(AdminCommand::SetParams(set))).contains("profit_fee_curve"));
    }
    // The full fee held to a minute and the base reached at 30 minutes is above launch everywhere.
    let steeper =
        vec![CurvePoint { hold_ms: 60_000, fee_bps: 10_000 }, CurvePoint { hold_ms: 1_800_000, fee_bps: 1_000 }];
    let set = AdminCommand::SetParams(ProtocolParams { profit_fee_curve: steeper.clone(), ..params(&h) });
    assert_eq!(refused(&h.operate(set.clone())), "a profit_fee_curve change needs a notice of at least seven days");
    assert!(applied(&h.operate(notice(None, Some(steeper.clone()), due))));
    h.advance_to(due - 1);
    assert_eq!(refused(&h.operate(set.clone())), format!("the profit_fee_curve change takes effect from {due}"));
    h.advance_to(due);
    assert!(applied(&h.operate(set)));
    assert_eq!(params(&h).profit_fee_curve, steeper);
    assert_eq!(h.engine.operator_bounds().notice, None, "the applied curve spends its notice");
}

#[test]
fn a_fresh_log_records_its_first_parameters_curve_as_the_launch_curve() {
    // A fresh node's log activates the version before its genesis parameters, which here name a curve
    // other than the default.
    let curve =
        vec![CurvePoint { hold_ms: 60_000, fee_bps: 10_000 }, CurvePoint { hold_ms: 1_800_000, fee_bps: 1_000 }];
    for version in [V55, ProtocolVersion::LATEST] {
        let mut h = fresh_at(version, ProtocolParams { profit_fee_curve: curve.clone(), ..v54_params() });
        assert_eq!(h.engine.operator_bounds().launch_curve.as_ref(), Some(&curve), "{version:?}");
        // A bp below it at 30 minutes is refused, though it is above the default curve at every hold.
        let mut lowered = curve.clone();
        lowered[1].fee_bps -= 1;
        let events = h.operate(notice(None, Some(lowered), h.ts + NOTICE_MS));
        let reason = "profit_fee_curve is below its launch curve at a hold of 1800000 ms";
        assert_eq!(refused(&events), reason, "{version:?}");
    }
}

#[test]
fn before_v55_a_curve_charges_nothing_and_activation_keeps_it_as_the_launch_curve() {
    let (mut h, client) = funded(V54, "1000000");
    let due = h.ts + NOTICE_MS;
    // A notice names no curve before V55, and a cut within ±50% applies after its notice, as V35 has it.
    let named = notice(None, Some(DEFAULT_PROFIT_FEE_CURVE.to_vec()), due);
    assert_eq!(refused(&h.operate(named)), "a profit fee curve needs protocol V55");
    assert!(applied(&h.operate(notice(Some(800), None, due))));
    // Parameters may carry a curve, which charges nothing until V55.
    let steeper =
        vec![CurvePoint { hold_ms: 60_000, fee_bps: 10_000 }, CurvePoint { hold_ms: 1_800_000, fee_bps: 1_000 }];
    let set = ProtocolParams { profit_fee_curve: steeper.clone(), ..params(&h) };
    assert!(applied(&h.operate(AdminCommand::SetParams(set))));
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    h.price(BTC, "101000");
    let close = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "101000")).clone();
    assert_eq!((close.hold_ms, close.profit_fee_bps), (None, None));
    assert_eq!(close.profit_fee, close.realised_pnl.mul_bps_rounded(1_000, engine_core::fixed::Rounding::Up));
    // Activation keeps the curve the parameters name, as the launch curve.
    h.admin(AdminCommand::ActivateProtocol { version: V55 });
    assert_eq!(params(&h).profit_fee_curve, steeper);
    assert_eq!(h.engine.operator_bounds().launch_curve.as_ref(), Some(&steeper));
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    h.price(BTC, "101000");
    let close = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "101000")).clone();
    assert_eq!((close.hold_ms, close.profit_fee_bps), (Some(0), Some(10_000)));
}

#[test]
fn activation_raises_a_base_cut_before_v55_back_to_its_launch_value() {
    // A cut to 8% within V35's range applies after its notice; V55 then holds every hold at the 10%
    // launch base or above.
    let (mut h, client) = funded(V54, "1000000");
    let due = h.ts + NOTICE_MS;
    h.operator_admin(notice(Some(800), None, due));
    h.advance_to(due);
    h.operator_admin(AdminCommand::SetParams(ProtocolParams { profit_fee_bps: 800, ..params(&h) }));
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    h.admin(AdminCommand::ActivateProtocol { version: V55 });
    assert_eq!(params(&h).profit_fee_bps, 1_000);
    h.price(BTC, "101000");
    let close = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "1", "101000")).clone();
    assert_eq!((close.hold_ms, close.profit_fee_bps), (Some(900_000), Some(1_000)));
}

#[test]
fn every_admitted_base_pays_at_least_the_launch_fee_at_every_hold_and_never_more_later() {
    // The launch fee at each hold, against each base the bounds admit, 10% to 15%, on the launch
    // curve and on a curve raised everywhere.
    let raised: Vec<CurvePoint> =
        DEFAULT_PROFIT_FEE_CURVE.iter().map(|p| CurvePoint { fee_bps: (p.fee_bps + 250).min(10_000), ..*p }).collect();
    for curve in [DEFAULT_PROFIT_FEE_CURVE.to_vec(), raised] {
        for base in [1_000, 1_100, 1_250, 1_500] {
            let mut previous = i64::MAX;
            for hold in (0..=2_000_000).step_by(997).chain([10_000, 899_999, 900_000]) {
                let fee = fee_at(base, &curve, hold);
                assert!(fee >= fee_at(1_000, &DEFAULT_PROFIT_FEE_CURVE, hold), "{base} at {hold}");
                if hold % 997 == 0 {
                    assert!(fee <= previous, "{base} at {hold}");
                    previous = fee;
                }
            }
        }
    }
}

// ── Helpers ──

fn params(h: &Harness) -> ProtocolParams {
    h.engine.params().expect("the harness sets parameters").clone()
}

fn notice(
    profit_fee_bps: Option<i64>,
    profit_fee_curve: Option<Vec<CurvePoint>>,
    effective_at: Millis,
) -> AdminCommand {
    AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps,
        closeout_level_bps: None,
        stress: None,
        profit_fee_curve,
        effective_at,
        score_tiers: None,
    })
}
