//! Tests of the metrics and alert config: the Critical route a node off the mocks needs, the dev
//! config's telemetry, and webhook URLs.

use super::*;

use crate::test_support::{ANVIL_CONFIG as ANVIL, DEV_CONFIG as DEV, own_operator_key};

/// The Critical route section a non-mock config needs.
pub(super) const CRITICAL_ROUTE: &str = "\n[alerts.critical]\nurl_env = \"PAGER\"\n";

/// A node beside a real enclave or real funds refuses to start without a Critical route; a mock
/// node may run without one.
#[test]
fn only_a_mock_node_may_run_without_a_critical_route() {
    let dev = std::fs::read_to_string(DEV).unwrap().replace("auth = \"none\"", "auth = \"eip712\"");
    let anvil = std::fs::read_to_string(ANVIL).unwrap();
    let evm = anvil.replace("[alerts.critical]\nurl_env = \"ENGINE_ALERTS_CRITICAL_URL\"\n", "");
    let tee = |text: &str| text.replace("[tee]\nprovider = \"mock\"", "[tee]\nprovider = \"configfs_tsm\"");
    // (config, needs a route): the dev config, with a real TEE, with EVM custody, and with both.
    let rows = [(dev.clone(), false), (tee(&dev), true), (evm.clone(), true), (tee(&evm), true)];
    for (text, _) in &rows[1..] {
        assert!(text != &dev && text != &anvil, "each replacement must change the config");
    }
    // Real attestation also refuses the dev configs' published operator key.
    let parse = |text: &str| {
        let mut cfg: NodeConfig = toml::from_str(text).unwrap();
        cfg.operator.public_key = own_operator_key();
        cfg.tee.use_published_mock_key = false;
        if cfg.custody.provider == crate::config::CustodyProvider::Evm
            && cfg.tee.provider != crate::config::TeeProvider::Mock
        {
            let evm = cfg.custody.evm.as_mut().expect("EVM fixture");
            evm.finality_tag = crate::config::FinalityTag::Latest;
            evm.withdrawal_finality_tag = crate::config::FinalityTag::Finalized;
            cfg.checkpoints = Some(toml::from_str("rpc_url = 'http://127.0.0.1:8545'\nrelayer = '0x1111111111111111111111111111111111111111'\nretry_interval = '2s'\nrpc_timeout = '10s'").unwrap());
        }
        cfg
    };
    for (text, needs_route) in rows {
        let cfg = parse(&text);
        let unrouted = cfg.validate();
        assert_eq!(unrouted.is_err(), needs_route, "{:?} {:?}", cfg.tee.provider, cfg.custody.provider);
        if let Err(err) = unrouted {
            assert!(err.to_string().contains("[alerts.critical] is required"), "{err:#}");
        }
        parse(&format!("{text}{CRITICAL_ROUTE}")).validate().unwrap();
    }
}

#[test]
fn the_dev_config_serves_metrics_and_only_logs_alerts() {
    let cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    assert_eq!(cfg.metrics.listen, "127.0.0.1:9464".parse().unwrap());
    assert_eq!(cfg.alerts.no_price_after, Duration::from_secs(5));
    assert!(cfg.alerts.critical.is_none() && cfg.alerts.warning.is_none());
    assert!(cfg.feeds.mock_mids.iter().all(|m| m.stop_after.is_none()));
}

#[test]
fn alert_routes_and_a_mock_stop_parse_and_unknown_alert_keys_fail() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let text = format!("{dev}\n[alerts.critical]\nurl_env = \"PAGER\"\n")
        .replace("{ market = 1, mid = \"100000\" }", "{ market = 1, mid = \"100000\", stop_after = \"30s\" }");
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    assert_eq!(cfg.alerts.critical.unwrap().url_env, "PAGER");
    assert_eq!(cfg.feeds.mock_mids[0].stop_after, Some(Duration::from_secs(30)));
    // A misspelt route would otherwise leave Critical alerts unrouted without a word.
    let text = format!("{dev}\n[alerts.critcal]\nurl_env = \"PAGER\"\n");
    assert!(toml::from_str::<NodeConfig>(&text).is_err());
}

#[test]
fn a_webhook_url_comes_from_its_variable_and_never_prints() {
    let env = |value: &'static str| move |var: &str| (var == "PAGER").then(|| value.to_owned());
    let url = WebhookUrl::from_env("PAGER", env("https://hooks.example/v1/secret-token")).unwrap();
    assert_eq!(url.expose().as_str(), "https://hooks.example/v1/secret-token");
    assert_eq!(format!("{url:?}"), "WebhookUrl(<redacted>)");
    for local in ["http://127.0.0.1:9000/hook", "http://[::1]:9000/hook"] {
        assert!(WebhookUrl::from_env("PAGER", env(local)).is_ok(), "{local}");
    }

    let err = WebhookUrl::from_env("PAGER", |_| None).unwrap_err();
    assert_eq!(err.to_string(), "environment variable PAGER is not set");
    for bad in ["http://hooks.example/secret-token", "ftp://127.0.0.1/secret-token", "secret-token"] {
        let err = WebhookUrl::from_env("PAGER", env(bad)).unwrap_err();
        assert!(!err.to_string().contains("secret-token"), "{err}");
    }
}

#[test]
fn outbox_bounds_are_nonzero_and_its_directory_is_beside_the_public_log() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    for dir in ["data/dev/log", "data/dev/log/alerts", "data/elsewhere/alerts", "data/dev/../alerts"] {
        cfg.alerts.outbox.dir = dir.into();
        assert!(cfg.alerts.outbox.validate_location(&cfg.node.log_dir).is_err(), "{dir}");
    }
    cfg.alerts.outbox.dir = "data/dev/alerts".into();
    cfg.alerts.outbox.validate_location(&cfg.node.log_dir).unwrap();
    cfg.alerts.outbox.max_pages = 0;
    assert!(cfg.alerts.outbox.validate().unwrap_err().to_string().contains("max_pages"));
    cfg.alerts.outbox.max_pages = 1;
    cfg.alerts.outbox.max_bytes = 0;
    assert!(cfg.alerts.outbox.validate().unwrap_err().to_string().contains("max_bytes"));
    cfg.alerts.outbox.max_bytes = 1;
    cfg.alerts.outbox.max_age = Duration::ZERO;
    assert!(cfg.alerts.outbox.validate().unwrap_err().to_string().contains("max_age"));
}

#[test]
fn an_outbox_symlink_into_the_public_log_is_refused() {
    let dir = crate::test_support::scratch_dir("outbox-symlink");
    std::fs::create_dir_all(dir.join("log")).unwrap();
    std::os::unix::fs::symlink(dir.join("log"), dir.join("alerts")).unwrap();
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.alerts.outbox.dir = dir.join("alerts");
    assert!(cfg.alerts.outbox.check_disk_location(&dir.join("log")).unwrap_err().to_string().contains("overlaps"));
}
