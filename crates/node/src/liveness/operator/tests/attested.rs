//! A member with no promotable standby key, as after a promotion consumed its registration: its
//! statuses count on the key its link attests, once this enclave verifies the attestation.

use super::*;
use crate::config::TeeProvider;
use crate::handover::evidence::Quoting;
use crate::standby_key::StandbyKeys;
use crate::standby_key::vouch::{VouchRequest, request};
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
/// candidacy does not name or the timelock has not approved, is of a key the schedule took, as a key a
/// promotion consumed is, or carries a quote over other report data than its own.
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
    // A forged attestation: eligible in every other way, its quote binds another nonce than it states.
    let rig = attested_as("attested-forged", member, |_, _| {}, |request| request.nonce += 1).await;
    assert_eq!(rig.gate.confirmed(), 0, "a forged quote confirmed the log");
    assert!(!rig.gate.sequences(), "the start check passed on a forged quote");
    rig.task.abort();
}

/// A member whose attested key an earlier read verified keeps it through a re-check that fails once, its
/// statuses still counting, until a check refutes the key: the schedule has taken it.
#[tokio::test(start_paused = true)]
async fn a_failed_recheck_keeps_the_verified_attested_key_until_a_check_refutes_it() {
    let (rig, key) = attested_keyed("attested-kept", AccountId([MEMBER; 20]), |_, _| {}, |_| {}, 14).await;
    assert_eq!(rig.gate.confirmed(), 10, "the attested key's status confirmed nothing");
    // The member's attestation fails to answer at the next members read, as its status still does.
    rig.members.2.lock().unwrap().clear();
    tokio::time::sleep(MEMBERS_EVERY).await;
    settle().await;
    rig.answer(Some(12));
    settle().await;
    assert_eq!(rig.gate.confirmed(), 12, "one failed re-check dropped the key");
    // The schedule takes the key: the next failed re-check is refuted by it, and the status counts for nothing.
    rig.chain.set(|chain| chain.holders.push((key, Address::repeat_byte(MEMBER))));
    tokio::time::sleep(MEMBERS_EVERY).await;
    settle().await;
    rig.answer(Some(14));
    settle().await;
    assert_eq!(rig.gate.confirmed(), 12, "a refuted key still confirmed");
    rig.task.abort();
}

/// A failed re-check warns once, until one passes again.
#[test]
fn a_failed_recheck_warns_once_per_episode() {
    let (kept, member) = (super::super::attested::Kept::default(), AccountId([MEMBER; 20]));
    assert!(kept.first_failure(member) && !kept.first_failure(member), "the second failure warned again");
    assert!(kept.first_failure(AccountId([0x22; 20])), "another member's episode");
}

// ── Helpers ──

/// A rule whose other member, once its promotable key is gone, signs its statuses with a new standby
/// key whose attestation, quoted for `attesting`, its link serves, on a chain `change` sets given that
/// key; the rule has read the members afresh and run a few rounds.
async fn attested(name: &str, attesting: AccountId, change: impl FnOnce(&mut ChainState, Address)) -> Rig {
    attested_as(name, attesting, change, |_| {}).await
}

/// `attested`, with the attestation as `tamper` leaves it after the quote.
async fn attested_as(
    name: &str,
    attesting: AccountId,
    change: impl FnOnce(&mut ChainState, Address),
    tamper: impl FnOnce(&mut VouchRequest),
) -> Rig {
    attested_keyed(name, attesting, change, tamper, 10).await.0
}

/// `attested_as` over a log whose tip is `tip`, with the standby key the member attests too.
async fn attested_keyed(
    name: &str,
    attesting: AccountId,
    change: impl FnOnce(&mut ChainState, Address),
    tamper: impl FnOnce(&mut VouchRequest),
    tip: u64,
) -> (Rig, Address) {
    let rig = Rig::spawn(tip);
    let keys = StandbyKeys::for_test(&scratch_dir(name));
    let mut attestation = request(&QUOTING, &MockAttestor, &keys, &DOMAIN.standby, attesting, TRUE_MS).await.unwrap();
    tamper(&mut attestation);
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
    (rig, key)
}
