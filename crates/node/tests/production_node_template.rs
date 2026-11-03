//! The production node template. Assembled as the deploy runbook says, with the benchmark
//! publishers and the production genesis, it loads and validates as a node config, and it holds what
//! a production build requires of one. Every placeholder carries a marker, so none is mistaken for a
//! value.

use std::fs;
use std::path::Path;

use engine_node::NodeConfig;
use engine_node::config::{CustodyProvider, FeedProvider, TeeProvider};

const TEMPLATE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/runbooks/production-node.toml");
const PUBLISHERS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/benchmark-publishers.toml");
const GENESIS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/production-genesis.toml");

fn template() -> String {
    fs::read_to_string(TEMPLATE).unwrap()
}

/// `cat template publishers genesis`, written to a file named for `test` and loaded by the node's loader.
fn assembled(test: &str) -> NodeConfig {
    let text = [TEMPLATE, PUBLISHERS, GENESIS].map(|path| fs::read_to_string(path).unwrap()).concat();
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("production-node-template-{test}.toml"));
    fs::write(&path, text).unwrap();
    NodeConfig::load(&path).unwrap()
}

#[test]
fn the_template_with_the_production_genesis_loads_as_a_node_config() {
    let cfg = assembled("loads");
    assert_eq!(cfg.custody.chain_id, 42_161, "Arbitrum One");
    assert!(cfg.benchmarks.is_some(), "the publishers' section is part of the assembled config");
    assert!(cfg.genesis.markets.len() > 30, "the production genesis is in place");
}

#[test]
fn the_template_holds_what_a_production_build_requires() {
    let cfg = assembled("production");
    // The refusals of a production build, which `NodeConfig::load` applies only when built with it.
    assert_eq!(cfg.tee.provider, TeeProvider::ConfigfsTsm);
    assert_eq!(cfg.custody.provider, CustodyProvider::Evm);
    assert_eq!(cfg.feeds.provider, FeedProvider::Venues);
    assert!(cfg.api.tls.is_some(), "[api.tls.acme] is required");
    assert!(cfg.api.allowed_origins.iter().all(|origin| origin.starts_with("https://")));
    assert!(cfg.alerts.critical.is_some(), "real attestation needs a Critical route");
    assert!(cfg.checkpoints.is_some() && cfg.tee.sealing.is_some(), "hardware custody needs both");
    let evm = cfg.custody.evm.as_ref().expect("EVM custody");
    assert!(evm.providers.len() >= 3);
    // The template's fresh-account budgets are the production values, the ceiling a production build enforces.
    let fresh = cfg.api.fresh_accounts;
    assert_eq!((fresh.shared.per_minute.get(), fresh.shared.burst.get()), (60, 120));
    assert_eq!((fresh.per_client.per_minute.get(), fresh.per_client.burst.get()), (12, 6));
    // A production build reads secrets only from a delivered bundle.
    let sealing = cfg.tee.sealing.as_ref().expect("[tee.sealing]");
    assert!(sealing.secrets.is_some(), "[tee.sealing.secrets] takes the delivery");
    let text = template();
    assert!(!text.contains("api_key_env =") && !text.contains("url_env ="), "the environment is refused");
}

#[test]
fn every_placeholder_is_marked_and_no_marker_is_a_standalone_comment() {
    const PLACEHOLDERS: [&str; 9] = [
        "example.com",
        "0x1111111111111111",
        "0x2222222222222222",
        "0x3333333333333333",
        "0x4444444444444444",
        "0x5555555555555555",
        "0x6666666666666666",
        "start_block = 1 ",
        "processor = ",
    ];
    for (n, line) in template().lines().enumerate() {
        let code = line.split('#').next().unwrap();
        let marked = line.contains("# OWNER") || line.contains("# REPLACE");
        if PLACEHOLDERS.iter().any(|placeholder| code.contains(placeholder)) {
            assert!(marked, "line {}: a placeholder with no marker: {line}", n + 1);
        }
        assert!(
            !line.trim_start().starts_with("# OWNER") && !line.trim_start().starts_with("# REPLACE"),
            "line {}",
            n + 1
        );
    }
}
