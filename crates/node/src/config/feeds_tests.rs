use super::*;

use chrono::DateTime;
use engine_core::futures::FuturesContract;

use crate::test_support::DEV_CONFIG as DEV;

/// The vendor connections that quote the dev genesis' FX, index, commodity, and share markets, as a
/// production `[feeds.venues]` lists them: EUR-USD from two vendors, the rest from Massive alone.
const VENDORS: &str = r#"
[[feeds.venues.connections]]
venue = "massive"
url = "wss://socket.massive.com/forex"
api_key_env = "MASSIVE_API_KEY"
symbols = [{ market = 10, symbol = "EUR-USD" }]

[[feeds.venues.connections]]
venue = "massive"
url = "wss://socket.massive.com/futures"
api_key_env = "MASSIVE_API_KEY"
symbols = [
  { market = 20, symbol = "ESZ6", contract = "ESZ6" },
  { market = 20, symbol = "ESH7", contract = "ESH7" },
  { market = 30, symbol = "GCZ6", contract = "GCZ6" },
]

[[feeds.venues.connections]]
venue = "massive"
url = "wss://socket.massive.com/stocks"
api_key_env = "MASSIVE_API_KEY"
symbols = [{ market = 40, symbol = "AAPL" }]

[[feeds.venues.connections]]
venue = "tradermade"
url = "wss://stream.tradermade.com/feedAdv"
api_key_env = "TRADERMADE_API_KEY"
symbols = [{ market = 10, symbol = "EURUSD" }]
"#;

#[test]
fn vendor_connections_with_their_keys_and_contracts_load() {
    let cfg = with_vendors();
    cfg.validate().unwrap();
    let connections = &cfg.feeds.venues.as_ref().unwrap().connections;
    let vendors: Vec<Venue> = connections.iter().map(|c| c.venue).filter(|v| v.needs_key()).collect();
    use Venue::{Massive, Tradermade};
    assert_eq!(vendors, [Massive, Massive, Massive, Tradermade]);
    // Each key comes from the variable its connection names, and never reaches a log.
    let env = |var: &str| (var == "TRADERMADE_API_KEY").then(|| "tradermade-secret".to_owned());
    let key = connections[5].api_key(env).unwrap().expect("a vendor key");
    assert_eq!(key.expose(), "tradermade-secret");
    assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
    assert!(connections[0].api_key(env).unwrap().is_none());
    let unset = format!("{:#}", connections[2].api_key(env).unwrap_err());
    assert!(unset.contains("massive api_key_env") && unset.contains("MASSIVE_API_KEY is not set"), "{unset}");
    let empty = format!("{:#}", connections[2].api_key(|_| Some(String::new())).unwrap_err());
    assert!(empty.contains("is not set"), "{empty}");
}

// Runs in the production lane too, which filters on `production_build`.
#[test]
fn a_production_build_takes_the_vendor_feeds_and_refuses_the_mock_feed() {
    let mut cfg = with_vendors();
    cfg.validate().unwrap();
    (cfg.tee.provider, cfg.custody.provider) = (TeeProvider::ConfigfsTsm, CustodyProvider::Evm);
    cfg.feeds.provider = FeedProvider::Venues;
    cfg.refuse_mocks(true).unwrap();
    cfg.feeds.provider = FeedProvider::Mock;
    let err = cfg.refuse_mocks(true).unwrap_err().to_string();
    assert!(err.contains("feeds.provider set to"), "{err}");
}

#[test]
fn vendor_settings_that_would_fail_or_could_be_forged_are_refused() {
    type Edit = fn(&mut Vec<VenueConnection>);
    let cases: [(&str, Edit); 7] = [
        ("massive api_key_env must be set", |c| c[2].api_key_env = None),
        ("coinbase api_key_env must be set", |c| c[0].api_key_env = c[2].api_key_env.clone()),
        ("massive url must be one of its", |c| c[2].url = "wss://socket.massive.com/crypto".into()),
        ("tradermade url must be on tradermade.com", |c| c[5].url = "wss://stream.example.com/feedAdv".into()),
        ("massive lists market 20 contract ESZ6 twice", |c| {
            let futures = c[3].symbols[0].clone();
            c[4].symbols.push(futures)
        }),
        ("massive lists symbol ESZ6 twice", |c| c[3].symbols[1].symbol = "ESZ6".into()),
        ("massive lists url wss://socket.massive.com/stocks twice", |c| c[3].url = c[4].url.clone()),
    ];
    for (expected, edit) in cases {
        let mut cfg = with_vendors();
        edit(&mut cfg.feeds.venues.as_mut().unwrap().connections);
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(expected), "expected {expected:?}, got {err}");
    }
}

#[test]
fn an_endpoint_without_tls_is_refused_for_every_venue() {
    let count = with_vendors().feeds.venues.unwrap().connections.len();
    for (at, scheme) in (0..count).flat_map(|at| ["ws", "tcp", "https"].map(|scheme| (at, scheme))) {
        let mut cfg = with_vendors();
        let conn = &mut cfg.feeds.venues.as_mut().unwrap().connections[at];
        conn.url = conn.url.replacen("wss", scheme, 1);
        let name = conn.venue.name();
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(&format!("{name} url must be wss://")), "{scheme} for {name}: {err}");
    }
}

