//! The `[history.marks]` section: defaults when absent, retention bounds, and the backfill
//! endpoint's transport pinning.

use super::*;

use crate::test_support::DEV_CONFIG as DEV;

fn with_marks(section: &str) -> String {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let at = dev.find("[history]").expect("the dev config has a history section");
    let rest = &dev[at..];
    let end = rest.find("\n[").map_or(rest.len(), |n| n + 1);
    format!("{}{section}\n{}", &dev[..at], &rest[end..])
}

#[test]
fn absent_marks_runs_the_cache_with_defaults_and_no_backfill() {
    let cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    assert!(cfg.history.marks.is_none());
    assert!(cfg.validate().is_ok());
}

#[test]
fn defaults_apply_field_by_field() {
    let text = with_marks("[history.marks]\n");
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    cfg.validate().unwrap();
    let marks = cfg.history.marks.unwrap();
    assert_eq!(marks.retention_1s, default_marks_retention_1s());
    assert_eq!(marks.retention_1m, default_marks_retention_1m());
    assert_eq!(marks.retention_1h, default_marks_retention_1h());
    assert_eq!(marks.retention_1d, None);
    assert!(marks.backfill.is_none());
}

#[test]
fn a_zero_retention_or_a_zero_depth_is_refused() {
    let text = with_marks("[history.marks]\nretention_1m = \"0s\"\n");
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    assert!(cfg.validate().is_err());
    let text = with_marks(
        "[history.marks.backfill]\nenabled = true\nbase_url = \"https://api.exchange.coinbase.com\"\ndepth_1m = \"0s\"\ndepth_1h = \"1d\"\ndepth_1d = \"1d\"\n",
    );
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn the_backfill_endpoint_is_the_venues_own_over_tls() {
    let section = |base_url: &str| {
        with_marks(&format!(
            "[history.marks.backfill]\nenabled = true\nbase_url = \"{base_url}\"\ndepth_1m = \"60d\"\ndepth_1h = \"2y\"\ndepth_1d = \"2y\"\n"
        ))
    };
    for url in
        ["https://api.exchange.coinbase.com", "https://coinbase.com", "http://127.0.0.1:8080", "http://localhost:9999"]
    {
        let cfg: NodeConfig = toml::from_str(&section(url)).unwrap();
        cfg.validate().unwrap_or_else(|e| panic!("{url}: {e:#}"));
    }
    for url in [
        "http://api.exchange.coinbase.com",
        "https://candles.example.com",
        "https://notcoinbase.com",
        "ftp://api.exchange.coinbase.com",
        "not a url",
    ] {
        let cfg: NodeConfig = toml::from_str(&section(url)).unwrap();
        assert!(cfg.validate().is_err(), "{url} validated");
    }
}

#[test]
fn a_production_build_refuses_a_plaintext_backfill_endpoint() {
    let cfg: NodeConfig = toml::from_str(&with_marks(
        "[history.marks.backfill]\nenabled = true\nbase_url = \"http://127.0.0.1:8080\"\ndepth_1m = \"60d\"\ndepth_1h = \"2y\"\ndepth_1d = \"2y\"\n",
    ))
    .unwrap();
    assert!(cfg.history.marks.as_ref().unwrap().backfill.as_ref().unwrap().validate(true).is_err());
    assert!(cfg.history.marks.as_ref().unwrap().validate(true).is_err());
}
