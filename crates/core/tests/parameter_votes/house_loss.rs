//! Parameter votes held to the house loss trigger's bound from `V59`: a voted change needs no notice,
//! but keeps level 2's launch threshold of zero at level 2 or above, at zero or below.

use super::*;
use engine_core::Fixed;
use engine_core::house_loss::LossTrigger;

/// The baseline parameters with level 2's loss threshold at `threshold`.
fn threshold(h: &Harness, threshold: Option<Fixed>) -> AdminCommand {
    with_params(h, |params| params.stress_levels[2].trigger.get_or_insert_with(Default::default).house_loss = threshold)
}

#[test]
fn a_vote_keeps_the_loss_threshold_at_its_level_and_at_most_launchs() {
    let mut h = governed_at(ProtocolVersion::V59, TTL);
    let launch = vec![LossTrigger { level: 2, house_loss: Fixed::ZERO }];
    assert_eq!(h.engine.operator_bounds().launch_loss_triggers, Some(launch));
    let (dropped, raised, lowered) = (threshold(&h, None), threshold(&h, Some(f("1"))), threshold(&h, Some(f("-1"))));
    for (proposal, command) in [(1, dropped.clone()), (2, raised.clone()), (3, lowered.clone())] {
        assert!(matches!(pass(&mut h, proposal, false, command).last(), Some(Event::ProposalScheduled { .. })));
    }
    // The operator's own change is refused while rotation is on, whatever it names.
    rejected(&h.operate(lowered.clone()), "rotation");
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 1, dropped), "keep a house loss trigger at level 2 or above");
    rejected(&apply(&mut h, 2, raised), "at most its launch threshold 0");
    assert_eq!(apply(&mut h, 3, lowered).last(), Some(&Event::AdminApplied));
    let trigger = h.engine.params().expect("set").stress_levels[2].trigger.clone().expect("a trigger");
    assert_eq!(trigger.house_loss, Some(f("-1")));
    assert!(h.engine.operator_bounds().notice.is_none(), "no parameter notice was needed");
}
