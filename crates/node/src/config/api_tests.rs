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
