//! What bounds the toxicity score from `V54`, through `Engine::apply`: activation starts every
//! account neutral and floors every listing, a market's floor is named at its listing and fixed after, the tiers move only
//! after a notice and within ±50% of launch, and the top add-on bounds a closing trade and the
//! close-spread ceilings.

mod common;

use common::close_spread::{install, rejected, share_params};
use common::session_keys::funded_at;
use common::toxicity::{crypto, fill, floored, fresh_at, fresh_v54, mark, standing, v54_params};
use common::{BTC, ETH, Harness, f};
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::{HORIZON_MS, ScoreTier, default_markout_floor_bps, launch_score_tiers};
use engine_core::types::{AssetClass, Side};

fn applied(events: &[Event]) -> bool {
    events.last() == Some(&Event::AdminApplied)
}

fn tiers(pairs: &[(&str, i64)]) -> Vec<ScoreTier> {
    pairs.iter().map(|(score, bps)| ScoreTier { min_score: f(score), extra_spread_bps: *bps }).collect()
}

fn notice(score_tiers: Vec<ScoreTier>, effective_at: i64) -> AdminCommand {
    AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps: None,
        closeout_level_bps: None,
        stress: None,
        score_tiers: Some(score_tiers),
        effective_at,
        profit_fee_curve: None,
    })
}

fn with_tiers(h: &Harness, score_tiers: Vec<ScoreTier>) -> AdminCommand {
    let mut params = h.engine.params().unwrap().clone();
    params.toxicity.score_tiers = score_tiers;
    AdminCommand::SetParams(params)
}

#[test]
fn activation_drops_queued_markouts_clears_the_v11_scores_and_starts_every_account_neutral() {
    let (mut h, client, _) = funded_at(ProtocolVersion::V51);
    // Under `V51` a buy the index then rises 40 bps from scores the account, and a second waits.
    common::close_spread::trade(&mut h, &client, BTC, Side::Buy, "0.1", "100000");
    h.advance(1_000);
    h.price(BTC, "100400");
    common::close_spread::trade(&mut h, &client, BTC, Side::Buy, "0.1", "100400");
    let before = &h.engine.accounts()[&client.account];
    assert!(before.toxicity_bps > 0 && !before.toxicity_history.is_zero(), "{before:?}");
    assert!(!serde_json::to_value(&h.engine).unwrap()["markouts"].as_array().unwrap().is_empty());

    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V54 });
    let after = &h.engine.accounts()[&client.account];
    assert_eq!((after.toxicity_bps, after.toxicity_fraction, after.toxicity_history), (0, f("0"), f("0")));
    assert!(
        serde_json::to_value(&h.engine).unwrap().get("markouts").is_none_or(|queued| queued == &serde_json::json!([]))
    );
    let params = h.engine.params().unwrap();
    assert!(params.toxicity.tiers.is_empty());
    assert_eq!((params.toxicity.horizon_ms, &params.toxicity.score_tiers), (HORIZON_MS, &launch_score_tiers()));
    assert_eq!(h.engine.operator_bounds().launch_score_tiers, Some(launch_score_tiers()));
    assert_eq!(standing(&h, client.account), Default::default());
    // BTC was listed before `V54` with no floor: the activation gives it a crypto listing's, and its
    // fills score.
    assert_eq!(h.engine.markets()[&BTC].spec.markout_floor_bps, Some(default_markout_floor_bps(AssetClass::Crypto)));
    h.price(BTC, "100400");
    fill(&mut h, &client, Side::Buy, "0.1", "100400");
    mark(&mut h, "100800");
    assert_eq!(standing(&h, client.account).fills, f("1"));
}

#[test]
fn activation_refuses_parameters_whose_score_tiers_leave_no_close_able_to_fill() {
    // Under `V51` a stress minimum of 9996 bps and the earlier top tier's 3 bps leave a close a
    // basis point; the launch score tiers' 5 bps the activation would install leave none.
    for (minimum, refusal) in [
        (9_996, Some("stress and toxicity spreads of 9996 and 5 bps reach the price, so no closing trade could fill")),
        (9_994, None),
    ] {
        let mut params = share_params();
        params.stress_levels[1].min_spread_bps = minimum;
        let mut h = fresh_at(ProtocolVersion::V51, params);
        let events = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V54 }));
        match refusal {
            Some(reason) => assert_eq!(rejected(&events), reason),
            None => assert!(applied(&events), "{events:?}"),
        }
    }
}

#[test]
fn a_markout_floor_is_named_at_listing_and_fixed_after() {
    // Before `V54` a listing may name a floor, and change it, for the activation to take up in place
    // of its class's.
    let (mut h, _, _) = funded_at(ProtocolVersion::V51);
    h.operator_admin(AdminCommand::UpsertMarket(floored(crypto(BTC, "BTC-USD"), "3")));
    h.operator_admin(AdminCommand::UpsertMarket(floored(crypto(BTC, "BTC-USD"), "2")));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V54 });
    let events = h.operate(AdminCommand::UpsertMarket(floored(crypto(BTC, "BTC-USD"), "1")));
    assert_eq!(rejected(&events), "market 1's markout floor is fixed at 2 bps");

    let (mut h, _, _) = funded_at(ProtocolVersion::V54);
    // ETH, listed before `V54` with no floor, took a crypto listing's at the activation, and keeps it.
    h.operator_admin(AdminCommand::UpsertMarket(floored(crypto(ETH, "ETH-USD"), "1")));
    for (spec, reason) in [
        (floored(crypto(ETH, "ETH-USD"), "2"), "market 2's markout floor is fixed at 1 bps"),
        (crypto(ETH, "ETH-USD"), "market 2 names no markout floor"),
        (floored(crypto(ETH, "ETH-USD"), "0"), "markout_floor_bps must be in 0.01..=10000"),
        (floored(crypto(ETH, "ETH-USD"), "0.009999999"), "markout_floor_bps must be in 0.01..=10000"),
        (floored(crypto(ETH, "ETH-USD"), "10000.000000001"), "markout_floor_bps must be in 0.01..=10000"),
    ] {
        assert_eq!(rejected(&h.operate(AdminCommand::UpsertMarket(spec))), reason);
    }
    assert_eq!(h.engine.markets()[&ETH].spec.markout_floor_bps, Some(f("1")));
}

