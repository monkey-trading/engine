//! `sign-admission` writes the admission a successor's node reads, signed by the key given, for the
//! config's custody chain and vault, and refuses a release or key it cannot name.

use super::*;

use crate::handover::admission::ADMISSION_FILE;
use crate::test_support::{dev_config, scratch_dir};

/// The arguments naming `release` and `key`, writing into `dir`'s admission file.
fn args(release: &str, key: &str, dir: &std::path::Path) -> SignArgs {
    SignArgs { release: release.into(), key: key.into(), out: dir.join(ADMISSION_FILE) }
}

#[test]
fn the_admission_names_the_key_and_release_signed_by_the_key_given_for_the_configs_vault() {
    let dir = scratch_dir("admin-sign-admission");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dev_config();
    let signer = SigningKey::from_slice(&[0x66; 32]).unwrap();
    let standby = AccountId([0x5b; 20]);
    let release = format!("0x{}", hex::encode([0x2e; 32]));
    sign(&cfg, &signer, &args(&release, &standby.to_string(), &dir)).unwrap();
    let admission = Admission::read(&dir).unwrap().expect("the admission was written");
    assert!(admission.names(standby, [0x2e; 32]), "{admission:?}");
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    assert_eq!(admission.signer(domain), Some(eip712::address(signer.verifying_key())));
    let other_chain = CheckpointDomain { chain_id: cfg.custody.chain_id + 1, ..domain };
    assert_ne!(admission.signer(other_chain), Some(eip712::address(signer.verifying_key())), "another chain");
}

#[test]
fn a_release_or_key_it_cannot_name_writes_nothing() {
    let dir = scratch_dir("admin-sign-admission-refused");
    std::fs::create_dir_all(&dir).unwrap();
    let signer = SigningKey::from_slice(&[0x66; 32]).unwrap();
    let release = hex::encode([0x2e; 32]);
    for (what, args, refusal) in [
        ("a short release", args(&hex::encode([0x2e; 31]), &AccountId([1; 20]).to_string(), &dir), "not the 32"),
        ("a release not in hex", args("0xnothex", &AccountId([1; 20]).to_string(), &dir), "not hex"),
        ("a key that is no address", args(&release, "0x1234", &dir), "not an address"),
    ] {
        let err = sign(&dev_config(), &signer, &args).expect_err(what);
        assert!(format!("{err:#}").contains(refusal), "{what}: {err:#}");
        assert!(!dir.join(ADMISSION_FILE).exists(), "{what}: an admission was written");
    }
}
