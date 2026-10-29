//! Startup constraints on independent custody providers.

use std::path::Path;

use crate::config::NodeConfig;

#[test]
fn custody_requires_three_distinct_named_endpoints_even_with_the_mock_tee() {
    let text = std::fs::read_to_string(Path::new("../../config/anvil.toml")).unwrap();
    let mut value: toml::Value = toml::from_str(&text).unwrap();
    let providers = value["custody"]["evm"]["providers"].as_array().unwrap().clone();
    for count in 0..3 {
        value["custody"]["evm"]["providers"] = toml::Value::Array(providers[..count].to_vec());
        let cfg: NodeConfig = value.clone().try_into().unwrap();
        assert!(cfg.validate().unwrap_err().to_string().contains("at least three"));
    }
    for (field, replacement) in [("name", "local-a"), ("rpc_url", "http://127.0.0.1:8545"), ("name", " ")] {
        value["custody"]["evm"]["providers"] = toml::Value::Array(providers.clone());
        value["custody"]["evm"]["providers"][1][field] = replacement.into();
        let cfg: NodeConfig = value.clone().try_into().unwrap();
        assert!(cfg.validate().unwrap_err().to_string().contains("non-empty unique names and distinct URLs"));
    }
}

#[test]
fn every_provider_is_checked_and_unknown_keys_and_legacy_single_rpc_are_refused() {
    let text = std::fs::read_to_string(Path::new("../../config/anvil.toml")).unwrap();
    for url in ["http://remote.example/key", "ftp://127.0.0.1/key", "not a url"] {
        let text = text.replace("http://127.0.0.1:8547", url);
        let cfg: NodeConfig = toml::from_str(&text).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("providers.rpc_url"), "{err}");
        assert!(!err.contains(url), "provider credentials must not reach errors");
    }
    let unknown = text.replace("name = \"local-c\"", "name = \"local-c\", typo = true");
    assert!(toml::from_str::<NodeConfig>(&unknown).is_err());
    let legacy = text.replace("providers = [", "rpc_url = \"https://rpc.example/key\"\nproviders = [");
    assert!(toml::from_str::<NodeConfig>(&legacy).is_err());
}

#[test]
fn production_requires_latest_credit_and_ethereum_finality_for_release() {
    use crate::config::{FinalityTag, TeeProvider};
    let text = std::fs::read_to_string(Path::new("../../config/anvil.toml")).unwrap();
    for tee in [TeeProvider::Mock, TeeProvider::ConfigfsTsm] {
        for credit in [FinalityTag::Latest, FinalityTag::Safe, FinalityTag::Finalized] {
            for release in [FinalityTag::Latest, FinalityTag::Safe, FinalityTag::Finalized] {
                let mut cfg: NodeConfig = toml::from_str(&text).unwrap();
                cfg.tee.provider = tee;
                let evm = cfg.custody.evm.as_mut().unwrap();
                evm.finality_tag = credit;
                evm.withdrawal_finality_tag = release;
                let expected =
                    tee == TeeProvider::Mock || (credit == FinalityTag::Latest && release == FinalityTag::Finalized);
                assert_eq!(cfg.validate_custody().is_ok(), expected, "{tee:?} {credit:?} {release:?}");
            }
        }
    }
}

#[test]
fn the_operator_registry_must_be_a_contract_of_its_own() {
    let text = std::fs::read_to_string(Path::new("../../config/anvil.toml")).unwrap();
    let staking = "0x00000000000000000000000000000000000000a5";
    let section = |registry: &str| {
        format!(
            "{text}\n[custody.evm.staking]\ncontract = '{staking}'\ncursor_path = 'staking.json'\n[custody.evm.staking.registry]\ncontract = '{registry}'\n"
        )
    };
    let cfg: NodeConfig = toml::from_str(&section("0x00000000000000000000000000000000000000a6")).unwrap();
    cfg.validate_custody().unwrap();
    let usdc = cfg.custody.evm.as_ref().unwrap().usdc.to_string();
    for other in [staking, cfg.custody.vault.as_str(), usdc.as_str()] {
        let cfg: NodeConfig = toml::from_str(&section(other)).unwrap();
        let err = cfg.validate_custody().unwrap_err().to_string();
        assert!(err.contains("custody.evm.staking.registry.contract must be the operator registry"), "{err}");
    }
}
