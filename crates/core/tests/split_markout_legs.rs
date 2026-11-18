//! From `V54` a fill's markout leg is indexed; a split zeroes its market's markouts and leaves every other
//! market's, so the index must lose the first and keep the second, as a rebuild from state would.

mod common;

use common::close_spread::trade;
use common::events::only_fill;
use common::session_keys::funded_at;
use common::{AAPL, BTC, ny};
use engine_core::command::{AdminCommand, Command};
use engine_core::corporate::{Adjustment, CorporateAction};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;
use std::collections::BTreeSet;

#[test]
fn a_split_drops_its_markets_markout_legs_and_keeps_another_markets() {
    let (mut h, client, _) = funded_at(ProtocolVersion::LATEST);
    h.price(AAPL, "80");
    // An open leg in each market, then a partial close that leaves AAPL a closed leg too.
    only_fill(&trade(&mut h, &client, AAPL, Side::Buy, "2", "80"));
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "1", "100000"));
    only_fill(&trade(&mut h, &client, AAPL, Side::Sell, "1", "80"));
    let action = CorporateAction {
        id: "legs".into(),
        market: AAPL,
        previous_close: ny(2026, 10, 30, 16, 0),
        ex_open: ny(2026, 11, 2, 9, 30),
        adjustment: Adjustment::Split { new_units: 2, old_units: 1 },
    };
    assert!(h.operate(AdminCommand::ScheduleCorporateAction(action.clone())).contains(&Event::AdminApplied));
    // Their horizon is seconds, so each is moved a fortnight out to still wait when the split applies.
    let mut state = serde_json::to_value(&h.engine).unwrap();
    for markout in state["markouts"].as_array_mut().unwrap() {
        markout["due"] = (markout["due"].as_i64().unwrap() + 14 * 86_400_000).into();
    }
    h.engine = serde_json::from_value(state).unwrap();
    h.engine.restore_indexes();
    assert_eq!(h.engine.markout_leg_markets(), BTreeSet::from([AAPL, BTC]));
    assert!(h.engine.indexes_match_restore());
    h.advance_to(action.ex_open);
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|e| matches!(e, Event::CorporateSplit { .. })), "{events:?}");
    assert_eq!(h.engine.markout_leg_markets(), BTreeSet::from([BTC]), "only the split market's legs leave");
    assert!(h.engine.indexes_match_restore());
}