#[test]
fn a_new_listing_from_v54_names_its_floor() {
    let mut h = fresh_v54();
    let events = h.cmd(Command::Admin(AdminCommand::UpsertMarket(crypto(ETH, "ETH-USD"))));
    assert_eq!(rejected(&events), "market 2 names no markout floor");
    h.admin(AdminCommand::UpsertMarket(floored(crypto(ETH, "ETH-USD"), "0.01")));
}

#[test]
fn score_tiers_move_only_after_a_notice_within_half_their_launch_values() {
    let (mut h, _, _) = funded_at(ProtocolVersion::V54);
    let raised = tiers(&[("6", 3), ("12", 7)]);
    assert_eq!(
        rejected(&h.operate(with_tiers(&h, raised.clone()))),
        "a score tier change needs a notice of at least seven days"
    );
    for (out, reason) in [
        (
            tiers(&[("1.999999999", 2), ("8", 5)]),
            "a score tier at 1.999999999 adding 2 bps is outside ±50% of its launch tier at 4 adding 2 bps",
        ),
        (
            tiers(&[("4", 2), ("8", 8)]),
            "a score tier at 8 adding 8 bps is outside ±50% of its launch tier at 8 adding 5 bps",
        ),
        (tiers(&[("4", 2)]), "the score tiers keep their launch count of 2"),
        (tiers(&[("4", 0), ("8", 5)]), "a score tier needs 0 < min_score <= 1000 and extra_spread_bps in 1..=10000"),
    ] {
        assert_eq!(rejected(&h.operate(notice(out, h.ts + NOTICE_MS))), reason);
    }
    let due = h.ts + NOTICE_MS;
    h.operator_admin(notice(raised.clone(), due));
    h.advance(NOTICE_MS - 1);
    assert_eq!(
        rejected(&h.operate(with_tiers(&h, raised.clone()))),
        format!("the score tier change takes effect from {due}")
    );
    h.advance(1);
    h.operator_admin(with_tiers(&h, raised.clone()));
    assert_eq!(h.engine.params().unwrap().toxicity.score_tiers, raised);
    assert_eq!(h.engine.operator_bounds().notice, None);
    // Back to launch needs a notice of its own.
    assert_eq!(
        rejected(&h.operate(with_tiers(&h, launch_score_tiers()))),
        "a score tier change needs a notice of at least seven days"
    );

    let (mut h, _, _) = funded_at(ProtocolVersion::V51);
    assert_eq!(
        rejected(&h.operate(notice(launch_score_tiers(), h.ts + NOTICE_MS))),
        "a score tier notice needs protocol V54"
    );
}

#[test]
fn parameters_take_the_versions_tiers_and_horizon() {
    let mut h = fresh_v54();
    let mut earlier = v54_params();
    earlier.toxicity.tiers = common::params().toxicity.tiers;
    let mut horizon = v54_params();
    horizon.toxicity.horizon_ms = 2_000;
    let mut tierless = v54_params();
    tierless.toxicity.score_tiers.clear();
    let reason = "from protocol V54 toxicity takes score_tiers, no tiers, and a horizon of 5000 ms";
    for params in [earlier, horizon, tierless] {
        assert_eq!(rejected(&h.cmd(Command::Admin(AdminCommand::SetParams(params)))), reason);
    }
    // Parameters set before `V54` may name score tiers, which its activation takes up as the launch
    // tiers.
    let (mut h, _, _) = funded_at(ProtocolVersion::V51);
    let raised = tiers(&[("5", 3), ("9", 6)]);
    let mut params = h.engine.params().unwrap().clone();
    params.toxicity.score_tiers = raised.clone();
    h.operator_admin(AdminCommand::SetParams(params));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V54 });
    assert_eq!(h.engine.params().unwrap().toxicity.score_tiers, raised);
    assert_eq!(h.engine.operator_bounds().launch_score_tiers, Some(raised));
}

#[test]
fn the_top_score_tier_bounds_a_closing_trade_and_the_ceiling_floor() {
    // The widest stress minimum, 5 bps, and the top tier's 5 bps: a ceiling below 10 bps would
    // clamp a launch close, and BTC's first band, 1 bp at the widest 2x, makes its charge 12 bps.
    let mut h = fresh_v54();
    let ceiling = |bps| vec![CloseSpreadCeiling { market: BTC, max_close_spread_bps: bps }];
    assert_eq!(
        rejected(&install(&mut h, ceiling(9))),
        "a nonzero ceiling below the 10bps largest close charge would clamp a launch close"
    );
    assert_eq!(
        rejected(&install(&mut h, ceiling(11))),
        "market 1's close-spread ceiling of 11bps is below its launch close charge of 12.0bps"
    );
    assert!(applied(&install(&mut h, ceiling(12))));
    // A stress minimum and a top add-on that reach the price leave no closing trade able to fill.
    let mut params = v54_params();
    params.stress_levels[1].min_spread_bps = 9_995;
    assert!(params.check_closing_spread().is_err());
    params.stress_levels[1].min_spread_bps = 9_994;
    assert!(params.check_closing_spread().is_ok());
}
