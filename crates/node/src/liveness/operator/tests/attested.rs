//! A member with no promotable standby key, as after a promotion consumed its registration: its
//! statuses count on the key its link attests, once this enclave verifies the attestation.

use super::*;
use crate::config::TeeProvider;
use crate::handover::evidence::Quoting;
use crate::standby_key::StandbyKeys;
use crate::standby_key::vouch::request;
use crate::tee::MockAttestor;
use crate::test_support::scratch_dir;

const QUOTING: Quoting<'static> = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "" };

/// How a case sets the chain, given the attested key.
type Change<'a> = &'a dyn Fn(&mut ChainState, Address);

/// In a set of two whose other member's promotable key a promotion consumed, that member confirms this
/// log on the replacement key its link attests, once the quote proves, for that member, the release
/// its candidacy registered and the timelock approved, and the schedule never took the key: the start
/// check passes and checkpoints may pass, with no registration sent yet.
#[tokio::test(start_paused = true)]
async fn a_member_with_no_promotable_key_confirms_on_the_key_its_link_attests() {
    let rig = attested("attested-member", AccountId([MEMBER; 20]), |_, _| {}).await;
    assert_eq!(rig.gate.confirmed(), 10, "the attested key's status confirmed nothing");
    assert!(rig.gate.sequences(), "the start check did not pass on it");
    rig.task.abort();
}

/// An attestation counts for nothing when it names another candidate, proves a release the member's
/// candidacy does not name or the timelock has not approved, or is of a key the schedule took, as a
/// key a promotion consumed is.
#[tokio::test(start_paused = true)]
async fn an_attestation_the_member_cannot_stand_on_confirms_nothing() {
    let member = AccountId([MEMBER; 20]);
    let other_release = |chain: &mut ChainState, _: Address| chain.release = B256::repeat_byte(0x99);
    let unapproved = |chain: &mut ChainState, _: Address| chain.approved = false;
    let taken = |chain: &mut ChainState, key: Address| chain.holders.push((key, Address::repeat_byte(MEMBER)));
    let cases: [(&str, AccountId, Change<'_>); 4] = [
        ("another candidate's", AccountId([0x77; 20]), &|_, _| {}),
        ("another release", member, &other_release),
        ("an unapproved release", member, &unapproved),
        ("a key the schedule took", member, &taken),
    ];
    for (case, attesting, change) in cases {
        let rig = attested("attested-refused", attesting, change).await;
        assert_eq!(rig.gate.confirmed(), 0, "{case}: the attestation confirmed the log");
        assert!(!rig.gate.sequences(), "{case}: the start check passed on it");
        rig.task.abort();
    }
}

// ── Helpers ──

/// A rule whose other member, once its promotable key is gone, signs its statuses with a new standby
/// key whose attestation, quoted for `attesting`, its link serves, on a chain `change` sets given that
/// key; the rule has read the members afresh and run a few rounds.
async fn attested(name: &str, attesting: AccountId, change: impl FnOnce(&mut ChainState, Address)) -> Rig {
    let rig = Rig::spawn(10);
    let keys = StandbyKeys::for_test(&scratch_dir(name));
    let attestation = request(&QUOTING, &MockAttestor, &keys, &DOMAIN.standby, attesting, TRUE_MS).await.unwrap();
    let key = Address::from(keys.address().0);
    rig.chain.set(|chain| {
        chain.members[1].2 = Address::ZERO;
        chain.release = B256::from(keys.release().id());
        change(chain, key);
    });
    let signing = Arc::new(keys.signing_for_test());
    rig.members.0.lock().unwrap().insert("member:1".into(), (AccountId([MEMBER; 20]), signing, None));
    rig.members.2.lock().unwrap().insert("member:1".into(), attestation);
    rig.answer(Some(10));
    // The members the first read listed are read again once they are a minute old.
    tokio::time::sleep(MEMBERS_EVERY).await;
    settle().await;
    rig
}
