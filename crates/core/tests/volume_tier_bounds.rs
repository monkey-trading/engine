//! The operator's bounds on `V59`'s volume tier table, through `Engine::apply`: a table moves only as
//! a seven-day notice names it, keeps its launch tiers, never lowers a threshold or a day divisor or
//! raises a share, and with every listed market's bands keeps each tier's spread from its launch
//! spread to twice it; a listing's new bands are held to the same at every tier. Votes are held to
//! the same bounds (`parameter_votes/volume_tiers.rs`).

mod common;

use common::close_spread::trade;
use common::events::only_fill;
use common::session_keys::funded_at;
use common::spreads::{applied, refused, spread_notice, tenths, with_spreads};
use common::{BTC, Harness, f, market};
use engine_core::command::AdminCommand;
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AssetClass, MarketId, Millis, Side};
use engine_core::volume_tier::{VolumeTier, launch_volume_tiers};

const V59: ProtocolVersion = ProtocolVersion::V59;
/// A market the harness does not list.
const SOL: MarketId = 7;

#[test]
fn a_table_change_applies_only_from_its_notice_and_spends_it() {
    let (mut h, _, _) = funded_at(V59);
    let raised = with_tiers(|tiers| tiers[1].threshold = f("2000000"));
    let set = AdminCommand::SetParams(params(&h, raised.clone()));
    assert!(refused(&h.operate(set.clone())).contains("needs a notice of at least seven days"));
    let due = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(notice(raised.clone(), due))));
    assert!(refused(&h.operate(set.clone())).contains(&format!("takes effect from {due}")));
    h.advance_to(due);
    assert!(applied(&h.operate(set)));
    assert_eq!(h.engine.params().unwrap().volume_tiers, raised);
    assert_eq!(h.engine.operator_bounds().notice, None, "the applied table spent the notice");
}

#[test]
fn a_table_more_generous_than_launch_or_of_another_shape_is_refused() {
    let (mut h, _, _) = funded_at(V59);
    let due = h.ts + NOTICE_MS;
    let cases: [(Change, &str); 5] = [
        (|tiers| tiers[1].threshold = f("999999"), "volume tier 1's threshold and day_divisor stay at or above"),
        (|tiers| tiers[2].day_divisor = 4, "volume tier 2's threshold and day_divisor stay at or above"),
        (|tiers| tiers[0].best_price_share_bps = 2_501, "volume tier 0's best_price_share_bps 2501 is above"),
        (|tiers| tiers.truncate(3), "keeps its 4 launch tiers"),
        (|tiers| tiers[2].best_price_share_bps = 4_000, "never fall with the tier"),
    ];
    for (change, reason) in cases {
        let refusal = refused(&h.operate(notice(with_tiers(change), due))).to_owned();
        assert!(refusal.contains(reason), "{reason}: {refusal}");
    }
    // Less generous on every count is within the bound.
    let tighter = with_tiers(|tiers| {
        tiers[1].threshold = f("5000000");
        tiers[2].day_divisor = 10;
        tiers[3].best_price_share_bps = 20_000;
    });
    assert!(applied(&h.operate(notice(tighter, due))));
}

#[test]
fn a_noticed_listing_is_held_to_each_tiers_launch_spread_when_it_applies() {
    let (mut h, _, _) = funded_at(V59);
    // BTC's bands read 1bp to $1M, 2bp to $5M and 5bp above. 4bp to $5M is twice launch, alone and at
    // every tier's launch share.
    let wider = tenths(&[(Some(1_000_000), 10), (Some(5_000_000), 40), (None, 50)]);
    let (listing, listed_at) = spread_notice(&h, BTC, wider.clone());
    assert!(applied(&h.operate(listing)));
    // An eighth at tier 0 charges 2bp from $125k, twice launch's 1bp: within the bound until the
    // wider bands apply.
    let eighth = with_tiers(|tiers| tiers[0].best_price_share_bps = 1_250);
    let due = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(notice(eighth.clone(), due))));
    h.advance_to(due.max(listed_at));
    assert!(applied(&h.operate(AdminCommand::SetParams(params(&h, eighth)))));
    let refusal = refused(&h.operate(AdminCommand::UpsertMarket(with_spreads(&h, BTC, wider.clone())))).to_owned();
    assert!(refusal.contains("market 1 at volume tier 0"), "{refusal}");
    assert!(refusal.contains("a spread rate of 40 tenths of a bp above 125000"), "{refusal}");
    // With those bands in force, the same table is refused as a notice.
    let (mut h, _, _) = funded_at(V59);
    let (listing, listed_at) = spread_notice(&h, BTC, wider.clone());
    assert!(applied(&h.operate(listing)));
    h.advance_to(listed_at);
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(with_spreads(&h, BTC, wider)))));
    let eighth = with_tiers(|tiers| tiers[0].best_price_share_bps = 1_250);
    let refusal = refused(&h.operate(notice(eighth, h.ts + NOTICE_MS))).to_owned();
    assert!(refusal.contains("market 1 at volume tier 0"), "{refusal}");
}

