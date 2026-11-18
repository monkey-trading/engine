//! The production node template. Assembled as the deploy runbook says, with the benchmark
//! publishers and the production genesis, it loads and validates as a node config, and it holds what
//! a production build requires of one, two TLS sources for every launch market included. Every
//! placeholder carries a marker, so none is mistaken for a value. The share markets are quoted by
//! Massive and Alpaca's consolidated feed, and without Alpaca would fall short of `min_sources`.

use std::fs;
use std::path::Path;

use engine_node::NodeConfig;
use engine_node::config::{CustodyProvider, FeedProvider, TeeProvider, Venue};

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
    // A rotation-set member witnesses, promotes and relays heads through its own funded caller.
    assert!(cfg.log_heads.is_some() && cfg.liveness.is_some(), "[log_heads] and [liveness] run the heartbeat rule");
    let evm = cfg.custody.evm.as_ref().expect("EVM custody");
    assert!(evm.providers.len() >= 3);
    // The template's fresh-account budgets are the production values, the ceiling a production build enforces.
    let fresh = cfg.api.fresh_accounts;
    assert_eq!((fresh.shared.per_minute.get(), fresh.shared.burst.get()), (60, 120));
    assert_eq!((fresh.per_client.per_minute.get(), fresh.per_client.burst.get()), (12, 6));
    // Its screening shares and reserve are within what a production build allows.
    cfg.access.as_ref().expect("[access]").refuse_dev_screening_shares(true).expect("the production screening shares");
    // A production build reads secrets only from a delivered bundle.
    let sealing = cfg.tee.sealing.as_ref().expect("[tee.sealing]");
    assert!(sealing.secrets.is_some(), "[tee.sealing.secrets] takes the delivery");
    let text = template();
    assert!(!text.contains("api_key_env =") && !text.contains("url_env ="), "the environment is refused");
    // Crypto from Coinbase and Kraken, FX from Massive and TraderMade, the CME Group exchanges' index
    // and commodity futures, every contract, from Massive and CME, and the US shares from Massive and
    // Alpaca meet min_sources 2.
    cfg.refuse_unmet_min_sources(true).expect("every launch market has its min_sources over TLS");
}

#[test]
fn the_share_markets_are_quoted_by_massive_and_alpacas_consolidated_feed() {
    let mut cfg = assembled("shares");
    let venues = cfg.feeds.venues.as_ref().expect("[feeds.venues]");
    let alpaca = venues.connections.iter().find(|conn| conn.venue == Venue::Alpaca).expect("an Alpaca connection");
    assert_eq!(alpaca.url, "wss://stream.data.alpaca.markets/v2/sip");
    let pair = alpaca.key_pair.as_ref().expect("Alpaca's key pair");
    assert_eq!(
        (pair.key_id_secret.to_string(), pair.secret_key_secret.to_string()),
        ("alpaca_key_id".into(), "alpaca_secret_key".into())
    );
    let stocks = venues.connections.iter().find(|conn| conn.url == "wss://socket.massive.com/stocks").expect("stocks");
    // Each share market, by its genesis symbol, on both vendors' connections.
    let shares: Vec<(u32, String)> =
        cfg.genesis.markets.iter().filter(|m| (40..=59).contains(&m.id)).map(|m| (m.id, m.symbol.clone())).collect();
    assert_eq!(shares.len(), 20);
    for conn in [alpaca, stocks] {
        let quoted: Vec<(u32, String)> = conn.symbols.iter().map(|s| (s.market, s.symbol.clone())).collect();
        assert_eq!(quoted, shares, "{}", conn.url);
    }

    // Without Alpaca, every share market has Massive alone, short of min_sources 2.
    cfg.feeds.venues.as_mut().unwrap().connections.retain(|conn| conn.venue != Venue::Alpaca);
    let err = cfg.refuse_unmet_min_sources(true).unwrap_err().to_string();
    for short in 40..=59 {
        assert!(err.contains(&format!("market {short} (")), "market {short}: {err}");
    }
    assert!(err.contains("market 40 (AAPL) has 1 (massive) of 2"), "{err}");
    for met in [1, 2, 3, 10, 11, 12, 13, 20, 21, 22, 25, 30, 31, 32] {
        assert!(!err.contains(&format!("market {met} (")), "market {met}: {err}");
    }
    // The Eurex and ICE markets are not in the launch genesis (#673).
    for deferred in [23, 24, 33] {
        assert!(!err.contains(&format!("market {deferred} (")), "market {deferred}: {err}");
    }
}

#[test]
fn the_cme_connection_names_cme_endpoints_and_its_oauth_secrets() {
    let cfg = assembled("cme");
    let venues = cfg.feeds.venues.as_ref().expect("[feeds.venues]");
    let cme = venues.connections.iter().find(|conn| conn.venue == Venue::Cme).expect("a CME connection");
    let oauth = cme.oauth.as_ref().expect("CME's OAuth login");
    assert_eq!(oauth.token_url, "https://auth.cmegroup.com/as/token.oauth2");
    assert_eq!(cme.url, "wss://markets.api.cmegroup.com/marketdatastream/v1");
    assert_eq!(
        (oauth.client_id_secret.to_string(), oauth.client_secret_secret.to_string()),
        ("cme_api_id".into(), "cme_api_password".into())
    );
}

#[test]
fn every_placeholder_is_marked_and_no_marker_is_a_standalone_comment() {
    const PLACEHOLDERS: [&str; 14] = [
        "example.com",
        "0x1111111111111111",
        "0x2222222222222222",
        "0x3333333333333333",
        "0x4444444444444444",
        "0x5555555555555555",
        "0x6666666666666666",
        "0x7777777777777777",
        "0x8888888888888888",
        "0x9999999999999999",
        "0xaaaaaaaaaaaaaaaa",
        "0xbbbbbbbbbbbbbbbb",
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
