use super::*;

use engine_core::types::MarketId;

use crate::test_support::{ANVIL_CONFIG as ANVIL, DEV_CONFIG as DEV, own_operator_key};

use crate::config::telemetry_tests::CRITICAL_ROUTE;

#[test]
fn the_committed_dev_config_loads() {
    let cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    assert_eq!(cfg.node.tick, Duration::from_millis(250));
    assert!(!cfg.genesis.markets.is_empty());
    assert!(!cfg.operator.rotation_enabled);
    assert_eq!((cfg.api.per_ip.per_second.get(), cfg.api.per_ip.burst.get()), (50, 100));
}

#[test]
fn a_mistyped_key_fails_the_boot() {
    let text = std::fs::read_to_string(DEV).unwrap().replace("checkpoint_every", "checkpoint_evry");
    assert!(toml::from_str::<NodeConfig>(&text).is_err());
    let text = std::fs::read_to_string(DEV).unwrap().replace("house_cap_bps", "house_capp_bps");
    assert!(toml::from_str::<NodeConfig>(&text).is_err());
}

#[test]
fn a_zero_segment_size_is_refused() {
    let text = std::fs::read_to_string(DEV).unwrap().replace("segment_commands = 1000", "segment_commands = 0");
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn unsigned_requests_need_the_mock_tee_and_mock_custody() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let tee = dev.replace("provider = \"mock\" # \"configfs_tsm\"", "provider = \"configfs_tsm\" #");
    let anvil = std::fs::read_to_string(ANVIL).unwrap();
    let custody = anvil.replace("auth = \"eip712\"", "auth = \"none\"");
    for (text, source) in [(&tee, &dev), (&custody, &anvil)] {
        assert_ne!(text, source, "the replacement must change the config");
        let cfg: NodeConfig = toml::from_str(text).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("node.auth = \"none\""), "{err:#}");
        let route = if text.contains("\n[alerts.critical]") { "" } else { CRITICAL_ROUTE };
        let signed = format!("{}{route}", text.replace("auth = \"none\"", "auth = \"eip712\""));
        let mut signed: NodeConfig = toml::from_str(&signed).unwrap();
        // Real attestation also refuses the dev configs' published operator key.
        signed.operator.public_key = own_operator_key();
        signed.tee.use_published_mock_key = false;
        signed.validate().unwrap();
    }
}

#[test]
fn signed_requests_take_their_domain_from_the_custody_chain_and_vault() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    assert_eq!(toml::from_str::<NodeConfig>(&dev).unwrap().client_auth().unwrap(), ClientAuth::Unsigned);
    let vault = "0x5fbdb2315678afecb367f032d93f642f64180aa3";
    let signed = dev
        .replace("auth = \"none\"", "auth = \"eip712\"")
        .replace("chain_id = 31337", "chain_id = 42161")
        .replace("vault = \"0x0000000000000000000000000000000000000000\"", &format!("vault = \"{vault}\""));
    let cfg: NodeConfig = toml::from_str(&signed).unwrap();
    assert_eq!(cfg.client_auth().unwrap(), ClientAuth::Eip712 { chain_id: 42_161, vault: vault.parse().unwrap() });
}

#[test]
fn the_operator_is_its_public_keys_address_under_the_custody_chain_and_vault() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let operator = toml::from_str::<NodeConfig>(&dev).unwrap().operator().unwrap();
    // The address of the local test key 0x4242…42.
    let address: AccountId = "0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025".parse().unwrap();
    assert_eq!(operator, Operator { address, chain_id: 31_337, vault: AccountId([0; 20]) });
    let uncompressed = dev.split("public_key = \"0x04").nth(1).unwrap().split('"').next().unwrap();
    let compressed = format!("0x03{}", &uncompressed[..64]);
    let text = dev.replace(&format!("0x04{uncompressed}"), &compressed);
    assert_eq!(toml::from_str::<NodeConfig>(&text).unwrap().operator().unwrap(), operator);

    let private_key = "0x4242424242424242424242424242424242424242424242424242424242424242";
    let off_curve = format!("0x04{}", "00".repeat(64));
    for bad in [private_key, off_curve.as_str(), "operator"] {
        let text = dev.replace(&format!("0x04{uncompressed}"), bad);
        let err = toml::from_str::<NodeConfig>(&text).unwrap().validate().unwrap_err();
        assert!(err.to_string().contains("operator.public_key"), "{bad}: {err:#}");
    }
}

