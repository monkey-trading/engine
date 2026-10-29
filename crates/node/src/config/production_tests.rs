//! The config file's hash, which the attestation binds, and a production build's refusal of mock
//! providers.

use super::*;

use crate::test_support::DEV_CONFIG as DEV;

#[test]
fn the_config_hash_is_the_sha256_of_the_file_bytes() {
    let cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    // The value `sha256sum config/dev.toml` prints, so anyone can check a published config.
    let expected: [u8; 32] = Sha256::digest(std::fs::read(DEV).unwrap()).into();
    assert_eq!(cfg.file_sha256, expected);
    // Only `load` reads a file; the hash is not a config key, even holding 32 bytes.
    let key = format!("file_sha256 = [{}]\n[node]", ["0"; 32].join(", "));
    let text = std::fs::read_to_string(DEV).unwrap().replacen("[node]", &key, 1);
    let err = toml::from_str::<NodeConfig>(&text).unwrap_err().to_string();
    assert!(err.contains("unknown field `file_sha256`"), "{err}");
}

#[test]
fn a_production_build_refuses_each_mock_provider() {
    let mut cfg: NodeConfig = toml::from_str(&std::fs::read_to_string(DEV).unwrap()).unwrap();
    let err = cfg.refuse_mocks(true).unwrap_err().to_string();
    for key in ["tee.provider", "custody.provider", "feeds.provider"] {
        assert!(err.contains(key), "{key} missing from: {err}");
    }

    // With every provider real, nothing is refused; with one mock, only that one is named.
    cfg.tee.provider = TeeProvider::ConfigfsTsm;
    cfg.custody.provider = CustodyProvider::Evm;
    cfg.feeds.provider = FeedProvider::Venues;
    cfg.refuse_mocks(true).unwrap();
    for (key, set_mock) in [
        ("tee.provider", (|c: &mut NodeConfig| c.tee.provider = TeeProvider::Mock) as fn(&mut NodeConfig)),
        ("custody.provider", |c| c.custody.provider = CustodyProvider::Mock),
        ("feeds.provider", |c| c.feeds.provider = FeedProvider::Mock),
    ] {
        let mut one: NodeConfig = toml::from_str(&std::fs::read_to_string(DEV).unwrap()).unwrap();
        one.tee.provider = TeeProvider::ConfigfsTsm;
        one.custody.provider = CustodyProvider::Evm;
        one.feeds.provider = FeedProvider::Venues;
        set_mock(&mut one);
        let err = one.refuse_mocks(true).unwrap_err().to_string();
        assert!(err.contains(&format!("{key} set to")), "{err}");
        assert_eq!(err.matches(".provider").count(), 1, "{err}");
    }
}

#[test]
fn a_dev_build_accepts_mock_providers() {
    let cfg: NodeConfig = toml::from_str(&std::fs::read_to_string(DEV).unwrap()).unwrap();
    cfg.refuse_mocks(false).unwrap();
}

// The production lane runs the tests named `production_build`; a production build refuses the
// dev config every other test loads.
#[cfg(feature = "production")]
#[test]
fn a_production_build_refuses_the_dev_config_at_load() {
    let err = NodeConfig::load(Path::new(DEV)).unwrap_err();
    assert!(format!("{err:#}").contains("a production build refuses mock providers"), "{err:#}");
}

// A production build refuses an `http` origin at load, before any other check can pass the file.
#[cfg(feature = "production")]
#[test]
fn a_production_build_refuses_an_http_origin_at_load() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    assert!(dev.contains("allowed_origins = [\"http://localhost:3000\"]"));
    let dir = crate::test_support::scratch_dir("production-origin");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("node.toml");
    std::fs::write(&path, dev).unwrap();
    let err = NodeConfig::load(&path).unwrap_err();
    assert!(format!("{err:#}").contains("allows only https origins"), "{err:#}");
}
