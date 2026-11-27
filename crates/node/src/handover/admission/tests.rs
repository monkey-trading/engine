//! An admission recovers its signer only for the deployment, key and release it names, and a successor
//! sends only an admission of its own standby key for its own release.

use super::*;

use crate::test_support::scratch_dir;

const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 42161, vault: AccountId([0x7a; 20]) };

fn signer() -> SigningKey {
    SigningKey::from_slice(&[0x66; 32]).unwrap()
}

#[test]
fn an_admission_recovers_its_signer_only_for_its_domain_key_and_release() {
    let (key, release) = (AccountId([0x5b; 20]), [0x2e; 32]);
    let admission = Admission::sign(&signer(), DOMAIN, key, release);
    let address = eip712::address(signer().verifying_key());
    assert_eq!(admission.signer(DOMAIN), Some(address));
    assert!(admission.names(key, release) && !admission.names(AccountId([0x5c; 20]), release));
    assert!(!admission.names(key, [0x2f; 32]), "another release");
    let other_vault = CheckpointDomain { vault: AccountId([0x7b; 20]), ..DOMAIN };
    let moved_key = Admission { key: AccountId([0x5c; 20]), ..admission.clone() };
    let moved_release = Admission { release: B256::repeat_byte(0x2f), ..admission.clone() };
    for (what, admission, domain) in [
        ("another vault", &admission, other_vault),
        ("another key", &moved_key, DOMAIN),
        ("another release", &moved_release, DOMAIN),
    ] {
        assert_ne!(admission.signer(domain), Some(address), "{what}");
    }
}

#[test]
fn a_successor_sends_only_an_admission_of_its_own_key_for_its_own_release() {
    let dir = scratch_dir("handover-admission-requester");
    let keys = Arc::new(StandbyKeys::for_test(&dir));
    let (own, release) = (keys.address(), keys.release().id());
    let write =
        |admission: &Admission| std::fs::write(dir.join(ADMISSION_FILE), serde_json::to_vec(admission).unwrap());
    assert_eq!(Requester::with_admission(keys.clone(), &dir).admission, None, "no file");

    let admitted = Admission::sign(&signer(), DOMAIN, own, release);
    write(&admitted).unwrap();
    assert_eq!(Requester::with_admission(keys.clone(), &dir).admission, Some(admitted));
    for (what, admission) in [
        ("another key", Admission::sign(&signer(), DOMAIN, AccountId([0x5c; 20]), release)),
        ("another release", Admission::sign(&signer(), DOMAIN, own, [0x2f; 32])),
    ] {
        write(&admission).unwrap();
        assert_eq!(Requester::with_admission(keys.clone(), &dir).admission, None, "{what}");
    }
    std::fs::write(dir.join(ADMISSION_FILE), br#"{"key":"0x00","release":"0x00","signature":"0x00","more":1}"#)
        .unwrap();
    assert!(Admission::read(&dir).is_err(), "a file that does not parse");
    assert_eq!(Requester::with_admission(keys, &dir).admission, None, "a file that does not parse");
}