#[test]
fn genesis_values_the_engine_would_refuse_fail_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let capital = |amount: &str| dev.replace("house_capital = \"10000000\"", &format!("house_capital = \"{amount}\""));
    let rate = |bps: i64| {
        dev.replace("{ name = \"SOFR\", annual_bps = 400 }", &format!("{{ name = \"SOFR\", annual_bps = {bps} }}"))
    };
    let cap = |orders: usize| dev.replace("max_resting_orders = 100", &format!("max_resting_orders = {orders}"));
    // A market names "Sofr" while genesis rates "SOFR".
    let misspelt = dev.replacen("benchmark = \"SOFR\"", "benchmark = \"Sofr\"", 1);
    // Genesis rates no benchmark at all, leaving every rate to the rate feed.
    let unrated = {
        let start = dev.find("benchmarks = [").unwrap();
        let end = start + dev[start..].find("]\n").unwrap() + 1;
        format!("{}benchmarks = []{}", &dev[..start], &dev[end..])
    };
    let total_floor = dev.replace("house_total_cap_floor = \"2000000\"", "house_total_cap_floor = \"-1\"");
    let market_floor = dev.replace("house_cap_floor = \"300000\"", "house_cap_floor = \"-1\"");
    // Each genesis command: the parameters, a benchmark rate, a market, and the house capital.
    let refused = [
        (dev.replace("profit_fee_bps = 1000", "profit_fee_bps = 10000"), "genesis.params: the engine rejects it"),
        (total_floor, "genesis.params: the engine rejects it: house_total_cap_floor out of range"),
        (rate(40_000), "genesis benchmark SOFR: the engine rejects it: benchmark rate 40000bps out of range"),
        (rate(i64::MIN), "genesis benchmark SOFR: the engine rejects it"),
        (market_floor, "genesis market BTC-USD: the engine rejects it: house_cap_floor out of range"),
        (dev.replace("house_cap_bps = 1000\n", "house_cap_bps = 0\n"), "genesis market AAPL: the engine rejects it"),
        (capital("-1"), "genesis.house_capital: the engine rejects it"),
        (capital("1000000000.000000001"), "genesis.house_capital: the engine rejects it"),
        // The latest version takes a divergence trigger only as a share of each market's bound, and a
        // share above zero can never fire at a zero bound.
        (
            dev.replace("divergence_share_bps = 6000", "divergence_bps = 15"),
            "genesis.params: the engine rejects it: from protocol V13 a divergence trigger is divergence_share_bps",
        ),
        (
            dev.replacen("max_divergence_bps = 25", "max_divergence_bps = 0", 1),
            "genesis market BTC-USD: the engine rejects it: a divergence share above zero can never fire",
        ),
        (cap(0), "genesis.params: the engine rejects it: max_resting_orders must be in 1..=1000"),
        (cap(1_001), "genesis.params: the engine rejects it: max_resting_orders must be in 1..=1000"),
        (misspelt, "the engine rejects it: benchmark Sofr has no rate"),
    ];
    for (text, expected) in &refused {
        assert_ne!(text, &dev, "the replacement must change the config");
        let err = toml::from_str::<NodeConfig>(text).unwrap().validate().unwrap_err();
        assert!(format!("{err:#}").contains(expected), "expected {expected:?}, got {err:#}");
    }
    // A house that starts with no capital, the rate and resting-order limits themselves, and a
    // genesis that rates no benchmark are accepted.
    for text in [capital("0"), rate(-10_000), cap(1_000), unrated] {
        assert_ne!(text, dev, "the replacement must change the config");
        toml::from_str::<NodeConfig>(&text).unwrap().validate().unwrap();
    }
}

