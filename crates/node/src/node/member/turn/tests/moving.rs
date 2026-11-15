//! A member's holding while a change of signer is not yet final.

use super::*;

/// A member whose own turn's keys are sealed follows while the vault's latest signer is another key
/// than its finalized one, as just after a promotion past it, rather than reopening those keys; once
/// the two agree again, it holds.
#[tokio::test]
async fn a_member_follows_while_a_change_of_signer_is_not_final_and_holds_once_it_is() {
    let fixture = Fixture::start("member-signer-moving", true).await;
    let operator = fixture.operator(true).await;
    fixture.registry.name(&operator.host);
    fixture.seal_own_turn();
    fixture.time.set(OWN + 1);
    *fixture.registry.moving.lock().unwrap() = Some(AccountId([0x77; 20]));
    let mut member = fixture.member();
    tokio::select! {
        _ = member.until_turn() => panic!("the member reopened keys a change of signer moves past"),
        () = operator.wait_for_connections(1) => {}
    }
    *fixture.registry.moving.lock().unwrap() = None;
    let mut member = fixture.member();
    let turn = crate::test_support::wait_for("the member to hold its turn", member.until_turn()).await;
    assert!(matches!(turn.unwrap(), Turn::Hold));
}
