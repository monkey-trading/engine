use std::path::Path;

use alloy::primitives::utils::{parse_ether, parse_units};

use crate::config::SecretUse;
use crate::test_support::{ANVIL_CONFIG, DEV_CONFIG};

use super::*;

#[test]
fn the_section_reads_fees_in_gwei_and_balances_in_ether() {
    let parsed: SeedRelaySection = toml::from_str(&section("0.1", "0.05")).unwrap();
    let fee: U256 = parse_units("0.1", "gwei").unwrap().into();
    assert_eq!(parsed.l2_max_fee_per_gas, fee);
    assert_eq!(parsed.min_balance, parse_ether("0.05").unwrap());
    // The parser takes a negative amount's absolute value; the section refuses it instead.
    let negative = toml::from_str::<SeedRelaySection>(&section("-0.1", "0.05")).unwrap_err();
    assert!(negative.to_string().contains("negative"), "{negative}");
    assert!(toml::from_str::<SeedRelaySection>(&section("0.1", "-1")).is_err());
}

#[test]
fn a_valid_section_loads_beside_evm_custody() {
    let mut cfg = anvil_with(&section("0.1", "0.05"));
    cfg.validate_seed_relay().unwrap();
    // With mock custody there is nothing to read the receiver through.
    let mut dev = NodeConfig::load(Path::new(DEV_CONFIG)).unwrap();
    dev.seed_relay = cfg.seed_relay.take();
    assert!(dev.validate_seed_relay().unwrap_err().to_string().contains("EVM custody"));
}

#[test]
fn a_section_that_would_fail_quietly_is_refused() {
    let cases = [
        ("relayer = '0x0000000000000000000000000000000000000000'", "relayer must be non-zero"),
        ("relay = '0x0000000000000000000000000000000000000000'", "relay and receiver"),
        ("poll_interval = '0s'", "non-zero"),
        ("resend_after = '0s'", "non-zero"),
        ("beacon_api = 'http://10.0.0.1:5052'", "beacon_api must use https"),
        ("rpc_url = 'http://example.com'", "rpc_url must use https"),
    ];
    for (line, expected) in cases {
        // Only the section's own line: the providers' `rpc_url` lines come later.
        let key = format!("{} =", line.split(' ').next().unwrap());
        let base = section("0.1", "0.05");
        let at = base.find(&key).unwrap();
        let end = at + base[at..].find('\n').unwrap();
        let text = format!("{}{line}{}", &base[..at], &base[end..]);
        let err = anvil_with(&text).validate_seed_relay().unwrap_err().to_string();
        assert!(err.contains(expected), "{line}: {err}");
    }
    let mut zero_fee = anvil_with(&section("0", "0.05"));
    assert!(zero_fee.validate_seed_relay().unwrap_err().to_string().contains("l2_max_fee_per_gas"));
    zero_fee.seed_relay.as_mut().unwrap().l2_max_fee_per_gas = U256::from(1);
    zero_fee.validate_seed_relay().unwrap();
    let two: String = section("0.1", "0.05").replace("[[providers]]\nname = 'c'\nrpc_url = 'http://127.0.0.1:3'\n", "");
    assert!(anvil_with(&two).validate_seed_relay().unwrap_err().to_string().contains("at least three"));
}

#[test]
fn the_relayer_must_be_a_key_apart_from_the_checkpoint_relayer() {
    let mut cfg = anvil_with(&section("0.1", "0.05"));
    cfg.checkpoints = Some(
        toml::from_str(
            "rpc_url = 'http://127.0.0.1:8545'\n\
             relayer = '0x2222222222222222222222222222222222222222'\n\
             retry_interval = '2s'\n\
             rpc_timeout = '10s'",
        )
        .unwrap(),
    );
    let err = cfg.validate_seed_relay().unwrap_err().to_string();
    assert!(err.contains("apart from checkpoints.relayer"), "{err}");
    cfg.checkpoints.as_mut().unwrap().relayer = Address::repeat_byte(0x33);
    cfg.validate_seed_relay().unwrap();
}

#[test]
fn a_key_a_url_names_turns_the_relayer_off_until_it_is_delivered() {
    let text = section("0.1", "0.05")
        .replace("beacon_api = 'http://127.0.0.1:5052'", "beacon_api = 'https://beacon.example/{beacon_key}'");
    let mut cfg = anvil_with(&text);
    let named = cfg.named_secrets().unwrap();
    assert!(named.iter().any(|n| n.field == "seed_relay.beacon_api" && n.usage == SecretUse::SeedRelay), "{named:?}");
    let relay = cfg.seed_relay.as_mut().unwrap();
    assert!(!relay.delivered());
    relay.beacon_api.deliver_test_key("key");
    assert!(relay.delivered());
}

// ── Helpers ──

/// A section over three local providers, with the given fee and balance threshold.
fn section(fee: &str, balance: &str) -> String {
    format!(
        "chain_id = 31337\n\
         rpc_url = 'http://127.0.0.1:4'\n\
         relayer = '0x2222222222222222222222222222222222222222'\n\
         beacon_api = 'http://127.0.0.1:5052'\n\
         relay = '0x4444444444444444444444444444444444444444'\n\
         receiver = '0x5555555555555555555555555555555555555555'\n\
         poll_interval = '60s'\n\
         rpc_timeout = '10s'\n\
         state_timeout = '300s'\n\
         resend_after = '1h'\n\
         l2_max_fee_per_gas = '{fee}'\n\
         min_balance = '{balance}'\n\
         [[providers]]\nname = 'a'\nrpc_url = 'http://127.0.0.1:1'\n\
         [[providers]]\nname = 'b'\nrpc_url = 'http://127.0.0.1:2'\n\
         [[providers]]\nname = 'c'\nrpc_url = 'http://127.0.0.1:3'\n"
    )
}

/// The Anvil config, which has EVM custody, with `section` as its `[seed_relay]`.
fn anvil_with(section: &str) -> NodeConfig {
    let mut cfg = NodeConfig::load(Path::new(ANVIL_CONFIG)).unwrap();
    cfg.seed_relay = Some(toml::from_str(section).unwrap());
    cfg
}
