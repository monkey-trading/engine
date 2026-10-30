use super::*;
use crate::test_support::scratch_dir;

const RELEASE: [u8; 48] = [0x11; 48];
const UPGRADE: [u8; 48] = [0x22; 48];
const CONFIG: [u8; 32] = [0xc1; 32];
const OTHER_CONFIG: [u8; 32] = [0xc2; 32];
/// The vault's generation of the release the tests seal in.
const GEN: u64 = 3;

#[test]
fn a_restart_of_the_same_release_opens_the_keys_it_sealed() {
    let dir = scratch_dir("sealing-restart");
    let sealer = MockSealer { launch_digest: RELEASE };
    assert!(KeyStore::open(&dir, &sealer, CONFIG, GEN).unwrap().is_none(), "an empty directory holds keys");
    let keys = KeySet::generate().unwrap();
    KeyStore::create(&dir, &sealer, CONFIG, GEN, keys.clone()).unwrap();

    let reopened = KeyStore::open(&dir, &sealer, CONFIG, GEN).unwrap().expect("the release's keys");
    assert_eq!(reopened.keys(), keys);
    assert_eq!(reopened.release(), Release { launch_digest: RELEASE, config_sha256: CONFIG });
    assert_eq!(reopened.generation(), GEN);
}

#[test]
fn a_release_named_again_finds_no_keys_and_cannot_open_its_older_copy() {
    let dir = scratch_dir("sealing-other-generation");
    let sealer = MockSealer { launch_digest: RELEASE };
    let keys = KeySet::generate().unwrap();
    KeyStore::create(&dir, &sealer, CONFIG, GEN, keys.clone()).unwrap();
    assert!(KeyStore::open(&dir, &sealer, CONFIG, GEN + 1).unwrap().is_none(), "a later generation found keys");

    // The host gives the older copy the later generation's name, then rewrites its header to match.
    let release = Release { launch_digest: RELEASE, config_sha256: CONFIG };
    let mut sealed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(file_for(&dir, &release, GEN)).unwrap()).unwrap();
    std::fs::write(file_for(&dir, &release, GEN + 1), serde_json::to_vec(&sealed).unwrap()).unwrap();
    let err = KeyStore::open(&dir, &sealer, CONFIG, GEN + 1).err().expect("the renamed copy opened");
    assert!(format!("{err:#}").contains("sealed by another release or generation"), "{err:#}");
    sealed["generation"] = (GEN + 1).into();
    std::fs::write(file_for(&dir, &release, GEN + 1), serde_json::to_vec(&sealed).unwrap()).unwrap();
    let err = KeyStore::open(&dir, &sealer, CONFIG, GEN + 1).err().expect("the rewritten copy opened");
    assert!(format!("{err:#}").contains("does not open under this release's sealing key"), "{err:#}");

    // Keys handed over afresh are sealed beside it, and the older copy is left as it was.
    std::fs::remove_file(file_for(&dir, &release, GEN + 1)).unwrap();
    KeyStore::create(&dir, &sealer, CONFIG, GEN + 1, keys.clone()).unwrap();
    assert_eq!(KeyStore::open(&dir, &sealer, CONFIG, GEN + 1).unwrap().unwrap().keys(), keys);
    assert_eq!(KeyStore::open(&dir, &sealer, CONFIG, GEN).unwrap().unwrap().keys(), keys);
}