// Runs in the production lane too, which filters on `production_build`.
#[test]
fn a_production_build_refuses_a_genesis_market_its_venues_cannot_price() {
    let mut cfg = with_vendors();
    cfg.feeds.provider = FeedProvider::Venues;
    // US 500 prices from futures here, through three contracts that Massive quotes two of.
    let from = DateTime::from_timestamp_millis(0).expect("in range");
    let us500 = cfg.genesis.markets.iter_mut().find(|market| market.id == 20).expect("US 500");
    us500.contracts =
        ["ESZ6", "ESH7", "ESM7"].map(|contract| FuturesContract { contract: contract.into(), from }).to_vec();
    // A dev build prices what it can.
    cfg.refuse_unmet_min_sources(false).unwrap();

    let err = cfg.refuse_unmet_min_sources(true).unwrap_err().to_string();
    // Crypto from Coinbase and Kraken, and EUR-USD from Massive and TraderMade, meet min_sources 2.
    for met in ["market 1 ", "market 2 ", "market 10 "] {
        assert!(!err.contains(met), "{met}: {err}");
    }
    for unmet in [
        "market 20 (US500) contract ESZ6 has 1 (massive) of 2",
        "market 20 (US500) contract ESH7 has 1 (massive) of 2",
        // Listed, and quoted by none.
        "market 20 (US500) contract ESM7 has no venue of 2",
        "market 23 (DE40) has no venue of 2",
        // Its symbol names a contract, GCZ6, that a market not priced from futures never takes.
        "market 30 (XAU-USD) has no venue of 2",
        "market 40 (AAPL) has 1 (massive) of 2",
    ] {
        assert!(err.contains(unmet), "{unmet}: {err}");
    }

    // A second venue on the contracts it quotes meets them, and leaves the unquoted one short.
    let tradermade = &mut cfg.feeds.venues.as_mut().unwrap().connections[5];
    for contract in ["ESZ6", "ESH7"] {
        tradermade.symbols.push(VenueSymbol { market: 20, symbol: contract.into(), contract: Some(contract.into()) });
    }
    let err = cfg.refuse_unmet_min_sources(true).unwrap_err().to_string();
    assert!(!err.contains("contract ESZ6") && !err.contains("contract ESH7"), "{err}");
    assert!(err.contains("market 20 (US500) contract ESM7 has no venue of 2"), "{err}");

    // With every market's min_sources met, nothing is refused; the mock feed's sources are no venues.
    cfg.genesis.markets.retain(|market| [1, 2, 10].contains(&market.id));
    cfg.refuse_unmet_min_sources(true).unwrap();
    cfg.feeds.provider = FeedProvider::Mock;
    let err = cfg.refuse_unmet_min_sources(true).unwrap_err().to_string();
    assert!(err.contains("market 1 (BTC-USD) has no venue of 2"), "{err}");
}

#[test]
fn a_quote_spacing_that_outlasts_the_venue_timeout_is_refused() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    let venues = cfg.feeds.venues.as_mut().unwrap();
    venues.min_quote_spacing = venues.timeout;
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("feeds.venues.min_quote_spacing must be shorter than feeds.venues.timeout"), "{err}");
}

#[test]
fn a_venue_quote_pace_over_the_flood_bound_is_refused() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.feeds.venues.as_mut().unwrap().min_quote_spacing = Duration::from_millis(5);
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("feeds.venues.min_quote_spacing must be at least 10ms"), "{err}");
}

#[test]
fn a_ping_interval_that_is_zero_or_outlasts_the_delay_bound_is_refused() {
    let edits: [fn(&mut crate::config::VenuesSection); 3] = [
        |venues| venues.ping_interval = Duration::ZERO,
        |venues| venues.ping_interval = venues.max_delay,
        |venues| venues.max_delay = venues.ping_interval / 2,
    ];
    for edit in edits {
        let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
        edit(cfg.feeds.venues.as_mut().unwrap());
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains("ping_interval must be non-zero and shorter than max_delay"), "{err}");
    }
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
fn venue_timing_paces_from_the_quote_spacing_not_the_mock_interval() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    let timing = crate::feeds::venue::Timing::for_venues(cfg.feeds.venues.as_ref().unwrap());
    assert_eq!(timing.interval, Duration::from_millis(10));
    assert_ne!(timing.interval, cfg.feeds.interval, "the mock tick and the venue pace differ");
    // The mock tick has no venue-timeout bound: the spacing alone governs venue timing.
    cfg.feeds.interval = cfg.feeds.venues.as_ref().unwrap().timeout;
    cfg.validate().expect("a mock tick past the venue timeout stays valid");
}