#[test]
fn a_genesis_market_converting_at_anything_but_its_pair_listed_before_it_fails_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let via = "quote_currency = { currency = \"eur\", fx_market = 10 }";
    let at = |fx_market: u32| format!("quote_currency = {{ currency = \"eur\", fx_market = {fx_market} }}");
    // US500 (20) is not EUR-USD, 99 is never listed, and XAU-USD (30), renamed EUR-USD here while
    // the EUR-USD listing takes another symbol, is listed after Germany 40.
    let not_the_pair = dev.replace(via, &at(20));
    let unlisted = dev.replace(via, &at(99));
    let listed_after = dev
        .replace(via, &at(30))
        .replace("symbol = \"EUR-USD\"", "symbol = \"EUR-USD-SPOT\"")
        .replace("symbol = \"XAU-USD\"", "symbol = \"EUR-USD\"");
    for text in [&not_the_pair, &unlisted, &listed_after] {
        assert_ne!(text, &dev, "the replacement must change the config");
        let err = toml::from_str::<NodeConfig>(text).unwrap().validate().unwrap_err();
        assert!(format!("{err:#}").contains("is not the listed EUR-USD market"), "{err:#}");
    }
}

#[test]
fn genesis_entries_listed_twice_fail_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let us500 = &dev[dev.find("[[genesis.markets]]\nid = 20").unwrap()..];
    let us500 = &us500[..us500[1..].find("[[genesis.markets]]").unwrap() + 1];
    let refused = [
        // A block copied from US500 that keeps its id under a new symbol.
        (format!("{dev}\n{}", us500.replace("\"US500\"", "\"US100\"")), "genesis lists market 20 twice"),
        // The same listing under a new id.
        (format!("{dev}\n{}", us500.replace("id = 20", "id = 21")), "genesis lists market symbol US500 twice"),
        (
            dev.replace("{ name = \"ESTR\", annual_bps = 200 }", "{ name = \"SOFR\", annual_bps = 200 }"),
            "genesis lists benchmark SOFR twice",
        ),
    ];
    for (text, expected) in &refused {
        assert_ne!(text, &dev, "the replacement must change the config");
        let err = toml::from_str::<NodeConfig>(text).unwrap().validate().unwrap_err();
        assert!(format!("{err:#}").contains(expected), "expected {expected:?}, got {err:#}");
    }
}

#[test]
fn a_logging_level_that_hides_the_nodes_errors_fails_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let level = |level: &str| dev.replace("level = \"info\"", &format!("level = \"{level}\""));
    let refused = [
        "warning",
        "",
        "off",
        "engine_node=off",
        "info,engine_node=off",
        "engine_node::node=error",
        "error,engine_node::sequencer=off",
        "info,engine_node::custody=0",
        "info,engine_node:=off",
        "info,engine=off",
        "info,engine_node::feeds[{venue}]=off",
    ];
    for refused in refused {
        let err = toml::from_str::<NodeConfig>(&level(refused)).unwrap().validate().unwrap_err();
        assert!(format!("{err:#}").contains("logging.level must enable at least"), "{refused:?}: {err:#}");
    }
    // A span-scoped directive only enables events in its scope, and another crate is not the node.
    let accepted = [
        "error",
        "warn",
        "debug",
        "off,engine_node=error",
        "info,hyper=off,engine_node::feeds=error",
        "info,engine_node::trusted_time[sample]=off",
        "info,engine_node::feeds[{venue=kraken}]=off",
        "info,engine_core=off",
    ];
    for accepted in accepted {
        toml::from_str::<NodeConfig>(&level(accepted))
            .unwrap()
            .validate()
            .unwrap_or_else(|e| panic!("{accepted}: {e:#}"));
    }
}

