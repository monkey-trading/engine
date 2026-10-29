//! `api.allowed_origins` validation.

use super::*;

use crate::test_support::DEV_CONFIG as DEV;

fn with_origins(origins: &[&str]) -> NodeConfig {
    let mut cfg: NodeConfig = toml::from_str(&std::fs::read_to_string(DEV).unwrap()).unwrap();
    cfg.api.allowed_origins = origins.iter().map(|o| (*o).to_owned()).collect();
    cfg
}

#[test]
fn no_origin_is_allowed_by_default() {
    let text = std::fs::read_to_string(DEV).unwrap();
    let text: String = text.lines().filter(|l| !l.starts_with("allowed_origins")).map(|l| format!("{l}\n")).collect();
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    assert!(cfg.api.allowed_origins.is_empty());
    cfg.validate().unwrap();
}

#[test]
fn the_dev_configs_list_the_consoles_dev_origin() {
    for path in [DEV, "../../config/anvil.toml"] {
        let cfg: NodeConfig = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(cfg.api.allowed_origins, ["http://localhost:3000"], "{path}");
        cfg.validate().unwrap();
    }
}

#[test]
fn exact_origins_are_accepted() {
    with_origins(&[
        "http://localhost:3000",
        "https://console.example",
        "https://app.example:8443",
        "http://[::1]:3000",
    ])
    .validate()
    .unwrap();
}

#[test]
fn a_wildcard_path_duplicate_or_unsendable_origin_is_refused() {
    for origin in [
        "*",
        "https://*.example",
        "https://console.example/",
        "https://console.example/app",
        "https://console.example?x=1",
        "https://user@console.example",
        "console.example",
        "ftp://console.example",
        "https://",
        "https://Console.example",
        "https://console.example:443",
        "http://console.example:80",
        "https://console.example:0",
        "https://console.example:99999",
        "https://console.example:08443",
        "https://console.example:+8443",
        "http://::1",
        "http://[::1:3000",
        "http://127.1:3000",
        "https://console.example#frag",
        "https://console.example\t",
        "https://console.example:",
        "https://console.example ",
        "null",
        "",
    ] {
        assert!(with_origins(&[origin]).validate().is_err(), "{origin:?}");
    }
    let err = with_origins(&["https://a.example", "https://a.example"]).validate().unwrap_err();
    assert!(format!("{err:#}").contains("twice"), "{err:#}");
}

#[test]
fn a_production_build_takes_only_https_origins() {
    let cfg = with_origins(&["https://console.example", "http://localhost:3000"]);
    cfg.validate().unwrap();
    cfg.api.refuse_insecure_origins(false).unwrap();
    let err = cfg.api.refuse_insecure_origins(true).unwrap_err();
    assert!(format!("{err:#}").contains("http://localhost:3000"), "{err:#}");
    with_origins(&["https://console.example"]).api.refuse_insecure_origins(true).unwrap();
}

#[test]
fn an_unknown_key_beside_allowed_origins_is_still_refused() {
    let text = std::fs::read_to_string(DEV)
        .unwrap()
        .replace("max_subscribers = 256", "max_subscribers = 256\nallowed_origin = []");
    assert!(toml::from_str::<NodeConfig>(&text).is_err());
}
