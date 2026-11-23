//! The production node template. Assembled as the deploy runbook says, with the benchmark
//! publishers and the production genesis, it loads and validates as a node config, and it holds what
//! a production build requires of one, two TLS sources for every launch market but the futures markets
//! (Massive alone, by the owner's decision of 2026-11-20) included. Every
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
    // The launch release takes quotes from Siena (EPYC 8004) alone, at AMD's floor for it.
    let models: Vec<_> = cfg.tee.cpu_models.iter().map(|model| (model.cpuid(), model.minimum_tcb)).collect();
    let siena = engine_verify::Cpuid { family: 0x19, model: 0xA0, stepping: 0x02 };
    let floor = engine_verify::MinimumTcb { boot_loader: 0, tee: 0, snp: 0x1D, microcode: 0x1B };
    assert_eq!(models, [(siena, floor)]);
    // A rotation-set member witnesses, promotes and relays heads through its own funded caller.
    assert!(cfg.log_heads.is_some() && cfg.liveness.is_some(), "[log_heads] and [liveness] run the heartbeat rule");
    let evm = cfg.custody.evm.as_ref().expect("EVM custody");
    assert!(evm.providers.len() >= 3);
    // The template's fresh-account budgets are the production values, the ceiling a production build enforces.
    let fresh = cfg.api.fresh_accounts;
    assert_eq!((fresh.shared.per_minute.get(), fresh.shared.burst.get()), (60, 120));
    assert_eq!((fresh.per_client.per_minute.get(), fresh.per_client.burst.get()), (12, 6));
    // Its screening slots, shares and reserve are within what a production build allows.
    cfg.access.as_ref().expect("[access]").refuse_dev_screening_slots(true).expect("the production screening slots");
    // A production build reads secrets only from a delivered bundle.
    let sealing = cfg.tee.sealing.as_ref().expect("[tee.sealing]");
    assert!(sealing.secrets.is_some(), "[tee.sealing.secrets] takes the delivery");
    let text = template();
    assert!(!text.contains("api_key_env =") && !text.contains("url_env ="), "the environment is refused");
    // Crypto from Coinbase and Kraken, FX from Massive and TraderMade, and the US shares from Massive
    // and Alpaca meet min_sources 2. The index and commodity futures, every contract, meet their
    // min_sources 1 from Massive alone (owner's decision of 2026-11-20).
    cfg.refuse_unmet_min_sources(true).expect("every launch market has its min_sources over TLS");
}

/// The template takes a recovery cache every five minutes of sequenced time: a restart replays at most
/// about that much flow past the newest one (`sequencer::recovery_scale`). Its enclave address is
/// pinned in a release after the first, which created the key and leaves `[snapshots]` out.
#[test]
fn the_template_takes_a_recovery_cache_every_five_minutes() {
    let cfg = assembled("snapshots");
    let snapshots = cfg.snapshots.as_ref().expect("[snapshots]");
    assert_eq!(snapshots.interval, std::time::Duration::from_secs(300));
    assert!(snapshots.dir.starts_with("/data"), "the image's one writable disk");
    assert!(snapshots.trusted_signers.is_empty(), "a placeholder: a release pins the key a first one created");
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

/// The owner's decision of 2026-11-20: the CME-traded futures markets price from Massive's futures
/// feed alone at `min_sources = 1`, so the template has no CME Group connection or OAuth secret.
#[test]
fn the_futures_markets_price_from_massive_alone_by_the_decision_of_2026_11_20() {
    const FUTURES: [u32; 7] = [20, 21, 22, 25, 30, 31, 32];
    let mut cfg = assembled("futures");
    let venues = cfg.feeds.venues.as_ref().expect("[feeds.venues]");
    assert!(venues.connections.iter().all(|conn| conn.venue != Venue::Cme), "no CME Group connection");
    assert!(venues.connections.iter().all(|conn| conn.oauth.is_none()), "no connection logs in with OAuth credentials");
    let futures =
        venues.connections.iter().find(|conn| conn.url == "wss://socket.massive.com/futures").expect("futures");
    for id in FUTURES {
        let market = cfg.genesis.markets.iter().find(|m| m.id == id).expect("a futures market");
        assert_eq!(market.price_rules.min_sources, 1, "market {id}");
        let quoted = futures.symbols.iter().filter(|s| s.market == id).count();
        assert_eq!(quoted, market.contracts.len(), "market {id} quotes every contract it lists on Massive");
    }
    // Dropping Massive's futures connection leaves every one of them with no source, short of the 1 they need.
    cfg.feeds.venues.as_mut().unwrap().connections.retain(|conn| conn.url != "wss://socket.massive.com/futures");
    let err = cfg.refuse_unmet_min_sources(true).unwrap_err().to_string();
    for id in FUTURES {
        assert!(err.contains(&format!("market {id} (")), "market {id}: {err}");
    }
}

#[test]
fn every_placeholder_is_marked_and_no_marker_is_a_standalone_comment() {
    const PLACEHOLDERS: [&str; 17] = [
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
        "0xcccccccccccccccc",
        "0xdddddddddddddddd",
        "start_block = 1 ",
        "processor = ",
        "product_name = ",
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

/// The log is a mount point of its own volume: no store the production build keeps off the log's device is a path
/// inside the log directory (which would put it on that volume), and the template says so.
#[test]
fn the_template_keeps_the_log_on_a_volume_of_its_own() {
    let cfg = assembled("log-volume");
    let log = &cfg.node.log_dir;
    let snapshots = cfg.snapshots.as_ref().map(|s| &s.dir);
    let daily = cfg.daily_snapshots.as_ref().map(|s| &s.dir);
    for dir in [Some(&cfg.history.dir), snapshots, daily].into_iter().flatten() {
        assert!(!dir.starts_with(log) && !log.starts_with(dir), "{} is not apart from the log", dir.display());
    }
    assert!(template().contains("a mount point of its own"), "the template states the layout");
}