#[test]
fn the_dev_config_checks_time_against_three_roughtime_servers() {
    let cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    let keys: Vec<String> = cfg.time.servers.iter().map(|s| STANDARD.encode(s.public_key.as_bytes())).collect();
    assert_eq!(
        keys,
        [
            "AW5uAoTSTDfG5NfY1bTh08GUnOqlRb+HVhbJ3ODJvsE=",
            "v2CievhgKsxzlWPwIkYFUXeA51Akhkv5uhJCj1/kbiY=",
            "iBVjxg/1j7y1+kQUTBYdTabxCppesU/07D4PMDJk2WA=",
        ]
    );
    assert_eq!(cfg.time.max_drift, Duration::from_secs(15));
}

#[test]
fn a_public_key_that_is_not_32_bytes_of_base64_fails_the_boot() {
    let key = "AW5uAoTSTDfG5NfY1bTh08GUnOqlRb+HVhbJ3ODJvsE=";
    for bad in
        ["not base64!", "AW5uAoTSTDfG5NfY1bTh08GUnOqlRb+HVhbJ3ODJvg==", "AW5uAoTSTDfG5NfY1bTh08GUnOqlRb+HVhbJ3ODJvsEA"]
    {
        let text = std::fs::read_to_string(DEV).unwrap().replace(key, bad);
        assert!(toml::from_str::<NodeConfig>(&text).is_err(), "{bad}");
    }
}

#[test]
fn fewer_than_two_servers_or_a_key_listed_twice_is_refused() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.time.servers.truncate(1);
    assert!(format!("{:#}", cfg.validate().unwrap_err()).contains("at least two servers"));

    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.time.servers[2].public_key = cfg.time.servers[0].public_key;
    assert!(format!("{:#}", cfg.validate().unwrap_err()).contains("same public key twice"));
}

#[test]
fn a_zero_drift_bound_or_sampling_interval_is_refused() {
    for (from, to) in [("max_drift = \"15s\"", "max_drift = \"0s\""), ("interval = \"10s\"", "interval = \"0s\"")] {
        let text = std::fs::read_to_string(DEV).unwrap().replace(from, to);
        let cfg: NodeConfig = toml::from_str(&text).unwrap();
        assert!(format!("{:#}", cfg.validate().unwrap_err()).contains("time.max_drift and time.interval"), "{to}");
    }
}

#[test]
fn a_round_that_could_outlast_the_grace_is_refused() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.time.max_drift = Duration::from_millis(24_999);
    assert!(cfg.validate().is_ok());
    cfg.time.max_drift = Duration::from_secs(25);
    assert!(format!("{:#}", cfg.validate().unwrap_err()).contains("twice time.max_drift"));
}

#[test]
fn a_server_address_must_be_host_and_port() {
    for (address, ok) in [
        ("rough.time.nl:2002", true),
        ("[::1]:5319", true),
        ("rough.time.nl", false),
        (":2002", false),
        ("a:99999", false),
    ] {
        let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
        cfg.time.servers[0].address = address.into();
        assert_eq!(cfg.validate().is_ok(), ok, "{address}");
    }
}

#[test]
fn a_history_retention_of_zero_or_beyond_millisecond_range_is_refused() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    for (retention, refused) in [
        ("0s", Some("must be non-zero")),
        ("300000000years", Some("too long")),
        ("1ms", None),
        ("292000000years", None),
    ] {
        let text = dev.replace("retention = \"365d\"", &format!("retention = \"{retention}\""));
        assert_ne!(text, dev, "the replacement must change the config");
        let checked = toml::from_str::<NodeConfig>(&text).unwrap().validate();
        match refused {
            Some(cause) => assert!(format!("{:#}", checked.unwrap_err()).contains(cause), "{retention}"),
            None => checked.unwrap(),
        }
    }
}

