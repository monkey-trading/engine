//! A list of release manifests: the quote verifies against the first release that admits it and
//! names that one, and a refusal says why each release did not.

use super::{now, verify};
use crate::ReleaseManifest;
use crate::fixtures::*;

#[test]
fn the_release_that_admits_the_quote_is_named_wherever_it_sits_in_the_list() {
    let (chain, _enclave, attestation) = fixtures();
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let current = release();
    let previous = previous_release();

    for releases in [vec![previous.clone(), current.clone()], vec![current.clone(), previous.clone()]] {
        let verified = verify(&attestation, &releases, &collateral, now()).expect("one release admits the quote");
        assert_eq!(verified.release_name, "test-release-v1");
        assert_eq!(verified.config_name, "node.toml");
    }

    // Two releases that both admit it: the first listed is named.
    let renamed = ReleaseManifest { name: "test-release-v1-copy".into(), ..current.clone() };
    let verified = verify(&attestation, &[renamed, current], &collateral, now()).unwrap();
    assert_eq!(verified.release_name, "test-release-v1-copy");
}

#[test]
fn one_release_refuses_with_its_own_message_and_several_name_each_refusal() {
    let (chain, _enclave, attestation) = fixtures();
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let measurement = "the quote's launch measurement matches no release launch digest";

    let err = verify(&attestation, &[previous_release()], &collateral, now()).unwrap_err();
    assert_eq!(format!("{err:#}"), measurement);

    let mut unlisted = release();
    unlisted.name = "test-release-v2".into();
    unlisted.config_hashes = [("other.toml".to_owned(), [0xc1; 32])].into();
    let err = verify(&attestation, &[previous_release(), unlisted], &collateral, now()).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        format!(
            "the quote matches none of the 2 release manifests: test-release-v0: {measurement}; \
             test-release-v2: the bundle's configuration hash matches no release configuration"
        )
    );

    let err = verify(&attestation, &[], &collateral, now()).unwrap_err();
    assert_eq!(err.to_string(), "no release manifest to verify against");
}

#[test]
fn each_release_check_refuses_a_quote_outside_it() {
    let (chain, _enclave, attestation) = fixtures();
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    for (what, edit, message) in [
        (
            "measurement",
            (|r: &mut ReleaseManifest| r.launch_digest = [0x1e; 48]) as fn(&mut ReleaseManifest),
            "the quote's launch measurement matches no release launch digest",
        ),
        ("policy", |r| r.policy = POLICY + 1, "the quote's guest policy 0x0000000000030000 differs"),
        ("TCB", |r| r.minimum_tcb.microcode = 206, "the quote's reported TCB"),
        ("config", |r| r.config_hashes.clear(), "the bundle's configuration hash matches no release configuration"),
    ] {
        let mut release = release();
        edit(&mut release);
        let err = verify(&attestation, &[release], &collateral, now()).unwrap_err();
        assert!(err.to_string().starts_with(message), "{what}: {err:#}");
    }
}

// ── Helpers ──

/// A release the fixture quote does not run: another launch digest.
fn previous_release() -> ReleaseManifest {
    ReleaseManifest { name: "test-release-v0".into(), launch_digest: [0x0d; 48], ..release() }
}
