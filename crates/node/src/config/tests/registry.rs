//! The operator registry's lens and schedule, and the operator list's read interval.

use super::*;

/// `anvil.toml` with a staking contract, registry and `lens`.
fn anvil_with_lens(lens: &str) -> String {
    let anvil = std::fs::read_to_string(ANVIL).unwrap();
    format!(
        "{anvil}\n[custody.evm.staking]\ncontract = '0x3333333333333333333333333333333333333333'\ncursor_path = 'staking.json'\n[custody.evm.staking.registry]\ncontract = '0x4444444444444444444444444444444444444444'\nlens = '{lens}'\n"
    )
}

#[test]
fn the_registry_lens_must_be_a_contract_of_its_own() {
    let lens = "0x5555555555555555555555555555555555555555";
    let cfg: NodeConfig = toml::from_str(&anvil_with_lens(lens)).unwrap();
    cfg.validate().expect("a lens of its own loads");
    assert_eq!(cfg.custody.evm.unwrap().staking.unwrap().registry.lens, Some(lens.parse().unwrap()));
    for taken in ["0x3333333333333333333333333333333333333333", "0x4444444444444444444444444444444444444444"] {
        assert_validation_fails(&anvil_with_lens(taken), "custody.evm.staking.registry.lens must be the registry lens");
    }
    let anvil = std::fs::read_to_string(ANVIL).unwrap();
    let usdc = anvil
        .lines()
        .find_map(|line| line.strip_prefix("usdc = "))
        .unwrap()
        .trim()
        .trim_matches(['"', '\''])
        .to_owned();
    assert_validation_fails(&anvil_with_lens(&usdc), "custody.evm.staking.registry.lens must be the registry lens");
}

#[test]
fn the_registry_schedule_must_be_a_contract_of_its_own() {
    let lens = "0x5555555555555555555555555555555555555555";
    let with = |schedule: &str| format!("{}schedule = '{schedule}'\n", anvil_with_lens(lens));
    let own = "0x6666666666666666666666666666666666666666";
    let cfg: NodeConfig = toml::from_str(&with(own)).unwrap();
    cfg.validate().expect("a schedule of its own loads");
    assert_eq!(cfg.custody.evm.unwrap().staking.unwrap().registry.schedule, Some(own.parse().unwrap()));
    for taken in [lens, "0x3333333333333333333333333333333333333333", "0x4444444444444444444444444444444444444444"] {
        assert_validation_fails(&with(taken), "custody.evm.staking.registry.schedule must be the operator schedule");
    }
}

#[test]
fn the_operator_list_is_read_more_often_than_it_goes_stale() {
    let text = anvil_with_lens("0x5555555555555555555555555555555555555555");
    let with = |interval: &str| text.replacen("[api]", &format!("[api]\noperators_interval = \"{interval}\""), 1);
    assert_ne!(with("30s"), text, "the config has an [api] section to set it in");
    toml::from_str::<NodeConfig>(&with("30s")).unwrap().validate().expect("30s is under the stale bound");
    assert_validation_fails(&with("999ms"), "api.operators_interval must be at least 1s");
    assert_validation_fails(&with("5m"), "api.operators_interval must be under custody.evm.finality_stalled_after");
    // Without a lens nothing is read, so the interval bounds nothing.
    let anvil = std::fs::read_to_string(ANVIL).unwrap().replacen("[api]", "[api]\noperators_interval = \"10m\"", 1);
    toml::from_str::<NodeConfig>(&anvil).unwrap().validate().expect("no lens, no stale bound");
}
