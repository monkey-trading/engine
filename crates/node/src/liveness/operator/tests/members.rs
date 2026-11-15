//! Which members the rule asks for confirmations: every member of the turn's set but this node's own.

use super::*;

/// Past a boundary the turn names the successor while this node still holds the keys: it asks every
/// member but itself, the successor included, so its log is still confirmed and a handover's final
/// checkpoint can be.
#[tokio::test(start_paused = true)]
async fn past_a_boundary_the_incumbent_asks_its_successor() {
    let rig = Rig::spawn(10);
    rig.chain.set(|chain| chain.named = (Address::repeat_byte(MEMBER), 1));
    rig.answer(Some(10));
    settle().await;
    assert_eq!(rig.gate.confirmed(), 10, "the successor's copy confirmed nothing");
    assert!(rig.gate.sequences(), "the incumbent stopped with its successor following it");
    rig.task.abort();
}