#[test]
fn a_listing_notice_under_a_cut_share_is_refused_when_announced() {
    let (mut h, _, _) = funded_at(V59);
    let eighth = with_tiers(|tiers| tiers[0].best_price_share_bps = 1_250);
    let due = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(notice(eighth.clone(), due))));
    h.advance_to(due);
    assert!(applied(&h.operate(AdminCommand::SetParams(params(&h, eighth)))));
    let wider = tenths(&[(Some(1_000_000), 10), (Some(5_000_000), 40), (None, 50)]);
    let refusal = refused(&h.operate(spread_notice(&h, BTC, wider).0)).to_owned();
    assert!(refusal.contains("market 1 at volume tier 0: a spread rate of 40 tenths"), "{refusal}");
    assert!(h.engine.operator_bounds().markups.is_empty(), "no listing notice was recorded");
}

#[test]
fn a_raised_threshold_holds_every_tier_below_it_to_the_launch_spread_its_accounts_paid() {
    // BTC's second band at 4bp, twice launch's 2bp, is within the bound at every tier's own share.
    // Raising tier 3's threshold drops an account that reached it at launch as low as tier 0, which
    // pays 4bp from $250k to $2.5m where tier 3 paid 1bp at launch.
    let (mut h, _, _) = funded_at(V59);
    let wider = tenths(&[(Some(1_000_000), 10), (Some(5_000_000), 40), (None, 50)]);
    let (listing, listed_at) = spread_notice(&h, BTC, wider.clone());
    assert!(applied(&h.operate(listing)));
    h.advance_to(listed_at);
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(with_spreads(&h, BTC, wider.clone())))));
    let raised = with_tiers(|tiers| tiers[3].threshold = f("500000000"));
    let refusal = refused(&h.operate(notice(raised.clone(), h.ts + NOTICE_MS))).to_owned();
    let demoted = "market 1 at volume tier 0, for an account at tier 3 at launch: a spread rate of 40 tenths";
    assert!(refusal.contains(demoted), "{refusal}");
    // With launch's bands the raise is within the bound, and then the same bands are refused when
    // announced.
    let (mut h, _, _) = funded_at(V59);
    let due = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(notice(raised.clone(), due))));
    h.advance_to(due);
    assert!(applied(&h.operate(AdminCommand::SetParams(params(&h, raised)))));
    let refusal = refused(&h.operate(spread_notice(&h, BTC, wider).0)).to_owned();
    assert!(refusal.contains(demoted), "{refusal}");
}

#[test]
fn activation_over_a_listing_outside_a_tiers_bound_is_refused_until_it_is_relisted() {
    // Before V59 a noticed listing ends BTC's 2bp band at $2m, within twice launch at every notional
    // alone. Tier 3's first band, two and a half times $1m, then ends at $2m instead of $2.5m, and pays
    // 4bp from $2m to $2.5m where its launch spread is 1bp.
    let (mut h, _, _) = funded_at(ProtocolVersion::V58);
    let moved = tenths(&[(Some(1_000_000), 10), (Some(2_000_000), 20), (Some(5_000_000), 40), (None, 50)]);
    let (listing, listed_at) = spread_notice(&h, BTC, moved.clone());
    assert!(applied(&h.operate(listing)));
    h.advance_to(listed_at);
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(with_spreads(&h, BTC, moved)))));
    let activate = AdminCommand::ActivateProtocol { version: V59 };
    let refusal = refused(&h.cmd(engine_core::command::Command::Admin(activate.clone()))).to_owned();
    assert!(refusal.contains("market 1 at volume tier 3: a spread rate of 40 tenths"), "{refusal}");
    // Relisted at its launch bands, the log activates.
    let launch = h.engine.operator_bounds().launch_bands[&BTC].spread_tiers.clone();
    let (listing, listed_at) = spread_notice(&h, BTC, launch.clone());
    assert!(applied(&h.operate(listing)));
    h.advance_to(listed_at);
    assert!(applied(&h.operate(AdminCommand::UpsertMarket(with_spreads(&h, BTC, launch)))));
    h.admin(activate);
    assert_eq!(h.engine.protocol_version(), V59);
}