#[test]
fn a_zero_tick_is_refused() {
    let text = std::fs::read_to_string(DEV).unwrap().replace("tick = \"250ms\"", "tick = \"0s\"");
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn the_committed_anvil_config_loads_with_evm_custody_and_no_genesis_capital() {
    let cfg = NodeConfig::load(Path::new(ANVIL)).unwrap();
    assert_eq!(cfg.custody.provider, CustodyProvider::Evm);
    assert_eq!(cfg.custody.evm.unwrap().finality_tag, FinalityTag::Safe);
    assert_eq!(cfg.genesis.house_capital, Fixed::ZERO);
}

#[test]
fn the_custody_section_must_match_its_provider() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let evm_without_section = dev
        .replace("provider = \"mock\"\nchain_id", "provider = \"evm\"\nchain_id")
        .replace("auth = \"none\"", "auth = \"eip712\"");
    assert_validation_fails(&evm_without_section, "needs a [custody.evm] section");
    let anvil = std::fs::read_to_string(ANVIL).unwrap();
    let mock_with_section = anvil.replace("provider = \"evm\"", "provider = \"mock\"");
    assert_validation_fails(&mock_with_section, "[custody.evm] is set");
    assert_validation_fails(&anvil.replace("max_block_range = 1000", "max_block_range = 0"), "must be non-zero");
    assert_validation_fails(&anvil.replace("http://127.0.0.1:8545", "not a url"), "custody.evm.providers.rpc_url");
}

#[test]
fn genesis_house_capital_is_refused_with_evm_custody() {
    let anvil = std::fs::read_to_string(ANVIL).unwrap();
    let text = anvil.replace("house_capital = \"0\"", "house_capital = \"1000\"");
    assert_ne!(text, anvil, "the replacement must change the config");
    assert_validation_fails(&text, "genesis.house_capital must be zero with EVM custody");
}

#[test]
fn the_rpc_url_is_redacted_from_debug_output() {
    let text = std::fs::read_to_string(ANVIL).unwrap().replace("127.0.0.1:8545", "rpc.example/v2/secret-key");
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    let printed = format!("{cfg:?}");
    assert!(printed.contains("SecretUrl(<redacted>)"), "{printed}");
    assert!(!printed.contains("secret-key"), "{printed}");
}

#[test]
fn a_plain_http_rpc_url_is_refused_unless_it_is_a_literal_loopback_address() {
    let anvil = std::fs::read_to_string(ANVIL).unwrap();
    let remote = anvil.replace("http://127.0.0.1:8545", "http://rpc.example/v2/key");
    assert_validation_fails(&remote, "must use https");
    // `localhost` resolves through the host's `/etc/hosts`, so only literal loopback passes.
    assert_validation_fails(&anvil.replace("127.0.0.1:8545", "localhost:8545"), "must use https");
    let cfg: NodeConfig = toml::from_str(&remote.replace("http://rpc.example", "https://rpc.example")).unwrap();
    cfg.validate().unwrap();
}

#[test]
fn a_malformed_config_line_is_named_but_never_quoted() {
    let anvil = std::fs::read_to_string(ANVIL).unwrap();
    let line = anvil.lines().position(|l| l.contains("rpc_url")).unwrap() + 1;
    let text =
        anvil.replace("rpc_url = \"http://127.0.0.1:8545\"", "rpc_url = [\"https://rpc.example/v2/secret-key\"]");
    let path = std::env::temp_dir().join(format!("engine-config-test-{}.toml", std::process::id()));
    std::fs::write(&path, text).unwrap();

    let err = format!("{:#}", NodeConfig::load(&path).unwrap_err());
    assert!(err.contains(&format!("line {line}:")), "{err}");
    assert!(!err.contains("secret-key"), "{err}");
}

// ── Helpers ──

#[test]
fn the_committed_venue_feeds_price_both_crypto_markets_from_three_venues() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.feeds.provider = FeedProvider::Venues;
    cfg.validate().unwrap();
    let connections = cfg.feeds.venues.unwrap().connections;
    let venues: Vec<Venue> = connections.iter().map(|c| c.venue).collect();
    assert_eq!(venues, [Venue::Coinbase, Venue::Kraken, Venue::Bitstamp]);
    for conn in connections {
        let markets: Vec<MarketId> = conn.symbols.iter().map(|s| s.market).collect();
        assert_eq!(markets, [1, 2], "{}", conn.venue.name());
    }
}

