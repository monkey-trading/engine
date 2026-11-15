//! A member re-reads the operator in effect after each stint, so a promotion that moves the vault's
//! signer moves its following at once, not at the next boundary.

use super::*;

/// The vault naming another enclave than the one followed gives the operator to follow now, at the
/// registry's hostname; the same enclave gives none, so the member keeps its stream.
#[tokio::test]
async fn a_member_moves_on_only_when_the_vault_names_another_enclave() {
    let fixture = Fixture::start("member-moved-on", false).await;
    fixture.registry.name("promoted.example");
    let member = fixture.member();
    let named = AccountId(EnclaveKey::test_key().address());
    assert!(member.moved_on(named).await.is_none(), "moved on while the vault named the followed enclave");
    let moved = member.moved_on(AccountId([0xee; 20])).await.expect("the vault names another enclave");
    assert_eq!((moved.enclave, moved.origin.as_str()), (named, "https://promoted.example"));
    // A read that fails keeps the member on its stream.
    fixture.registry.name("");
    assert!(member.moved_on(AccountId([0xee; 20])).await.is_none(), "moved on without a read");
}
