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
fn a_production_build_refuses_fresh_account_budgets_above_its_values() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let load = |text: &str| toml::from_str::<NodeConfig>(text).unwrap();
    // The dev config's rates are refused, naming the budget; a dev build accepts them.
    let cfg = load(&dev);
    cfg.api.refuse_dev_fresh_budgets(false).unwrap();
    let err = cfg.api.refuse_dev_fresh_budgets(true).unwrap_err().to_string();
    assert!(err.contains("api.fresh_accounts.shared at most 60 a minute with a burst of 120"), "{err}");

    // The production values pass, and any one of rate or burst above them is refused.
    let at_ceiling = dev
        .replace("shared = { per_minute = 3000, burst = 100 }", "shared = { per_minute = 60, burst = 120 }")
        .replace("per_client = { per_minute = 600, burst = 20 }", "per_client = { per_minute = 12, burst = 6 }");
    let cfg = load(&at_ceiling);
    cfg.validate().unwrap();
    cfg.api.refuse_dev_fresh_budgets(true).unwrap();
    for (from, to, key) in [
        ("shared = { per_minute = 60,", "shared = { per_minute = 61,", "shared"),
        ("burst = 120 }", "burst = 121 }", "shared"),
        ("per_client = { per_minute = 12,", "per_client = { per_minute = 13,", "per_client"),
        ("burst = 6 }", "burst = 7 }", "per_client"),
    ] {
        let err = load(&at_ceiling.replace(from, to)).api.refuse_dev_fresh_budgets(true).unwrap_err().to_string();
        assert!(err.contains(&format!("api.fresh_accounts.{key} at most")), "{to}: {err}");
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
    // Without the dev origin, which is refused first as `http`.
    let text: String = std::fs::read_to_string(DEV)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("allowed_origins"))
        .map(|l| format!("{l}\n"))
        .collect();
    let dir = crate::test_support::scratch_dir("production-dev-config");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("node.toml");
    std::fs::write(&path, text).unwrap();
    let err = NodeConfig::load(&path).unwrap_err();
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