#[test]
fn an_interval_that_outlasts_the_venue_timeout_is_refused() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.feeds.interval = cfg.feeds.venues.as_ref().unwrap().timeout;
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("feeds.interval must be shorter than feeds.venues.timeout"), "{err}");
}

#[test]
fn venue_feeds_need_their_section() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.feeds.provider = FeedProvider::Venues;
    cfg.feeds.venues = None;
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("needs [feeds.venues]"), "{err}");
}

#[test]
fn venue_settings_that_would_fail_silently_are_refused() {
    type Edit = fn(&mut VenuesSection);
    let cases: [(&str, Edit); 13] = [
        ("non-zero", |v| v.timeout = Duration::ZERO),
        ("non-zero", |v| v.reconnect_min = Duration::ZERO),
        ("non-zero", |v| v.stale_after = Duration::ZERO),
        ("exceeds reconnect_max", |v| v.reconnect_max = v.reconnect_min / 2),
        ("no connections", |v| v.connections.clear()),
        ("coinbase lists market 1 twice", |v| {
            let mut again = v.connections[0].clone();
            again.url.push_str("/again");
            v.connections.push(again)
        }),
        ("must be wss://", |v| v.connections[0].url = "ws://advanced-trade-ws.coinbase.com".into()),
        ("must be on coinbase.com", |v| v.connections[0].url = "wss://ws.kraken.com/v2".into()),
        ("must be on coinbase.com", |v| v.connections[0].url = "wss://coinbase.com.example.net".into()),
        ("must be on coinbase.com", |v| v.connections[0].url = "wss://evilcoinbase.com".into()),
        ("has no symbols", |v| v.connections[0].symbols.clear()),
        ("lists market 1 twice", |v| v.connections[0].symbols[1].market = 1),
        ("lists symbol BTC-USD twice", |v| v.connections[0].symbols[1].symbol = "BTC-USD".into()),
    ];
    for (expected, edit) in cases {
        let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
        edit(cfg.feeds.venues.as_mut().unwrap());
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(expected), "expected {expected:?}, got {err}");
    }
}

#[test]
fn a_zero_api_timeout_is_refused() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    for (from, to) in [
        ("send_timeout = \"30s\"", "send_timeout = \"0s\""),
        ("read_timeout = \"30s\"", "read_timeout = \"0s\""),
        ("handshake_timeout = \"10s\"", "handshake_timeout = \"0s\""),
    ] {
        let cfg: NodeConfig = toml::from_str(&dev.replace(from, to)).unwrap();
        assert!(cfg.validate().is_err(), "{to}");
    }
}

#[test]
fn a_zero_api_rate_or_cap_fails_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    for (from, to) in [
        ("per_ip = { per_second = 50,", "per_ip = { per_second = 0,"),
        ("per_second = 10, burst = 20 }", "per_second = 10, burst = 0 }"),
        ("max_subscribers = 256", "max_subscribers = 0"),
    ] {
        // Each edit touches one value, so nothing else can be what fails the parse.
        assert_eq!(dev.matches(from).count(), 1, "{from}");
        assert!(toml::from_str::<NodeConfig>(&dev.replace(from, to)).is_err(), "{to}");
    }
}

fn assert_validation_fails(text: &str, expected: &str) {
    let cfg: NodeConfig = toml::from_str(text).unwrap();
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains(expected), "{err}");
}

#[test]
fn genesis_stress_and_toxicity_spreads_that_reach_the_price_fail_the_boot() {
    let mut cfg: NodeConfig = toml::from_str(&std::fs::read_to_string(DEV).unwrap()).unwrap();
    cfg.validate().unwrap();
    let top = cfg.genesis.params.stress_levels.len() - 1;
    cfg.genesis.params.stress_levels[top].min_spread_bps = 10_000;
    let err = cfg.validate().unwrap_err();
    assert!(format!("{err:#}").contains("reach the price"), "{err:#}");
}