#[test]
fn the_same_code_under_another_config_finds_no_keys_and_cannot_open_the_file() {
    let dir = scratch_dir("sealing-other-config");
    let sealer = MockSealer { launch_digest: RELEASE };
    KeyStore::create(&dir, &sealer, CONFIG, GEN, KeySet::generate().unwrap()).unwrap();
    assert!(KeyStore::open(&dir, &sealer, OTHER_CONFIG, GEN).unwrap().is_none(), "another config found keys");

    // The host renames the file to the other config's release and rewrites its header to match.
    let other = Release { launch_digest: RELEASE, config_sha256: OTHER_CONFIG };
    let original = file_for(&dir, &Release { launch_digest: RELEASE, config_sha256: CONFIG }, GEN);
    let mut sealed: serde_json::Value = serde_json::from_slice(&std::fs::read(original).unwrap()).unwrap();
    std::fs::write(file_for(&dir, &other, GEN), serde_json::to_vec(&sealed).unwrap()).unwrap();
    let err = KeyStore::open(&dir, &sealer, OTHER_CONFIG, GEN).err().expect("the renamed file opened");
    assert!(format!("{err:#}").contains("sealed by another release"), "{err:#}");
    sealed["config_sha256"] = hex::encode(OTHER_CONFIG).into();
    std::fs::write(file_for(&dir, &other, GEN), serde_json::to_vec(&sealed).unwrap()).unwrap();
    let err = KeyStore::open(&dir, &sealer, OTHER_CONFIG, GEN).err().expect("the rewritten file opened");
    assert!(format!("{err:#}").contains("does not open under this release's sealing key"), "{err:#}");
}

#[test]
fn another_measurement_cannot_open_the_file_even_under_its_own_name() {
    let dir = scratch_dir("sealing-other-release");
    KeyStore::create(&dir, &MockSealer { launch_digest: RELEASE }, CONFIG, GEN, KeySet::generate().unwrap()).unwrap();
    let upgrade = MockSealer { launch_digest: UPGRADE };
    assert!(KeyStore::open(&dir, &upgrade, CONFIG, GEN).unwrap().is_none(), "the upgrade found keys sealed for it");

    let theirs = Release { launch_digest: UPGRADE, config_sha256: CONFIG };
    std::fs::copy(
        file_for(&dir, &Release { launch_digest: RELEASE, config_sha256: CONFIG }, GEN),
        file_for(&dir, &theirs, GEN),
    )
    .unwrap();
    let err = KeyStore::open(&dir, &upgrade, CONFIG, GEN).err().expect("the renamed file opened");
    assert!(format!("{err:#}").contains("sealed by another release"), "{err:#}");

    let path = file_for(&dir, &theirs, GEN);
    let mut sealed: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    sealed["launch_digest"] = hex::encode(UPGRADE).into();
    std::fs::write(&path, serde_json::to_vec(&sealed).unwrap()).unwrap();
    let err = KeyStore::open(&dir, &upgrade, CONFIG, GEN).err().expect("the rewritten file opened");
    assert!(format!("{err:#}").contains("does not open under this release's sealing key"), "{err:#}");
}

#[test]
fn any_altered_field_of_the_file_refuses_to_open() {
    let dir = scratch_dir("sealing-altered");
    let sealer = MockSealer { launch_digest: RELEASE };
    KeyStore::create(&dir, &sealer, CONFIG, GEN, KeySet::generate().unwrap()).unwrap();
    let path = file_for(&dir, &Release { launch_digest: RELEASE, config_sha256: CONFIG }, GEN);
    let original = std::fs::read(&path).unwrap();
    let flip_hex = |text: &str| {
        let mut bytes = hex::decode(text).unwrap();
        bytes[0] ^= 1;
        serde_json::Value::from(hex::encode(bytes))
    };
    for field in ["ciphertext", "nonce", "tcb", "generation", "format"] {
        let mut sealed: serde_json::Value = serde_json::from_slice(&original).unwrap();
        sealed[field] = match field {
            "tcb" | "generation" => (sealed[field].as_u64().unwrap() + 1).into(),
            "format" => 2.into(),
            _ => flip_hex(sealed[field].as_str().unwrap()),
        };
        std::fs::write(&path, serde_json::to_vec(&sealed).unwrap()).unwrap();
        assert!(KeyStore::open(&dir, &sealer, CONFIG, GEN).is_err(), "a file with an altered {field} opened");
    }
    std::fs::write(&path, b"{\"format\":1}").unwrap();
    assert!(KeyStore::open(&dir, &sealer, CONFIG, GEN).is_err(), "a truncated file opened");
}

