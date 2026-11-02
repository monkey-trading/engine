//! What a bundle's signature covers, and a bundle file that does not parse.

use super::*;

use crate::secrets::tests::bundle;
use crate::test_support::scratch_dir;

#[test]
fn a_bundles_digest_covers_its_deployment_version_and_every_name_and_value() {
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0x0a; 20]) };
    let digest = bundle(1, &[("a", "x"), ("b", "y")]).digest(domain);
    for (what, other) in [
        ("another chain", bundle(1, &[("a", "x"), ("b", "y")]).digest(CheckpointDomain { chain_id: 1, ..domain })),
        (
            "another vault",
            bundle(1, &[("a", "x"), ("b", "y")]).digest(CheckpointDomain { vault: AccountId([0x0b; 20]), ..domain }),
        ),
        ("another version", bundle(2, &[("a", "x"), ("b", "y")]).digest(domain)),
        ("another value", bundle(1, &[("a", "z"), ("b", "y")]).digest(domain)),
        ("another name", bundle(1, &[("c", "x"), ("b", "y")]).digest(domain)),
        ("a missing secret", bundle(1, &[("a", "x")]).digest(domain)),
    ] {
        assert_ne!(other, digest, "{what}");
    }
    assert_eq!(bundle(1, &[("b", "y"), ("a", "x")]).digest(domain), digest, "the file's order counts");
    // The lengths keep a name and a value from sliding into each other.
    assert_ne!(
        bundle(1, &[("a", "xb"), ("c", "y")]).digest(domain),
        bundle(1, &[("a", "x"), ("bc", "y")]).digest(domain),
        "a shifted boundary"
    );
}

#[test]
fn a_bundle_file_that_does_not_parse_never_quotes_it() {
    let dir = scratch_dir("secrets-file");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bundle.toml");
    for text in [
        "version = \"FAKE-in-the-wrong-place\"\n[secrets]\n",
        "version = 1\n[secrets]\nkey = 5\n",
        "version = 1\n[secrets]\nBad-Name = \"FAKE-value\"\n",
        "version = 1\nFAKE-value = 1\n[secrets]\n",
        "version = 1\n[secrets\nkey = \"FAKE-value\"\n",
    ] {
        std::fs::write(&path, text).unwrap();
        let err = format!("{:#}", Bundle::from_file(&path).unwrap_err());
        assert!(err.contains("is not a secrets bundle") && !err.contains("FAKE"), "{text}: {err}");
    }
    std::fs::write(&path, "version = 7\n[secrets]\nchain_key = \"FAKE-chain\"\n").unwrap();
    let parsed = Bundle::from_file(&path).unwrap();
    assert_eq!(parsed, bundle(7, &[("chain_key", "FAKE-chain")]));
    assert!(!format!("{parsed:?}").contains("FAKE"));
}
