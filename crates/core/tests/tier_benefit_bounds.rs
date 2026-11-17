//! The operator's bounds on `V68`'s tier benefits, through `Engine::apply`: a table keeps each tier's fee
//! discount, financing discount and emissions boost at or below launch's and never falling with the
//! tier, so a lowered higher tier cannot pay more than a lower one, and moves only as a seven-day
//! notice names it. A voted table is held to the same (`examples/reference_log/v68.rs`). Before `V68` a
//! table may name no benefit.

mod common;

use common::Harness;
use common::session_keys::funded_at;
use common::spreads::{applied, refused};
use engine_core::command::AdminCommand;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;
use engine_core::volume_tier::{
    ACTIVATION_EMISSIONS_BOOSTS_BPS, ACTIVATION_FEE_DISCOUNTS_BPS, ACTIVATION_FINANCING_DISCOUNTS_BPS, VolumeTier,
};

const V66: ProtocolVersion = ProtocolVersion::V66;
const V68: ProtocolVersion = ProtocolVersion::V68;

#[test]
fn a_table_naming_a_benefit_above_launch_is_refused() {
    let (mut h, _, _) = funded_at(V68);
    let in_force = h.engine.params().unwrap().volume_tiers.clone();
    let due = h.ts + NOTICE_MS;
    let cases: [(Change, &str); 4] = [
        (
            |tiers| tiers[3].fee_discount_bps = 2_001,
            "volume tier 3's fee_discount_bps 2001 is above its launch value 2000",
        ),
        (
            |tiers| tiers[2].financing_discount_bps = 1_001,
            "volume tier 2's financing_discount_bps 1001 is above its launch value 1000",
        ),
        (|tiers| tiers[1].emissions_boost_bps = 1_001, "volume tier 1's emissions_boost_bps 1001 is above its launch"),
        (|tiers| tiers[1].fee_discount_bps = 1, "volume tier 1's fee_discount_bps 1 is above its launch value 0"),
    ];
    for (change, reason) in cases {
        let mut tiers = in_force.clone();
        change(&mut tiers);
        let refusal = refused(&h.operate(notice(tiers, due))).to_owned();
        assert!(refusal.contains(reason), "{reason}: {refusal}");
    }
}

#[test]
fn a_table_whose_benefits_fall_with_the_tier_or_leave_their_range_is_refused() {
    let (mut h, _, _) = funded_at(V68);
    let in_force = h.engine.params().unwrap().volume_tiers.clone();
    let due = h.ts + NOTICE_MS;
    // Lowering tier 3's fee discount below tier 2's would make the higher tier pay more.
    let cases: [(Change, &str); 5] = [
        (|tiers| tiers[3].fee_discount_bps = 500, "fee_discount_bps must be in 0..=10000 and never fall with the tier"),
        (
            |tiers| tiers[3].financing_discount_bps = 500,
            "financing_discount_bps must be in 0..=10000 and never fall with the tier",
        ),
        (|tiers| tiers[3].emissions_boost_bps = 2_000, "emissions_boost_bps must be in 0..=100000 and never fall"),
        (|tiers| tiers[0].fee_discount_bps = -1, "fee_discount_bps must be in 0..=10000"),
        (|tiers| tiers[3].fee_discount_bps = 10_001, "fee_discount_bps must be in 0..=10000"),
    ];
    for (change, reason) in cases {
        let mut tiers = in_force.clone();
        change(&mut tiers);
        let refusal = refused(&h.operate(notice(tiers, due))).to_owned();
        assert!(refusal.contains(reason), "{reason}: {refusal}");
    }
}

#[test]
fn a_smaller_benefit_is_noticed_and_applies_from_its_time() {
    let (mut h, _, _) = funded_at(V68);
    let in_force = h.engine.params().unwrap().volume_tiers.clone();
    let column = |tiers: &[VolumeTier], read: fn(&VolumeTier) -> i64| tiers.iter().map(read).collect::<Vec<_>>();
    assert_eq!(column(&in_force, |tier| tier.fee_discount_bps), ACTIVATION_FEE_DISCOUNTS_BPS);
    assert_eq!(column(&in_force, |tier| tier.financing_discount_bps), ACTIVATION_FINANCING_DISCOUNTS_BPS);
    assert_eq!(column(&in_force, |tier| tier.emissions_boost_bps), ACTIVATION_EMISSIONS_BOOSTS_BPS);
    let mut lower = in_force;
    lower.iter_mut().zip([0, 0, 500, 1_000]).for_each(|(tier, bps)| tier.fee_discount_bps = bps);
    lower.iter_mut().zip([0, 0, 0, 1_000]).for_each(|(tier, bps)| tier.financing_discount_bps = bps);
    lower.iter_mut().zip([0, 500, 1_000, 2_000]).for_each(|(tier, bps)| tier.emissions_boost_bps = bps);
    let set = AdminCommand::SetParams(params(&h, lower.clone()));
    assert!(refused(&h.operate(set.clone())).contains("needs a notice of at least seven days"));
    let due = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(notice(lower.clone(), due))));
    assert!(refused(&h.operate(set.clone())).contains("takes effect from"));
    h.advance_to(due);
    assert!(applied(&h.operate(set)));
    assert_eq!(h.engine.params().unwrap().volume_tiers, lower);
    // The launch table keeps the benefits it started with, so the table cannot rise back above them.
    let launch = h.engine.operator_bounds().launch_volume_tiers.clone().unwrap();
    assert_eq!(column(&launch, |tier| tier.fee_discount_bps), ACTIVATION_FEE_DISCOUNTS_BPS);
    let mut raised = lower;
    raised[3].fee_discount_bps = 2_001;
    let refusal = refused(&h.operate(notice(raised, h.ts + NOTICE_MS))).to_owned();
    assert!(refusal.contains("fee_discount_bps 2001 is above its launch value 2000"), "{refusal}");
}

#[test]
fn before_v68_a_table_naming_a_benefit_is_refused_whether_noticed_or_set() {
    let (mut h, _, _) = funded_at(V66);
    let reason = "a tier discount or boost needs protocol V68";
    let columns: [Change; 3] = [
        |tiers| tiers[3].fee_discount_bps = 2_000,
        |tiers| tiers[3].financing_discount_bps = 2_000,
        |tiers| tiers[3].emissions_boost_bps = 5_000,
    ];
    for change in columns {
        let mut named = h.engine.params().unwrap().volume_tiers.clone();
        change(&mut named);
        assert!(refused(&h.operate(notice(named.clone(), h.ts + NOTICE_MS))).contains(reason));
        assert!(refused(&h.operate(AdminCommand::SetParams(params(&h, named)))).contains(reason));
    }
}

// ── Helpers ──

/// An edit of a tier table.
type Change = fn(&mut Vec<VolumeTier>);

/// The parameters in force with `volume_tiers`.
fn params(h: &Harness, volume_tiers: Vec<VolumeTier>) -> ProtocolParams {
    ProtocolParams { volume_tiers, ..h.engine.params().unwrap().clone() }
}

/// A notice of `volume_tiers` from `effective_at`.
fn notice(volume_tiers: Vec<VolumeTier>, effective_at: Millis) -> AdminCommand {
    AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        score_tiers: None,
        profit_fee_curve: None,
        volume_tiers: Some(volume_tiers),
        rollover: None,
        corporate_bounds: None,
        effective_at,
    })
}