#[test]
fn an_update_reseals_and_an_older_copy_still_opens_to_the_older_keys() {
    let dir = scratch_dir("sealing-update");
    let sealer = MockSealer { launch_digest: RELEASE };
    let keys = KeySet::generate().unwrap();
    let store = KeyStore::create(&dir, &sealer, CONFIG, GEN, keys.clone()).unwrap();
    let path = file_for(&dir, &store.release(), GEN);
    let older = std::fs::read(&path).unwrap();

    store.update(|keys| keys.set_certificate("https://ca.test/dir", &[vec![9u8; 4].into()])).unwrap();
    assert!(store.keys().certificate.is_some(), "the store kept the old keys");
    let reopened = KeyStore::open(&dir, &sealer, CONFIG, GEN).unwrap().unwrap();
    assert_eq!(reopened.keys(), store.keys());
    assert!(!path.with_extension("partial").exists(), "a partial file was left");

    // A host that restores the older copy restores the same signing key, without the certificate.
    std::fs::write(&path, older).unwrap();
    assert_eq!(KeyStore::open(&dir, &sealer, CONFIG, GEN).unwrap().unwrap().keys(), keys);
}

#[test]
fn a_release_never_seals_over_its_existing_file() {
    let dir = scratch_dir("sealing-create-twice");
    let sealer = MockSealer { launch_digest: RELEASE };
    let first = KeySet::generate().unwrap();
    KeyStore::create(&dir, &sealer, CONFIG, GEN, first.clone()).unwrap();
    assert!(
        KeyStore::create(&dir, &sealer, CONFIG, GEN, KeySet::generate().unwrap()).is_err(),
        "a second create replaced keys"
    );
    assert_eq!(KeyStore::open(&dir, &sealer, CONFIG, GEN).unwrap().unwrap().keys(), first);
}

#[test]
fn a_failed_update_keeps_the_file_and_the_keys() {
    let dir = scratch_dir("sealing-failed-update");
    let sealer = MockSealer { launch_digest: RELEASE };
    let store = KeyStore::create(&dir, &sealer, CONFIG, GEN, KeySet::generate().unwrap()).unwrap();
    let before = store.keys();
    // A directory where the partial file goes makes the write fail.
    std::fs::create_dir(file_for(&dir, &store.release(), GEN).with_extension("partial")).unwrap();
    assert!(store.update(|keys| keys.set_certificate("https://ca.test/dir", &[vec![1u8].into()])).is_err());
    assert_eq!(store.keys(), before, "a failed update changed the keys in memory");
    assert_eq!(KeyStore::open(&dir, &sealer, CONFIG, GEN).unwrap().unwrap().keys(), before);
}

#[test]
fn the_release_id_is_the_vaults_and_the_stand_in_key_depends_on_the_measurement_and_tcb() {
    let release = Release { launch_digest: RELEASE, config_sha256: CONFIG };
    let expected: [u8; 32] = Keccak256::digest([RELEASE.as_slice(), &CONFIG].concat()).into();
    assert_eq!(release.id(), expected, "keccak256(launch digest || config hash), as the vault keys it");
    assert_ne!(release.id(), Release { config_sha256: OTHER_CONFIG, ..release }.id());
    assert_ne!(release.id(), Release { launch_digest: UPGRADE, ..release }.id());

    let (mock, upgrade) = (MockSealer { launch_digest: RELEASE }, MockSealer { launch_digest: UPGRADE });
    assert_ne!(mock.sealing_key(0).unwrap(), upgrade.sealing_key(0).unwrap());
    assert_ne!(mock.sealing_key(0).unwrap(), mock.sealing_key(1).unwrap());
}
