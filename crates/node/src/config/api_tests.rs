use super::*;

use crate::test_support::DEV_CONFIG as DEV;

#[test]
fn a_signed_stream_heartbeat_is_five_seconds_unless_set_and_never_under_one() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let interval = |text: &str| {
        let cfg: NodeConfig = toml::from_str(text).unwrap();
        cfg.validate().map(|()| cfg.api.heartbeat_interval)
    };
    let unset = dev.replace("heartbeat_interval = \"5s\"\n", "");
    assert!(!unset.contains("heartbeat_interval"));
    assert_eq!(interval(&unset).unwrap(), Duration::from_secs(5));
    let set = |value: &str| dev.replace("heartbeat_interval = \"5s\"", &format!("heartbeat_interval = \"{value}\""));
    assert_eq!(interval(&set("1s")).unwrap(), Duration::from_secs(1));
    for short in ["999ms", "0s"] {
        let err = interval(&set(short)).unwrap_err();
        assert!(format!("{err:#}").contains("api.heartbeat_interval must be at least 1s"), "{short}: {err:#}");
    }
}

#[test]
fn quote_pushes_are_a_tenth_of_a_second_apart_unless_set_and_never_closer() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let api = |text: &str| {
        let cfg: NodeConfig = toml::from_str(text).unwrap();
        cfg.validate().map(|()| (cfg.api.quote_interval, cfg.api.quote_max_markets.get()))
    };
    let unset = dev.replace("quote_interval = \"100ms\"\n", "").replace("quote_max_markets = 4\n", "");
    assert!(!unset.contains("quote_interval") && !unset.contains("quote_max_markets"), "the dev config sets both");
    assert_eq!(api(&unset).unwrap(), (Duration::from_millis(100), 4));
    let set = |value: &str| dev.replace("quote_interval = \"100ms\"", &format!("quote_interval = \"{value}\""));
    assert_eq!(api(&set("250ms")).unwrap().0, Duration::from_millis(250));
    for short in ["99ms", "0s"] {
        let err = api(&set(short)).unwrap_err();
        assert!(format!("{err:#}").contains("api.quote_interval must be at least 100ms"), "{short}: {err:#}");
    }
    let none = dev.replace("quote_max_markets = 4", "quote_max_markets = 0");
    assert!(toml::from_str::<NodeConfig>(&none).is_err());
}

#[test]
fn stats_scans_are_a_minute_apart_unless_set_and_never_closer_than_a_second() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let interval = |text: &str| {
        let cfg: NodeConfig = toml::from_str(text).unwrap();
        cfg.validate().map(|()| cfg.api.stats_interval)
    };
    let unset = dev.replace("stats_interval = \"60s\"\n", "");
    assert!(!unset.contains("stats_interval = "), "the dev config sets it");
    assert_eq!(interval(&unset).unwrap(), Duration::from_secs(60));
    let set = |value: &str| dev.replace("stats_interval = \"60s\"", &format!("stats_interval = \"{value}\""));
    assert_eq!(interval(&set("1s")).unwrap(), Duration::from_secs(1));
    for short in ["999ms", "0s"] {
        let err = interval(&set(short)).unwrap_err();
        assert!(format!("{err:#}").contains("api.stats_interval must be at least 1s"), "{short}: {err:#}");
    }
}

