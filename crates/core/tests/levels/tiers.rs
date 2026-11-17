//! An account's own tiers move its closing price, so a toxicity tier it reaches and a volume tier it
//! loses each fire a level at the next price with the index where it was.

use crate::common::close_spread::trade;
use crate::common::events::only_fill;
use crate::common::levels::{levels, open_with, triggered};
use crate::common::toxicity::{FLOOR_BPS, crypto, fill, floored, mark, relisted_v54, standing};
use crate::common::{BTC, ETH, f};
use engine_core::command::{AdminCommand, Command};
use engine_core::levels::LevelKind;
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::HORIZON_MS;
use engine_core::types::Side;
use engine_core::volume_tier::{DAY_MS, day_of};

#[test]
fn a_toxicity_tier_the_account_reaches_fires_its_stop_loss_with_the_index_unchanged() {
    let (mut h, a) =
        relisted_v54([floored(crypto(BTC, "BTC-USD"), FLOOR_BPS), floored(crypto(ETH, "ETH-USD"), FLOOR_BPS)]);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V79 });
    // An ETH long whose markout scores nothing: the index has not moved a horizon later.
    h.price(ETH, "3000");
    open_with(&mut h, &a, ETH, Side::Buy, "1", "3000", levels(None, Some("2990")));
    h.advance(HORIZON_MS);
    h.price(ETH, "3000");
    // At 2990.4 the long closes 1bp lower, above its stop loss; with tier 1's 2bp add-on, below it.
    assert_eq!(triggered(&h.price(ETH, "2990.4")), None);
    // Twenty BTC buys each followed by a 1bp rise score about 4.5 standard errors: tier 1.
    let mut mid = f("100000");
    h.price(BTC, &mid.to_string());
    for _ in 0..20 {
        fill(&mut h, &a, Side::Buy, "0.1", &mid.to_string());
        mid += f("10");
        mark(&mut h, &mid.to_string());
    }
    assert_eq!((standing(&h, a.account).tier, standing(&h, a.account).add_on_bps), (1, 2));
    let events = h.price(ETH, "2990.4");
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::StopLoss));
    assert_eq!(only_fill(&events).price, f("2989.50288"));
}

#[test]
fn a_volume_tier_the_account_loses_fires_its_stop_loss_with_the_index_unchanged() {
    let (mut h, a, _) = crate::common::levels::funded();
    // $600k bought on one day and sold the next counts $500k on each: tier 1 from the sale's horizon.
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "6", "100000"));
    let first = day_of(h.ts);
    h.advance(DAY_MS);
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &a, BTC, Side::Sell, "6", "100000"));
    h.advance(HORIZON_MS);
    h.price(BTC, "100000");
    assert_eq!(h.engine.volume_tier(a.account), 1);
    // Four BTC: at tier 1 the first band covers all $400k at 1bp; at tier 0 it ends at $250k and the
    // rest pays 2bp. At 99,802 the long closes at 99,792.02 at tier 1 and 99,788.29 at tier 0.
    open_with(&mut h, &a, BTC, Side::Buy, "4", "100000", levels(None, Some("99790")));
    assert_eq!(triggered(&h.price(BTC, "99802")), None);
    // The first day leaves the thirty-day window at midnight, and the account falls to tier 0.
    let leaves = (first + 30) * DAY_MS;
    while h.ts + DAY_MS < leaves {
        h.advance(DAY_MS);
        h.cmd(Command::Tick);
    }
    h.advance_to(leaves - 1_000);
    assert_eq!(triggered(&h.price(BTC, "99802")), None);
    assert_eq!(h.engine.volume_tier(a.account), 1);
    h.advance_to(leaves + 1);
    let events = h.price(BTC, "99802");
    assert_eq!(h.engine.volume_tier(a.account), 0);
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::StopLoss));
}
