//! The sealed secrets on the host's disk: what a restart opens, what another release cannot, and
//! what each thing the host can do with the file achieves.

use super::*;

use k256::ecdsa::SigningKey;

use crate::config::{SecretName, SecretValue};
use crate::log::CheckpointDomain;
use crate::secrets::Bundle;
use crate::tee::keys::KeySet;
use crate::tee::sealing::{KeyStore, MockSealer};
use crate::test_support::scratch_dir;
use engine_core::types::AccountId;

const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31337, vault: AccountId([0x0a; 20]) };
const RELEASE: [u8; 48] = [0x11; 48];
const CONFIG: [u8; 32] = [0xc1; 32];

fn sealer(launch_digest: [u8; 48]) -> Box<dyn Sealer> {
    Box::new(MockSealer { launch_digest })
}

fn store(dir: &Path) -> SecretStore {
    SecretStore::open(dir, sealer(RELEASE), CONFIG).unwrap()
}

/// A bundle at `version` holding `value`, signed by the operator's key.
fn bundle(version: u64, value: &str) -> SignedBundle {
    let name = SecretName::try_from("alpha_key".to_owned()).unwrap();
    let secrets = [(name, SecretValue::test(value))].into_iter().collect();
    Bundle { version, secrets }.sign(DOMAIN, &SigningKey::from_slice(&[0x42; 32]).unwrap())
}

#[test]
fn a_restart_opens_what_a_delivery_sealed_and_the_file_holds_no_plaintext() {
    let dir = scratch_dir("secrets-restart");
    let first = store(&dir);
    assert_eq!((first.current(), first.unreadable()), (None, None), "a fresh release has no secrets and no problem");
    let delivered = bundle(1, "FAKE-sealed-value");
    assert_eq!(first.replace(delivered.clone()).unwrap(), Replaced::Sealed);
    assert_eq!(first.version(), Some(1));

    let reopened = store(&dir);
    assert_eq!(reopened.current(), Some(delivered));
    assert_eq!(reopened.unreadable(), None);
    let file = std::fs::read_to_string(file_for(&dir, &reopened.release())).unwrap();
    assert!(!file.contains("FAKE-sealed-value"), "the sealed file holds the value");
    assert!(!file.contains(&hex::encode("FAKE-sealed-value")), "the sealed file holds the value's hex");
}

#[test]
fn another_release_or_config_does_not_open_it_even_under_its_name() {
    let dir = scratch_dir("secrets-other-release");
    let ours = store(&dir);
    ours.replace(bundle(1, "FAKE-sealed-value")).unwrap();
    let path = file_for(&dir, &ours.release());

    // Another release looks under its own name and finds nothing.
    assert_eq!(SecretStore::open(&dir, sealer([0x22; 48]), CONFIG).unwrap().current(), None);
    assert_eq!(SecretStore::open(&dir, sealer(RELEASE), [0xc2; 32]).unwrap().current(), None);

    // A host that gives it this file's name gets a store with nothing in it, and the reason.
    for (what, launch_digest, config) in [("code", [0x22; 48], CONFIG), ("config", RELEASE, [0xc2; 32])] {
        let other = Release { launch_digest, config_sha256: config };
        std::fs::copy(&path, file_for(&dir, &other)).unwrap();
        let opened = SecretStore::open(&dir, sealer(launch_digest), config).unwrap();
        assert_eq!(opened.current(), None, "another {what} opened it");
        assert!(opened.unreadable().unwrap().contains("another release"), "{what}: {:?}", opened.unreadable());
    }
}

#[test]
fn a_file_with_its_names_rewritten_still_fails_authenticated_decryption() {
    let dir = scratch_dir("secrets-relabelled");
    let ours = store(&dir);
    ours.replace(bundle(1, "FAKE-sealed-value")).unwrap();
    let path = file_for(&dir, &ours.release());
    let other = Release { launch_digest: RELEASE, config_sha256: [0xc2; 32] };
    let relabelled = std::fs::read_to_string(&path).unwrap().replace(&hex::encode(CONFIG), &hex::encode([0xc2; 32]));
    std::fs::write(file_for(&dir, &other), relabelled).unwrap();
    let opened = SecretStore::open(&dir, sealer(RELEASE), [0xc2; 32]).unwrap();
    assert_eq!(opened.current(), None);
    assert!(opened.unreadable().unwrap().contains("does not open"), "{:?}", opened.unreadable());
}

