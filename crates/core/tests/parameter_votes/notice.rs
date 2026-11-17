//! The status notice from `V78` while votes govern: the operator in office sets and clears it, since it
//! moves no value, and no vote is needed or carried for it.

use super::*;
use engine_core::notice::Notice;

#[test]
fn the_operator_sets_and_clears_a_notice_while_votes_govern() {
    let mut h = governed_under(ProtocolVersion::V78, TTL);
    // The parameters are still the vote's.
    rejected(&h.operate(AdminCommand::SetParams(h.engine.params().unwrap().clone())), "only a passed vote");

    let text = "Withdrawals are paused while we investigate.";
    assert_eq!(h.operate(AdminCommand::SetNotice { text: text.into() }).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.notice(), Some(&Notice { text: text.into(), set_at_ms: h.ts }));
    assert_eq!(h.operate(AdminCommand::ClearNotice).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.notice(), None);
}