#[test]
fn a_cme_connection_names_its_oauth_secrets_and_each_contract_by_product_and_period() {
    let cfg = with_cme();
    cfg.validate().unwrap();
    let named = cfg.named_secrets().unwrap();
    let url = "wss://markets.api.cmegroup.com/marketdatastream/v1";
    let listed: Vec<(String, String)> =
        named.iter().map(|field| (field.field.clone(), field.name.to_string())).collect();
    let expected = [("client_id_secret", "cme_api_id"), ("client_secret_secret", "cme_api_secret")]
        .map(|(key, name)| (format!("feeds.venues.connections[{url}].oauth.{key}"), name.to_owned()));
    assert_eq!(listed, expected);
    // Without its secrets the connection stays off; the node runs.
    assert!(named.iter().all(|field| field.usage == crate::config::SecretUse::VendorKey));
}

#[test]
fn cme_settings_that_would_fail_or_could_be_forged_are_refused() {
    type Edit = fn(&mut Vec<VenueConnection>);
    let cases: [(&str, Edit); 10] = [
        ("cme oauth must be set exactly for cme", |c| c[2].oauth = None),
        ("coinbase oauth must be set exactly for cme", |c| c[0].oauth = c[2].oauth.clone()),
        ("cme api_key_env must be set exactly for a vendor", |c| c[2].api_key_env = Some("CME_KEY".into())),
        ("cme oauth.token_url must be https://", |c| {
            c[2].oauth.as_mut().unwrap().token_url = "http://auth.cmegroup.com/as/token.oauth2".into()
        }),
        ("cme oauth.token_url must be on cmegroup.com", |c| {
            c[2].oauth.as_mut().unwrap().token_url = "https://auth.example.com/as/token.oauth2".into()
        }),
        ("cme url must be on cmegroup.com", |c| c[2].url = "wss://markets.api.example.com/marketdatastream/v1".into()),
        ("cme symbol ESZ6: not <product>:<YYYYMM>", |c| c[2].symbols[0].symbol = "ESZ6".into()),
        ("cme symbol ES:202603: names ESH6, not contract ESZ6", |c| c[2].symbols[0].symbol = "ES:202603".into()),
        ("cme symbol ES:202612: a CME symbol names the contract it quotes", |c| c[2].symbols[0].contract = None),
        ("cme lists symbol ES:202612 twice", |c| c[2].symbols[1] = c[2].symbols[0].clone()),
    ];
    for (expected, edit) in cases {
        let mut cfg = with_cme();
        edit(&mut cfg.feeds.venues.as_mut().unwrap().connections);
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(expected), "expected {expected:?}, got {err}");
    }
}

// Runs in the production lane too, which filters on `production_build`: no build reads CME's
// credentials from a value in the file or the environment, only from a delivered bundle.
#[test]
fn a_production_build_reads_cme_credentials_only_as_delivered_secrets() {
    let cfg = with_cme();
    cfg.refuse_environment_secrets_at_load(true).unwrap();
    let base = "token_url = 'https://auth.cmegroup.com/as/token.oauth2'";
    for (field, value) in [("client_id_env", "'CME_API_ID'"), ("client_id", "'cme-api-id'"), ("client_secret", "'pw'")]
    {
        let oauth = format!("{base}\nclient_id_secret = 'a'\nclient_secret_secret = 'b'\n{field} = {value}");
        let err = toml::from_str::<OAuthSection>(&oauth).unwrap_err().to_string();
        assert!(err.contains(&format!("unknown field `{field}`")), "{field}: {err}");
    }
}

// ── Helpers ──

/// The committed dev config taking delivered secrets, with a CME connection after its crypto venues
/// that quotes two of US 500's contracts.
fn with_cme() -> NodeConfig {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let (head, tail) = dev.split_once("\n[time]\n").expect("dev.toml has a [time] section");
    let cme = r#"
[[feeds.venues.connections]]
venue = "cme"
url = "wss://markets.api.cmegroup.com/marketdatastream/v1"
oauth = { token_url = "https://auth.cmegroup.com/as/token.oauth2", client_id_secret = "cme_api_id", client_secret_secret = "cme_api_secret" }
symbols = [{ market = 20, symbol = "ES:202612", contract = "ESZ6" }, { market = 20, symbol = "ES:202703", contract = "ESH7" }]
"#;
    let mut cfg: NodeConfig = toml::from_str(&format!("{head}\n{cme}\n[time]\n{tail}")).unwrap();
    cfg.tee.use_published_mock_key = false;
    cfg.tee.sealing = Some(toml::from_str("dir = 'keys'\n[secrets]\nlisten = '127.0.0.1:9472'").unwrap());
    cfg
}

/// The committed dev config with `VENDORS` after its crypto venues: Coinbase and Kraken
/// at indices 0 and 1, then Massive's three feeds and TraderMade.
fn with_vendors() -> NodeConfig {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let (head, tail) = dev.split_once("\n[time]\n").expect("dev.toml has a [time] section");
    toml::from_str(&format!("{head}\n{VENDORS}\n[time]\n{tail}")).unwrap()
}
