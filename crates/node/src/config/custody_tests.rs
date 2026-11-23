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
fn a_production_build_refuses_a_custody_scan_wider_than_its_value() {
    let text = std::fs::read_to_string(Path::new("../../config/anvil.toml")).unwrap();
    let range = |blocks: u64| {
        let mut cfg: NodeConfig = toml::from_str(&text).unwrap();
        cfg.custody.evm.as_mut().unwrap().max_block_range = blocks;
        cfg
    };
    range(10_000).refuse_wide_custody_scans(true).unwrap();
    range(10_001).refuse_wide_custody_scans(false).unwrap();
    let err = range(10_001).refuse_wide_custody_scans(true).unwrap_err().to_string();
    assert!(err.contains("custody.evm.max_block_range at most 10000, and the config sets 10001"), "{err}");
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
fn a_staking_section_names_its_operator_registry_as_a_contract_of_its_own() {
    let text = std::fs::read_to_string(Path::new("../../config/anvil.toml")).unwrap();
    let staking = "0x00000000000000000000000000000000000000a5";
    let section = |registry: &str| {
        format!(
            "{text}\n[custody.evm.staking]\ncontract = '{staking}'\ncursor_path = 'staking.json'\n[custody.evm.staking.registry]\ncontract = '{registry}'\n"
        )
    };
    let cfg: NodeConfig = toml::from_str(&section("0x00000000000000000000000000000000000000a6")).unwrap();
    cfg.validate_custody().unwrap();
    // Every staking contract is the registry's, so a staking section names it.
    let unnamed = format!("{text}\n[custody.evm.staking]\ncontract = '{staking}'\ncursor_path = 'staking.json'\n");
    assert!(toml::from_str::<NodeConfig>(&unnamed).unwrap_err().to_string().contains("registry"));
    let usdc = cfg.custody.evm.as_ref().unwrap().usdc.to_string();
    for other in [staking, cfg.custody.vault.as_str(), usdc.as_str()] {
        let cfg: NodeConfig = toml::from_str(&section(other)).unwrap();
        let err = cfg.validate_custody().unwrap_err().to_string();
        assert!(err.contains("custody.evm.staking.registry.contract must be the operator registry"), "{err}");
    }
}

#[test]
fn a_fault_proof_section_needs_a_schedule_a_contract_of_its_own_and_a_bounded_penalty() {
    let text = std::fs::read_to_string(Path::new("../../config/anvil.toml")).unwrap();
    let (staking, registry, schedule) = (
        "0x00000000000000000000000000000000000000a5",
        "0x00000000000000000000000000000000000000a6",
        "0x00000000000000000000000000000000000000a7",
    );
    let section = |schedule: &str, contract: &str, initial_bps: i64, decay: &str| {
        format!(
            "{text}\n[custody.evm.staking]\ncontract = '{staking}'\ncursor_path = 'staking.json'\n[custody.evm.staking.registry]\ncontract = '{registry}'\n{schedule}\n[custody.evm.staking.registry.fault_proof]\ncontract = '{contract}'\npenalty = {{ initial_bps = {initial_bps}, decay = '{decay}' }}\n"
        )
    };
    let named = format!("schedule = '{schedule}'");
    let fault_proof = "0x00000000000000000000000000000000000000a8";
    let cfg: NodeConfig = toml::from_str(&section(&named, fault_proof, 10_000, "730d")).unwrap();
    cfg.validate_custody().unwrap();
    let recorded = cfg.custody.evm.as_ref().unwrap().staking.as_ref().unwrap().registry.fault_proof.as_ref().unwrap();
    let config = recorded.config().unwrap();
    assert_eq!((config.penalty.initial_bps, config.penalty.decay_ms), (10_000, 730 * 86_400_000));

    let refused = |text: String, expected: &str| {
        let cfg: NodeConfig = toml::from_str(&text).unwrap();
        let err = cfg.validate_custody().unwrap_err().to_string();
        assert!(err.contains(expected), "{err}");
    };
    refused(section("", fault_proof, 10_000, "730d"), "fault_proof needs a schedule");
    for other in [staking, registry, schedule, cfg.custody.vault.as_str()] {
        refused(section(&named, other, 10_000, "730d"), "fault_proof.contract must be the fault proof contract");
    }
    refused(
        section(&named, "0x0000000000000000000000000000000000000000", 10_000, "730d"),
        "fault_proof.contract must be a nonzero address",
    );
    // `LogHeads` answers `vault()` as the fault proof contract does, so its address is refused here.
    let heads = "0x00000000000000000000000000000000000000a9";
    let with_heads = format!(
        "{}\n[log_heads]\ncontract = '{heads}'\nrelayer = '0x00000000000000000000000000000000000000aa'\n",
        section(&named, heads, 10_000, "730d")
    );
    let with_checkpoints = with_heads.replace(
        "[log_heads]",
        "[checkpoints]\nrpc_url = 'http://127.0.0.1:8547'\nrelayer = '0x00000000000000000000000000000000000000ab'\nretry_interval = '1s'\nrpc_timeout = '5s'\n\n[log_heads]",
    );
    refused(with_checkpoints, "fault_proof.contract must be the fault proof contract");
    for initial_bps in [0, 10_001] {
        refused(section(&named, fault_proof, initial_bps, "730d"), "penalty.initial_bps must be from 1 to 10000");
    }
    for decay in ["0s", "3651d", "1500us"] {
        refused(section(&named, fault_proof, 10_000, decay), "penalty.decay must be whole milliseconds");
    }
}

#[test]
fn the_minter_is_a_contract_of_its_own_and_required() {
    let text = std::fs::read_to_string(Path::new("../../config/anvil.toml")).unwrap();
    let minter = "0x9fE46736679d2D9a65F0992F2272dE9f3c7fa6e0";
    assert!(text.contains(&format!("minter = \"{minter}\"")), "anvil.toml names the scenarios' minter");
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    cfg.validate_custody().unwrap();
    let unnamed = text.replace(&format!("minter = \"{minter}\"\n"), "");
    assert!(toml::from_str::<NodeConfig>(&unnamed).unwrap_err().to_string().contains("minter"));

    // The staking contract answers `monkey()` as the minter does, so only this check tells them apart.
    let staking = "0x00000000000000000000000000000000000000a5";
    let with_staking = format!(
        "{text}\n[custody.evm.staking]\ncontract = '{staking}'\ncursor_path = 'staking.json'\n[custody.evm.staking.registry]\ncontract = '0x00000000000000000000000000000000000000a6'\n"
    );
    let usdc = cfg.custody.evm.as_ref().unwrap().usdc.to_string();
    for other in [usdc.as_str(), cfg.custody.vault.as_str(), staking] {
        let cfg: NodeConfig = toml::from_str(&with_staking.replace(minter, other)).unwrap();
        let err = cfg.validate_custody().unwrap_err().to_string();
        assert!(err.contains("custody.evm.minter must be the MonkeyMinter"), "{other}: {err}");
    }
}
