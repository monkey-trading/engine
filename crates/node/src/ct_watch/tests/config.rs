use super::*;

use crate::ct_watch::config::WatchConfig;
use crate::test_support::scratch_dir;

const EXAMPLE: &str = include_str!("../../../../../config/ct-watch.toml");

fn parse(text: &str) -> WatchConfig {
    toml::from_str(text).unwrap()
}

#[test]
fn the_committed_example_loads() {
    let cfg = parse(EXAMPLE);
    cfg.validate(false).unwrap();
    assert_eq!(cfg.domain, "api.example.com");
}

#[test]
fn the_config_with_only_its_required_keys_loads_with_the_documented_defaults() {
    let minimum = r#"
domain = "api.example.com"
state_file = "state.json"
attestation_interval = "1m"
log_interval = "1m"
[node]
url = "https://api.example.com"
release = "release.toml"
processor = "Milan"
[[logs]]
name = "log"
url = "https://ct.example.com/log"
public_key = "KEY"
[logging]
level = "info"
[metrics]
listen = "127.0.0.1:9465"
[alerts]
episode_quiet = "1h"
[alerts.outbox]
dir = "alerts"
max_pages = 10
max_bytes = 1000
max_age = "1h"
"#
    .replace("KEY", &TestLogKey::new().spki_b64());
    let cfg = parse(&minimum);
    cfg.validate(false).unwrap();
    assert_eq!(cfg.grace, std::time::Duration::from_secs(600));
    assert_eq!(cfg.log_stale_after, std::time::Duration::from_secs(3600));
    assert_eq!(cfg.node.kds, crate::KDS_BASE_URL);
    assert!(cfg.logs[0].start_index.is_none() && cfg.node.root.is_none());
}

/// The example with `from` replaced by `to`, which must differ and occur.
fn edited(from: &str, to: &str) -> WatchConfig {
    assert!(EXAMPLE.contains(from), "{from}");
    parse(&EXAMPLE.replacen(from, to, 1))
}

#[test]
fn values_that_could_fail_silently_or_unsafely_are_refused() {
    for (from, to, why) in [
        ("domain = \"api.example.com\"", "domain = \"API.example.com\"", "uppercase domain"),
        ("domain = \"api.example.com\"", "domain = \"*.example.com\"", "wildcard domain"),
        ("domain = \"api.example.com\"", "domain = \"api..example.com\"", "empty label"),
        ("attestation_interval = \"5m\"", "attestation_interval = \"0s\"", "zero interval"),
        ("attestation_interval = \"5m\"", "attestation_interval = \"6m\"", "interval over half the grace"),
        ("log_stale_after = \"1h\"", "log_stale_after = \"30s\"", "stale before the next poll"),
        ("url = \"https://api.example.com\"", "url = \"http://api.example.com\"", "plain http node"),
        ("url = \"https://ct.example.com/log\"", "url = \"http://ct.example.com/log\"", "plain http log"),
        ("url = \"https://ct.example.com/log\"", "url = \"https://u:p@ct.example.com/log\"", "credentials"),
        ("name = \"example-log\"", "name = \"Example Log\"", "log name"),
        ("max_pages = 1000", "max_pages = 0", "empty outbox"),
        ("episode_quiet = \"1h\"", "episode_quiet = \"1m\"", "a quiet interval no longer than a poll"),
    ] {
        assert!(edited(from, to).validate(false).is_err(), "{why}");
    }
    let key = TestLogKey::new().spki_b64();
    let example_key =
        || EXAMPLE.lines().find(|line| line.starts_with("public_key")).unwrap().split('"').nth(1).unwrap().to_owned();
    let key_line = EXAMPLE.lines().find(|line| line.starts_with("public_key")).unwrap();
    assert!(edited(key_line, "public_key = \"AAAA\"").validate(false).is_err(), "bad log key");
    let twice =
        format!("{EXAMPLE}\n[[logs]]\nname = \"example-log\"\nurl = \"https://x.example\"\npublic_key = \"{key}\"\n");
    assert!(parse(&twice).validate(false).is_err(), "duplicate log name");
    let same_key = format!(
        "{EXAMPLE}\n[[logs]]\nname = \"mirror\"\nurl = \"https://x.example\"\npublic_key = \"{}\"\n",
        example_key()
    );
    assert!(parse(&same_key).validate(false).is_err(), "one key under two names");
}

#[test]
fn plain_http_to_loopback_is_for_mock_runs_only() {
    let loopback = edited("url = \"https://api.example.com\"", "url = \"http://127.0.0.1:8080\"");
    assert!(loopback.validate(false).is_err());
    loopback.validate(true).unwrap();
    let remote = edited("url = \"https://api.example.com\"", "url = \"http://203.0.113.9:8080\"");
    assert!(remote.validate(true).is_err(), "only loopback");
}

#[test]
fn unknown_keys_fail_the_load() {
    let dir = scratch_dir("ct-config-unknown");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("watch.toml");
    std::fs::write(&path, format!("grce = \"1m\"\n{EXAMPLE}")).unwrap();
    assert!(WatchConfig::load(&path, false).is_err());
    std::fs::write(&path, EXAMPLE).unwrap();
    assert!(WatchConfig::load(&path, false).is_ok());
}
