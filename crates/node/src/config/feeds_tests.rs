use super::*;

use crate::test_support::DEV_CONFIG as DEV;

/// The vendor connections that price the dev genesis' FX, index, commodity, and share markets from
/// two vendors each, as a production `[feeds.venues]` lists them.
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

[[feeds.venues.connections]]
venue = "databento"
url = "tcp://glbx-mdp3.lsg.databento.com:13000"
api_key_env = "DATABENTO_API_KEY"
dataset = "GLBX.MDP3"
symbols = [
  { market = 20, symbol = "ESZ6", contract = "ESZ6" },
  { market = 20, symbol = "ESH7", contract = "ESH7" },
  { market = 30, symbol = "GCZ6", contract = "GCZ6" },
]

[[feeds.venues.connections]]
venue = "databento"
url = "tcp://equs-mini.lsg.databento.com:13000"
api_key_env = "DATABENTO_API_KEY"
dataset = "EQUS.MINI"
symbols = [{ market = 40, symbol = "AAPL" }]
"#;

#[test]
fn vendor_connections_with_their_keys_datasets_and_contracts_load() {
    let cfg = with_vendors();
    cfg.validate().unwrap();
    let connections = &cfg.feeds.venues.as_ref().unwrap().connections;
    let vendors: Vec<Venue> = connections.iter().map(|c| c.venue).filter(|v| v.needs_key()).collect();
    use Venue::{Databento, Massive, Tradermade};
    assert_eq!(vendors, [Massive, Massive, Massive, Tradermade, Databento, Databento]);
    // Each key comes from the variable its connection names, and never reaches a log.
    let env = |var: &str| (var == "TRADERMADE_API_KEY").then(|| "tradermade-secret".to_owned());
    let key = connections[6].api_key(env).unwrap().expect("a vendor key");
    assert_eq!(key.expose(), "tradermade-secret");
    assert_eq!(format!("{key:?}"), "ApiKey(<redacted>)");
    assert!(connections[0].api_key(env).unwrap().is_none());
    let unset = format!("{:#}", connections[3].api_key(env).unwrap_err());
    assert!(unset.contains("massive api_key_env") && unset.contains("MASSIVE_API_KEY is not set"), "{unset}");
    let empty = format!("{:#}", connections[3].api_key(|_| Some(String::new())).unwrap_err());
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
    let cases: [(&str, Edit); 11] = [
        ("massive api_key_env must be set", |c| c[3].api_key_env = None),
        ("coinbase api_key_env must be set", |c| c[0].api_key_env = c[3].api_key_env.clone()),
        ("massive url must be one of its", |c| c[3].url = "wss://socket.massive.com/crypto".into()),
        ("tradermade url must be on tradermade.com", |c| c[6].url = "wss://stream.example.com/feedAdv".into()),
        ("databento url must be tcp://", |c| c[7].url = "wss://glbx-mdp3.lsg.databento.com:13000".into()),
        ("databento url must name the gateway port", |c| c[7].url = "tcp://glbx-mdp3.lsg.databento.com".into()),
        ("databento dataset must be set exactly for databento", |c| c[7].dataset = None),
        ("massive dataset must be set exactly for databento", |c| c[3].dataset = Some("GLBX.MDP3".into())),
        ("massive lists market 20 contract ESZ6 twice", |c| {
            let futures = c[4].symbols[0].clone();
            c[5].symbols.push(futures)
        }),
        ("databento lists symbol ESZ6 twice", |c| c[7].symbols[1].symbol = "ESZ6".into()),
        ("massive lists url wss://socket.massive.com/stocks twice", |c| c[4].url = c[5].url.clone()),
    ];
    for (expected, edit) in cases {
        let mut cfg = with_vendors();
        edit(&mut cfg.feeds.venues.as_mut().unwrap().connections);
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(expected), "expected {expected:?}, got {err}");
    }
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
    let venues = cfg.feeds.venues.as_ref().unwrap();
    let timing = crate::feeds::venue::Timing::new(venues.min_quote_spacing, venues);
    assert_eq!(timing.interval, Duration::from_millis(10));
    assert_ne!(timing.interval, cfg.feeds.interval, "the mock tick and the venue pace differ");
    // The mock tick has no venue-timeout bound: the spacing alone governs venue timing.
    cfg.feeds.interval = venues.timeout;
    cfg.validate().expect("a mock tick past the venue timeout stays valid");
}

// ── Helpers ──

/// The committed dev config with `VENDORS` after its crypto venues: Coinbase, Kraken, and Bitstamp
/// at indices 0 to 2, then Massive's three feeds, TraderMade, and Databento's two datasets.
fn with_vendors() -> NodeConfig {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let (head, tail) = dev.split_once("\n[time]\n").expect("dev.toml has a [time] section");
    toml::from_str(&format!("{head}\n{VENDORS}\n[time]\n{tail}")).unwrap()
}
