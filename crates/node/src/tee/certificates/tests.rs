//! The member's sealed certificate on the host's disk: what a restart finds, for what, and what
//! another release, or a host that renames or alters the file, gets.

use super::*;

use crate::tee::sealing::MockSealer;
use crate::test_support::scratch_dir;

const RELEASE: [u8; 48] = [0x11; 48];
const CONFIG: [u8; 32] = [0xc1; 32];
const DOMAIN: &str = "api.engine.test";
const DIRECTORY: &str = "https://ca.test/dir";
/// The SHA-256 of the TLS key the tests' certificates certify, and of another.
const KEY: [u8; 32] = [0x7e; 32];
const OTHER_KEY: [u8; 32] = [0x7f; 32];

#[test]
fn a_certificate_is_found_after_a_restart_only_for_the_domain_tls_key_and_directory_it_was_issued_for() {
    let dir = scratch_dir("certificates-keyed");
    let first = store(&dir, RELEASE, CONFIG).unwrap();
    assert_eq!(first.chain_for(DOMAIN, &KEY, DIRECTORY).unwrap(), None, "a fresh store holds a certificate");
    first.replace(DOMAIN, &KEY, DIRECTORY, &chain()).unwrap();

    let reopened = store(&dir, RELEASE, CONFIG).unwrap();
    assert_eq!(reopened.chain_for(DOMAIN, &KEY, DIRECTORY).unwrap(), Some(chain()));
    for (what, domain, key, directory) in [
        ("another domain", "other.engine.test", KEY, DIRECTORY),
        ("another TLS key", DOMAIN, OTHER_KEY, DIRECTORY),
        ("another directory", DOMAIN, KEY, "https://ca.test/production"),
    ] {
        assert_eq!(reopened.chain_for(domain, &key, directory).unwrap(), None, "{what}");
    }

    // An issue for a new TLS key replaces it.
    reopened.replace(DOMAIN, &OTHER_KEY, DIRECTORY, &chain()[..1]).unwrap();
    let replaced = store(&dir, RELEASE, CONFIG).unwrap();
    assert_eq!(replaced.chain_for(DOMAIN, &KEY, DIRECTORY).unwrap(), None);
    assert_eq!(replaced.chain_for(DOMAIN, &OTHER_KEY, DIRECTORY).unwrap(), Some(chain()[..1].to_vec()));
    // An empty chain is never served.
    replaced.replace(DOMAIN, &KEY, DIRECTORY, &[]).unwrap();
    assert!(replaced.chain_for(DOMAIN, &KEY, DIRECTORY).is_err(), "an empty chain was served");
}

#[test]
fn another_release_finds_nothing_and_a_renamed_or_altered_file_refuses_to_open() {
    let dir = scratch_dir("certificates-refused");
    let ours = store(&dir, RELEASE, CONFIG).unwrap();
    ours.replace(DOMAIN, &KEY, DIRECTORY, &chain()).unwrap();
    let path = file_for(&dir, &ours.release());
    let original = std::fs::read(&path).unwrap();

    // Another release, or this code under another config, finds nothing under its own name, and
    // refuses this file under that name.
    for (what, launch_digest, config) in [("code", [0x22; 48], CONFIG), ("config", RELEASE, [0xc2; 32])] {
        let other = store(&dir, launch_digest, config).unwrap();
        assert_eq!(other.chain_for(DOMAIN, &KEY, DIRECTORY).unwrap(), None, "another {what} found it");
        std::fs::copy(&path, file_for(&dir, &other.release())).unwrap();
        let err = store(&dir, launch_digest, config).err().unwrap_or_else(|| panic!("another {what} opened it"));
        assert!(format!("{err:#}").contains("another release"), "{what}: {err:#}");
        // With its names rewritten to that release's, it still fails authenticated decryption.
        let relabelled = String::from_utf8(original.clone())
            .unwrap()
            .replace(&hex::encode(RELEASE), &hex::encode(launch_digest))
            .replace(&hex::encode(CONFIG), &hex::encode(config));
        std::fs::write(file_for(&dir, &other.release()), relabelled).unwrap();
        let err =
            store(&dir, launch_digest, config).err().unwrap_or_else(|| panic!("relabelled for {what}, it opened"));
        assert!(format!("{err:#}").contains("does not open"), "{what}: {err:#}");
    }

    // Any altered field, a truncated file, or another kind's file under its name refuses.
    let flip_hex = |text: &str| {
        let mut bytes = hex::decode(text).unwrap();
        bytes[0] ^= 1;
        serde_json::Value::from(hex::encode(bytes))
    };
    for field in ["ciphertext", "nonce", "tcb", "format"] {
        let mut sealed: serde_json::Value = serde_json::from_slice(&original).unwrap();
        sealed[field] = match field {
            "tcb" => (sealed[field].as_u64().unwrap() + 1).into(),
            "format" => 2.into(),
            _ => flip_hex(sealed[field].as_str().unwrap()),
        };
        std::fs::write(&path, serde_json::to_vec(&sealed).unwrap()).unwrap();
        assert!(store(&dir, RELEASE, CONFIG).is_err(), "a file with an altered {field} opened");
    }
    std::fs::write(&path, b"{\"format\":1}").unwrap();
    assert!(store(&dir, RELEASE, CONFIG).is_err(), "a truncated file opened");
    let secrets = Kind { tag: b"engine.sealed-secrets.v1", ..CERTIFICATES };
    let sealer = Box::new(MockSealer { launch_digest: RELEASE });
    let certificate = ours.lock().clone().expect("a certificate is stored");
    ReleaseFile::new(&dir, secrets, sealer, CONFIG).unwrap().write(serde_json::to_vec(&certificate).unwrap()).unwrap();
    let err = store(&dir, RELEASE, CONFIG).err().expect("the secrets' kind opened as a certificate");
    assert!(format!("{err:#}").contains("does not open"), "{err:#}");
}

// ── Helpers ──

/// The store `launch_digest` running the config whose SHA-256 is `config` opens in `dir`, under the
/// mock stand-in.
fn store(dir: &Path, launch_digest: [u8; 48], config: [u8; 32]) -> anyhow::Result<CertificateStore> {
    CertificateStore::open(dir, Box::new(MockSealer { launch_digest }), config)
}

/// A chain of two certificates: the bytes are opaque to the store.
fn chain() -> Vec<CertificateDer<'static>> {
    vec![CertificateDer::from(vec![0x5a; 48]), CertificateDer::from(vec![0xa5; 40])]
}
