//! A close-out's later group members at the volume tier the group's earlier fills leave (#1008): a
//! close-out counts its volume at once, so closing a group's first member can lift the account's tier, and
//! the next member fills at that tier's best-price share and fee discount. From `V84` the step scores it
//! there, as it scores it at the house NAV the earlier fills leave; before it the step scored it at the tier
//! before the step.

use super::*;
use common::AAPL;
use common::portfolio::params_with;
use common::weekend::{genesis_with_params, globex};
use engine_core::params::ProtocolParams;
use engine_core::types::MarketId;
use engine_core::volume_tier::{VolumeTier, launch_volume_tiers};

#[test]
fn a_groups_later_member_is_scored_at_the_tier_its_earlier_fills_leave() {
    // On $250k, BTC long at 100,000 and ETH short at 4,000, each $1M, and $200k of AAPL long, held a minute:
    // $2.2M of counted volume, $300k short of tier 1, where the base profit fee is waived. ETH falls to 3,200,
    // a $200k gain, and BTC to 58,400: the account is about $29k short of its level. Closing the pair
    // releases about $43k of level and pays ETH's fee: $20k at tier 0, nothing at tier 1, which BTC's
    // $584k close-out lifts the account to. Scored at tier 1 the pair restores the level and AAPL, releasing
    // $20k, stays open; scored at tier 0 neither restores it, and AAPL, gaining more per dollar, closes first.
    assert_eq!(close_out(V84, 58_400, 10_000), (vec![BTC, ETH], f("1000")));
    assert_eq!(close_out(V83, 58_400, 10_000), (vec![AAPL, BTC, ETH], Fixed::ZERO));
}

#[test]
fn a_groups_later_member_is_scored_at_the_best_price_share_its_earlier_fills_leave() {
    // As above, with no fee discount at either tier: tier 1's first spread band, four times tier 0's, alone
    // makes ETH's $800k close about $55 cheaper. Somewhere BTC's fall leaves the account short of its level
    // by more than closing the pair restores at tier 0's spread and less than at tier 1's: there the pair
    // closes alone from `V84`, while `V83` closes AAPL first.
    let shared = (58_950..59_250).find(|btc| close_out(V84, *btc, 0) == (vec![BTC, ETH], f("1000")));
    let btc = shared.expect("a fall the share alone decides");
    assert_eq!(close_out(V83, btc, 0), (vec![AAPL, BTC, ETH], Fixed::ZERO), "BTC at {btc}");
}

// ── Helpers ──

/// The close-out above at `version`, with BTC falling to `btc` and tier 1 taking `fee_discount_bps` off the
/// base fee: the markets it closes in order, and the AAPL left.
fn close_out(version: ProtocolVersion, btc: i64, fee_discount_bps: i64) -> (Vec<MarketId>, Fixed) {
    let tiers: Vec<VolumeTier> = (launch_volume_tiers().into_iter().take(2).enumerate())
        .map(|(n, tier)| VolumeTier {
            threshold: Fixed::from_int(2_500_000 * n as i64),
            day_divisor: 1,
            best_price_share_bps: 2_500 * (1 + 3 * n as i64),
            fee_discount_bps: fee_discount_bps * n as i64,
            ..tier
        })
        .collect();
    let params = ProtocolParams { volume_tiers: tiers, ..params_with(pairs(5_000)) };
    let mut h = genesis_with_params(version, globex(vec![]), 750, 0, params);
    let a = client(&mut h, 1, "250000");
    for (market, side, qty, price) in
        [(BTC, Side::Buy, "10", "100000"), (ETH, Side::Sell, "250", "4000"), (AAPL, Side::Buy, "1000", "200")]
    {
        h.price(market, price);
        only_fill(&trade(&mut h, &a, market, side, qty, price));
    }
    h.advance(61_000);
    for (market, price) in [(BTC, "100000"), (ETH, "3200"), (AAPL, "200")] {
        assert!(fills(&h.price(market, price)).is_empty());
    }
    assert_eq!(h.engine.volume_tier(a.account), 0);
    let events = h.price(BTC, &btc.to_string());
    let closed = fills(&events).into_iter().filter(|fill| fill.liquidation).map(|fill| fill.market).collect();
    (closed, h.engine.accounts()[&a.account].position(AAPL).qty)
}