#[test]
fn an_altered_or_foreign_file_opens_empty_and_a_delivery_replaces_it() {
    for (what, damage) in [
        ("not JSON", Box::new(|_: &str| "not a sealed file".to_owned()) as Box<dyn Fn(&str) -> String>),
        (
            "a flipped ciphertext",
            Box::new(|file: &str| {
                let mut sealed: serde_json::Value = serde_json::from_str(file).unwrap();
                let ciphertext = sealed["ciphertext"].as_str().unwrap();
                let flipped = if ciphertext.starts_with('0') { "1" } else { "0" };
                sealed["ciphertext"] = format!("{flipped}{}", &ciphertext[1..]).into();
                sealed.to_string()
            }),
        ),
        ("another format", Box::new(|file: &str| file.replace("\"format\":1", "\"format\":2"))),
        (
            "a short nonce",
            Box::new(|file: &str| {
                let mut sealed: serde_json::Value = serde_json::from_str(file).unwrap();
                sealed["nonce"] = "00".into();
                sealed.to_string()
            }),
        ),
    ] {
        let dir = scratch_dir(&format!("secrets-damaged-{}", what.replace(' ', "-")));
        let ours = store(&dir);
        ours.replace(bundle(5, "FAKE-sealed-value")).unwrap();
        let path = file_for(&dir, &ours.release());
        let file = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, damage(&file)).unwrap();

        let opened = store(&dir);
        assert_eq!(opened.current(), None, "{what}");
        let reason = opened.unreadable().unwrap_or_else(|| panic!("{what}: no reason given"));
        assert!(!reason.contains("FAKE-sealed-value"), "{what}: {reason}");

        // The file it could not open sets no floor: the operator's next bundle replaces it.
        assert_eq!(opened.replace(bundle(1, "FAKE-new-value")).unwrap(), Replaced::Sealed, "{what}");
        assert_eq!(opened.unreadable(), None, "{what}");
        assert_eq!(store(&dir).version(), Some(1), "{what}");
    }
}

#[test]
fn only_a_higher_version_replaces_what_is_sealed_and_a_retry_changes_nothing() {
    let dir = scratch_dir("secrets-versions");
    let ours = store(&dir);
    ours.replace(bundle(3, "FAKE-three")).unwrap();

    let err = ours.replace(bundle(2, "FAKE-two")).unwrap_err().to_string();
    assert!(err.contains("older than the sealed version 3"), "{err}");
    let err = ours.replace(bundle(3, "FAKE-other")).unwrap_err().to_string();
    assert!(err.contains("already sealed with other secrets") && !err.contains("FAKE"), "{err}");
    assert_eq!(ours.replace(bundle(3, "FAKE-three")).unwrap(), Replaced::AlreadySealed);
    assert_eq!(store(&dir).current(), Some(bundle(3, "FAKE-three")), "a refused bundle reached the file");

    assert_eq!(ours.replace(bundle(4, "FAKE-four")).unwrap(), Replaced::Sealed);
    assert_eq!(store(&dir).version(), Some(4));
}

// A rotated operator redelivering what is sealed: its signature is what an upgrade to a config naming
// its key will check, so it replaces the old one with no change of version.
#[test]
fn a_retry_under_a_new_operator_key_keeps_that_keys_signature() {
    let dir = scratch_dir("secrets-new-signer");
    let ours = store(&dir);
    ours.replace(bundle(3, "FAKE-three")).unwrap();

    let next = SigningKey::from_slice(&[0x55; 32]).unwrap();
    let resigned = bundle(3, "FAKE-three").bundle.sign(DOMAIN, &next);
    assert_eq!(ours.replace(resigned.clone()).unwrap(), Replaced::AlreadySealed);
    let reopened = store(&dir);
    assert_eq!(reopened.current(), Some(resigned.clone()));
    assert_eq!(reopened.current().unwrap().signer(DOMAIN).unwrap(), engine_core::eip712::address(next.verifying_key()));
    // Its own retry changes nothing further.
    assert_eq!(ours.replace(resigned).unwrap(), Replaced::AlreadySealed);
}

// What a host gains by restoring an older copy: the older bundle, and nothing else. The status the
// operator reads shows it, and a higher version repairs it.
#[test]
fn restoring_an_older_copy_gives_the_older_bundle_and_a_new_delivery_repairs_it() {
    let dir = scratch_dir("secrets-rollback");
    let ours = store(&dir);
    ours.replace(bundle(1, "FAKE-old")).unwrap();
    let path = file_for(&dir, &ours.release());
    let older = std::fs::read(&path).unwrap();
    ours.replace(bundle(2, "FAKE-rotated")).unwrap();

    std::fs::write(&path, older).unwrap();
    let restored = store(&dir);
    assert_eq!(restored.version(), Some(1), "the store reports the version it opened");
    assert_eq!(restored.replace(bundle(2, "FAKE-rotated")).unwrap(), Replaced::Sealed);
    assert_eq!(store(&dir).version(), Some(2));
}

#[test]
fn a_crash_before_the_rename_leaves_the_file_it_had() {
    let dir = scratch_dir("secrets-crash");
    let ours = store(&dir);
    ours.replace(bundle(1, "FAKE-one")).unwrap();
    let path = file_for(&dir, &ours.release());
    std::fs::write(path.with_extension("partial"), b"half a write").unwrap();
    assert_eq!(store(&dir).version(), Some(1));
}