#[test]
fn the_stats_section_defaults_refuses_unknown_keys_and_unbounded_maps() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let with = |section: &str| format!("{dev}\n[stats]\n{section}\n");
    let load = |text: &str| {
        toml::from_str::<NodeConfig>(text).map_err(anyhow::Error::from).and_then(|cfg| {
            cfg.validate()?;
            Ok(cfg.stats)
        })
    };
    let defaults = load(&dev).unwrap();
    assert_eq!(
        (defaults.retention_1m, defaults.retention_1h, defaults.retention_1d),
        (Duration::from_secs(48 * 3600), Duration::from_secs(90 * 86_400), None)
    );
    assert_eq!(load(&with("retention_1d = \"730d\"")).unwrap().retention_1d, Some(Duration::from_secs(730 * 86_400)));
    assert!(toml::from_str::<NodeConfig>(&with("retention_1w = \"1d\"")).is_err(), "an unknown key is refused");
    for (bad, why) in [
        ("retention_1m = \"0s\"", "stats.retention_1m must be at least one millisecond"),
        ("retention_1m = \"30s\"", "stats.retention_1m must cover at least one bucket"),
        ("retention_1d = \"1h\"", "stats.retention_1d must cover at least one bucket"),
        ("retention_1h = \"2h\"", "stats.retention_1h must cover the 24 hours"),
        ("activity_limit = 0", "stats.activity_limit"),
        ("closeout_bucket_bps = 0", "must be positive"),
        ("closeout_range_bps = 100\ncloseout_bucket_bps = 200", "must not exceed"),
        ("closeout_range_bps = 5000\ncloseout_bucket_bps = 1", "more than 400 buckets"),
        ("closeout_range_bps = 10000", "a fall to zero"),
        ("closeout_range_bps = 9000\ncloseout_bucket_bps = 6000", "a fall to zero"),
        ("profit_range_bps = 4294967295\nprofit_bucket_bps = 4294967294", "past 1000000"),
    ] {
        let err = load(&with(bad)).unwrap_err();
        assert!(format!("{err:#}").contains(why), "{bad}: {err:#}");
    }
}

#[test]
fn light_stats_scans_are_five_seconds_apart_unless_set_and_never_closer_than_a_second() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let interval = |text: &str| {
        let cfg: NodeConfig = toml::from_str(text).unwrap();
        cfg.validate().map(|()| cfg.api.stats_light_interval)
    };
    let unset = dev.replace("stats_light_interval = \"5s\"\n", "");
    assert!(!unset.contains("stats_light_interval = "), "the dev config sets it");
    assert_eq!(interval(&unset).unwrap(), Duration::from_secs(5));
    let set =
        |value: &str| dev.replace("stats_light_interval = \"5s\"", &format!("stats_light_interval = \"{value}\""));
    assert_eq!(interval(&set("1s")).unwrap(), Duration::from_secs(1));
    assert_eq!(interval(&set("10m")).unwrap(), Duration::from_secs(600), "above the full interval is allowed");
    for short in ["999ms", "0s"] {
        let err = interval(&set(short)).unwrap_err();
        assert!(format!("{err:#}").contains("api.stats_light_interval must be at least 1s"), "{short}: {err:#}");
    }
}

#[test]
fn a_clients_fresh_account_share_must_be_below_the_shared_budget_in_rate_and_burst() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let load = |text: &str| {
        let cfg: NodeConfig = toml::from_str(text).unwrap();
        cfg.validate().map(|()| cfg.api.fresh_accounts)
    };
    let fresh = load(&dev).unwrap();
    assert_eq!((fresh.shared.per_minute.get(), fresh.per_client.burst.get()), (3000, 20));
    for (from, to) in [
        ("per_client = { per_minute = 600, burst = 20 }", "per_client = { per_minute = 3000, burst = 20 }"),
        ("per_client = { per_minute = 600, burst = 20 }", "per_client = { per_minute = 600, burst = 100 }"),
        ("shared = { per_minute = 3000, burst = 100 }", "shared = { per_minute = 600, burst = 100 }"),
    ] {
        let err = load(&dev.replace(from, to)).unwrap_err();
        assert!(format!("{err:#}").contains("api.fresh_accounts.per_client must be below"), "{to}: {err:#}");
    }
    // The share may go below one a second, and a budget needs both keys.
    assert!(load(&dev.replace("per_minute = 600, burst = 20", "per_minute = 12, burst = 6")).is_ok());
    assert!(toml::from_str::<NodeConfig>(&dev.replace("per_minute = 600, ", "")).is_err());
    assert!(toml::from_str::<NodeConfig>(&dev.replace("[api.fresh_accounts]", "[api.fresh]")).is_err());
}