#[test]
fn before_v59_a_table_prices_nothing_and_its_activation_takes_one_no_more_generous_than_launch() {
    let (mut h, client, _) = funded_at(ProtocolVersion::V58);
    let due = h.ts + NOTICE_MS;
    assert!(refused(&h.operate(notice(launch_volume_tiers(), due))).contains("needs protocol V59"));
    // A table set before V59 prices nothing, and one wider than launch's is refused at activation.
    let wider = with_tiers(|tiers| tiers[0].best_price_share_bps = 5_000);
    assert!(applied(&h.operate(AdminCommand::SetParams(params(&h, wider)))));
    assert_eq!(only_fill(&trade(&mut h, &client, BTC, Side::Buy, "6", "100000")).price, f("100010"));
    let activate = engine_core::command::Command::Admin(AdminCommand::ActivateProtocol { version: V59 });
    let refusal = refused(&h.cmd(activate.clone())).to_owned();
    assert!(refusal.contains("volume tier 0's best_price_share_bps 5000 is above its launch value 2500"), "{refusal}");
    // A narrower one is taken up as the launch table.
    let narrower = with_tiers(|tiers| tiers[0].best_price_share_bps = 2_000);
    assert!(applied(&h.operate(AdminCommand::SetParams(params(&h, narrower.clone())))));
    assert!(applied(&h.cmd(activate)));
    assert_eq!(h.engine.operator_bounds().launch_volume_tiers, Some(narrower));
}

#[test]
fn a_first_listing_after_a_share_cut_is_held_to_each_tiers_spread_at_its_own_bands() {
    // Tier 0's share cut to an eighth, a market listed at 1bp to $1m and 3bp above charges tier 0 3bp
    // from $125k to $250k, where its own bands charge 1bp at the launch quarter.
    let (mut h, _, _) = funded_at(V59);
    let eighth = with_tiers(|tiers| tiers[0].best_price_share_bps = 1_250);
    let due = h.ts + NOTICE_MS;
    assert!(applied(&h.operate(notice(eighth.clone(), due))));
    h.advance_to(due);
    assert!(applied(&h.operate(AdminCommand::SetParams(params(&h, eighth)))));
    let listing = MarketSpec {
        spread_tiers: tenths(&[(Some(1_000_000), 10), (None, 30)]),
        markout_floor_bps: Some(f("1")),
        ..market(SOL, "SOL-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)
    };
    let due = h.ts + NOTICE_MS;
    let adoption = AdminCommand::AdoptCloseSpreadCeiling {
        market: SOL,
        max_close_spread_bps: 50,
        effective_at: due,
        listing: Some(listing.clone()),
    };
    assert!(applied(&h.operate(adoption)));
    h.advance_to(due);
    let refusal = refused(&h.operate(AdminCommand::UpsertMarket(listing))).to_owned();
    assert!(refusal.contains("market 7's first listing at volume tier 0: a spread rate of 30 tenths"), "{refusal}");
    assert!(!h.engine.markets().contains_key(&SOL));
}

// ── Helpers ──

/// An edit of a tier table.
type Change = fn(&mut Vec<VolumeTier>);

/// The launch table with `change` made.
fn with_tiers(change: impl FnOnce(&mut Vec<VolumeTier>)) -> Vec<VolumeTier> {
    let mut tiers = launch_volume_tiers();
    change(&mut tiers);
    tiers
}

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
        effective_at,
    })
}