// The keys and the secrets share the platform's key: neither file opens as the other.
#[test]
fn a_secrets_file_does_not_open_as_keys_nor_keys_as_secrets() {
    let dir = scratch_dir("secrets-vs-keys");
    let ours = store(&dir);
    ours.replace(bundle(1, "FAKE-sealed-value")).unwrap();
    let release = ours.release();
    let keys_sealer = MockSealer { launch_digest: RELEASE };
    KeyStore::create(&dir, &keys_sealer, CONFIG, 1, KeySet::generate().unwrap()).unwrap();
    let keys_file = crate::tee::sealing::file_for(&dir, &release, 1);
    let secrets_file = file_for(&dir, &release);

    // Each file, with its fields made to match the other's layout, under the other's name.
    let mut as_keys: serde_json::Value = serde_json::from_slice(&std::fs::read(&secrets_file).unwrap()).unwrap();
    as_keys["generation"] = 1.into();
    std::fs::write(&keys_file, as_keys.to_string()).unwrap();
    assert!(KeyStore::open(&dir, &keys_sealer, CONFIG, 1).is_err(), "secrets opened as keys");

    let key_store_dir = scratch_dir("secrets-vs-keys-b");
    KeyStore::create(&key_store_dir, &keys_sealer, CONFIG, 1, KeySet::generate().unwrap()).unwrap();
    let mut real_keys: serde_json::Value =
        serde_json::from_slice(&std::fs::read(crate::tee::sealing::file_for(&key_store_dir, &release, 1)).unwrap())
            .unwrap();
    real_keys.as_object_mut().unwrap().remove("generation");
    std::fs::write(&secrets_file, real_keys.to_string()).unwrap();
    let opened = store(&dir);
    assert_eq!(opened.current(), None, "keys opened as secrets");
    assert!(opened.unreadable().unwrap().contains("does not open"), "{:?}", opened.unreadable());
}

// What a start runs is chosen, and what it says about that is said, under the lock a delivery seals
// under, so a delivery's warning about a newer bundle cannot come first and be ended by it.
#[test]
fn a_delivery_waits_for_a_start_to_finish_choosing_what_it_runs() {
    let store = std::sync::Arc::new(store(&scratch_dir("secrets-choosing")));
    store.replace(bundle(1, "FAKE-a")).unwrap();
    let said = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut delivery = None;
    let chosen = store.current_if_complete(
        |_| true,
        |version| {
            assert_eq!(version, Some(1));
            let (sealing, waited) = (store.clone(), said.clone());
            delivery = Some(std::thread::spawn(move || {
                sealing.replace(bundle(2, "FAKE-b")).unwrap();
                assert!(waited.load(std::sync::atomic::Ordering::SeqCst), "it sealed before the start had chosen");
            }));
            std::thread::sleep(std::time::Duration::from_millis(100));
            said.store(true, std::sync::atomic::Ordering::SeqCst);
        },
    );
    assert_eq!(chosen.unwrap().unwrap().bundle.version, 1);
    delivery.unwrap().join().unwrap();
}

// A start's completeness check and its choice of what it runs take one lock, so a delivery sealing
// a partial bundle in between cannot leave the start running it: the choice fails and the start
// keeps holding, and its next choice runs the repaired bundle.
#[test]
fn a_partial_delivery_cannot_replace_a_complete_bundle_between_the_check_and_the_choice() {
    let store = std::sync::Arc::new(store(&scratch_dir("secrets-partial-choice")));
    let beta = SecretName::try_from("beta_key".to_owned()).unwrap();
    let two_secrets = |version: u64| {
        let mut secrets = bundle(version, "FAKE-alpha").bundle.secrets;
        secrets.insert(beta.clone(), SecretValue::test("FAKE-beta"));
        Bundle { version, secrets }.sign(DOMAIN, &SigningKey::from_slice(&[0x42; 32]).unwrap())
    };
    let needs_beta = |held: Option<&SignedBundle>| held.is_some_and(|signed| signed.bundle.secrets.contains_key(&beta));
    store.replace(two_secrets(1)).unwrap();

    // While the start holds the lock choosing, a delivery seals a higher version without the
    // required secret; the start still runs the complete version it checked.
    let mut delivery = None;
    let chosen = store.current_if_complete(needs_beta, |version| {
        assert_eq!(version, Some(1));
        let sealing = store.clone();
        delivery = Some(std::thread::spawn(move || sealing.replace(bundle(2, "FAKE-alpha")).unwrap()));
        std::thread::sleep(std::time::Duration::from_millis(100));
    });
    delivery.unwrap().join().unwrap();
    assert_eq!(store.version(), Some(2));
    let chosen = chosen.expect("the complete bundle was chosen").expect("a bundle");
    assert_eq!(chosen.bundle.version, 1);
    assert!(chosen.bundle.secrets.contains_key(&beta));

    // The bundle now sealed lacks the required secret, so the same choice keeps the start holding.
    assert!(store.current_if_complete(needs_beta, |_| {}).is_none());

    // The delivery that completes the bundle ends the hold: the next choice runs it.
    store.replace(two_secrets(3)).unwrap();
    let chosen = store.current_if_complete(needs_beta, |_| {}).expect("the repaired bundle was chosen");
    assert_eq!(chosen.expect("a bundle").bundle.version, 3);
}
