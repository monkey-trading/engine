//! How a config names secrets: what a URL's `{name}` may change, which fields read what, and what a
//! production build refuses.

use super::*;

use crate::config::WebhookRoute;
use crate::test_support::dev_config;

fn url(text: &str) -> SecretUrl {
    #[derive(Deserialize)]
    struct Holder {
        url: SecretUrl,
    }
    toml::from_str::<Holder>(&format!("url = {text:?}")).unwrap().url
}

fn values(pairs: &[(&str, &str)]) -> Values {
    pairs.iter().map(|(name, value)| (name_of(name), SecretValue::test(value))).collect()
}

fn name_of(name: &str) -> SecretName {
    SecretName::try_from(name.to_owned()).unwrap()
}

#[test]
fn a_literal_url_stays_as_written_and_names_no_secret() {
    let literal = url("https://rpc.example/v2/written-out");
    assert_eq!(literal.secret_name().unwrap(), None);
    assert_eq!(literal.expose().unwrap().as_str(), "https://rpc.example/v2/written-out");
    assert_eq!(literal.attested().unwrap().as_str(), "https://rpc.example/v2/written-out");
}

#[test]
fn a_placeholder_reads_its_key_only_once_it_is_delivered() {
    let mut named = url("https://rpc.example/v2/{alpha_key}?chain=arb");
    assert_eq!(named.secret_name().unwrap(), Some(name_of("alpha_key")));
    let err = named.expose().unwrap_err().to_string();
    assert!(err.contains("alpha_key") && err.contains("not been delivered"), "{err}");
    // The checks a config load makes need no key.
    assert_eq!(named.attested().unwrap().host_str(), Some("rpc.example"));

    named.deliver(&values(&[("other", "not-this")])).unwrap();
    assert!(named.expose().is_err(), "a value for another name was used");
    named.deliver(&values(&[("alpha_key", "FAKE-key-1")])).unwrap();
    assert_eq!(named.expose().unwrap().as_str(), "https://rpc.example/v2/FAKE-key-1?chain=arb");
    assert_eq!(format!("{named:?}"), "SecretUrl(<redacted>)");
}

// Whatever a bundle holds, the provider asked is the one the attested config names.
#[test]
fn no_delivered_value_changes_which_server_is_asked() {
    for template in
        ["https://rpc.example:8443/v2/{key}", "https://rpc.example/v2?auth={key}&x=1", "http://127.0.0.1:9/{key}"]
    {
        let attested = url(template).attested().unwrap();
        for value in [
            "/../../evil",
            "@evil.test",
            "evil.test/",
            "?host=evil",
            "#fragment",
            "%2f%2fevil.test",
            "..%2f..",
            "a b\tc\nd",
            "\\evil.test",
            "x:y@evil.test:1/",
            "{key}",
        ] {
            let mut delivered = url(template);
            delivered.key = Some(SecretValue::test(value));
            let exposed = delivered.expose().unwrap();
            let origin = (exposed.scheme(), exposed.host_str(), exposed.port());
            assert_eq!(origin, (attested.scheme(), attested.host_str(), attested.port()), "{template} with {value}");
            assert_eq!(exposed.path().matches('/').count(), attested.path().matches('/').count(), "{value}");
            assert_eq!(exposed.username(), "", "{value}");
            assert_eq!(exposed.password(), None, "{value}");
            assert_eq!(exposed.fragment(), None, "{value}");
        }
    }
}

#[test]
fn a_placeholder_outside_the_path_or_query_is_refused() {
    for (template, why) in [
        ("https://{key}.rpc.example/v2", "must sit in a URL's path or query"),
        ("https://rpc.{key}/v2", "must sit in a URL's path or query"),
        ("https://{key}@rpc.example/v2", "must sit in a URL's path or query"),
        ("https://user:{key}@rpc.example/v2", "must sit in a URL's path or query"),
        ("https://rpc.example:{key}/v2", "must sit in a URL's path or query"),
        ("{key}://rpc.example/v2", "must sit in a URL's path or query"),
        ("https://rpc.example/v2#{key}", "must sit in a URL's path or query"),
        // A URL parser skips extra slashes and takes a backslash for one, so these reach the host.
        ("https:///{key}", "must sit in a URL's path or query"),
        ("https:///{key}.provider.example/v2", "must sit in a URL's path or query"),
        ("https:////{key}", "must sit in a URL's path or query"),
        ("https://\\/{key}", "must sit in a URL's path or query"),
        ("https:\\\\{key}/v2", "must sit in a URL's path or query"),
        ("https:///user:{key}@rpc.example/v2", "must sit in a URL's path or query"),
        ("https:///{key}@rpc.example/v2", "must sit in a URL's path or query"),
        ("https://rpc.example/{key}/{other}", "one secret"),
        ("https://rpc.example/{key", "not closed"),
        ("https://rpc.example/key}", "closing brace"),
        ("https://rpc.example/{Bad-Name}", "lowercase"),
        ("https://rpc.example/{}", "lowercase"),
    ] {
        let err = format!("{:#}", url(template).attested().unwrap_err());
        assert!(err.contains(why), "{template}: {err}");
    }
}

#[test]
fn an_error_never_quotes_the_url() {
    for template in ["not a url with credential-in-text", "https://rpc.example/credential-in-text{"] {
        let err = format!("{:#}", url(template).attested().unwrap_err());
        assert!(!err.contains("credential-in-text"), "{err}");
    }
}

#[test]
fn a_secret_name_is_lowercase_words_and_bounded() {
    for good in ["a", "custody_alpha_key", "key2", &"a".repeat(MAX_NAME_BYTES)] {
        assert!(SecretName::try_from(good.to_string()).is_ok(), "{good}");
    }
    for bad in ["", "Upper", "with-dash", "with space", "dot.ted", &"a".repeat(MAX_NAME_BYTES + 1)] {
        let err = SecretName::try_from(bad.to_string()).unwrap_err();
        assert!(!err.contains(bad) || bad.is_empty(), "the error quotes its input: {err}");
    }
}

#[test]
fn a_value_is_printable_bounded_and_more_than_dots() {
    let name = name_of("key");
    SecretValue::test("FAKE-key_1.2~3").check(&name).unwrap();
    SecretValue::test(&"k".repeat(MAX_VALUE_BYTES)).check(&name).unwrap();
    for (value, why) in [
        (String::new(), "empty"),
        ("k".repeat(MAX_VALUE_BYTES + 1), "over"),
        ("FAKE-key\n".to_owned(), "printable"),
        ("FAKE key".to_owned(), "printable"),
        ("FAKE-kéy".to_owned(), "printable"),
        (".".to_owned(), "dots"),
        ("..".to_owned(), "dots"),
    ] {
        let err = SecretValue::test(&value).check(&name).unwrap_err().to_string();
        assert!(err.contains(why) && err.contains("key"), "{value:?}: {err}");
        assert!(value.is_empty() || !err.contains(&value), "the error quotes the value: {err}");
    }
}

#[test]
fn a_value_prints_as_redacted_and_still_round_trips() {
    let value = SecretValue::test("FAKE-key");
    assert_eq!(format!("{value:?}"), "SecretValue(<redacted>)");
    assert_eq!(format!("{value}"), "<redacted>");
    let restored: SecretValue = serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
    assert_eq!(restored, value);
}

/// `cfg` with one vendor connection, whose key the secret `name` holds.
fn with_vendor(mut cfg: NodeConfig, name: &str) -> NodeConfig {
    cfg.feeds.venues = Some(
        toml::from_str(&format!(
            "timeout = '10s'\nreconnect_min = '1s'\nreconnect_max = '10s'\nstale_after = '10s'\n\
             min_quote_spacing = '100ms'\n[[connections]]\nvenue = 'massive'\n\
             url = 'wss://socket.massive.com/forex'\napi_key_secret = '{name}'\n\
             symbols = [{{ market = 1, symbol = 'C.EURUSD' }}]\n"
        ))
        .unwrap(),
    );
    cfg
}

/// A dev config that takes delivered secrets, naming one of each kind.
fn naming_each_kind() -> NodeConfig {
    let mut cfg = with_vendor(dev_config(), "massive_key");
    cfg.tee.use_published_mock_key = false;
    cfg.tee.sealing = Some(toml::from_str("dir = 'keys'\n[secrets]\nlisten = '127.0.0.1:9472'").unwrap());
    cfg.checkpoints = Some(
        toml::from_str(
            "rpc_url = 'https://rpc.example/v2/{chain_key}'\nrelayer = '0x0000000000000000000000000000000000000001'\n\
             retry_interval = '1s'\nrpc_timeout = '1s'",
        )
        .unwrap(),
    );
    cfg.alerts.critical = Some(WebhookRoute { url_env: None, url_secret: Some(name_of("critical_url")) });
    cfg
}

#[test]
fn every_field_that_reads_a_secret_is_listed_with_what_its_absence_does() {
    let named = naming_each_kind().named_secrets().unwrap();
    let listed: Vec<(&str, SecretUse)> = named.iter().map(|field| (field.field.as_str(), field.usage)).collect();
    assert_eq!(listed[0], ("checkpoints.rpc_url", SecretUse::Required));
    assert!(listed.iter().any(|(field, usage)| field.contains("api_key_secret") && *usage == SecretUse::VendorKey));
    assert!(listed.contains(&("alerts.critical.url_secret", SecretUse::WebhookUrl)));
    let names: Vec<String> = named.iter().map(|field| field.name.to_string()).collect();
    assert_eq!(names, ["chain_key", "massive_key", "critical_url"]);
}

#[test]
fn a_config_that_names_a_secret_needs_a_listener_to_receive_it() {
    let cfg = naming_each_kind();
    cfg.validate_secrets().unwrap();

    let mut no_listener = naming_each_kind();
    no_listener.tee.sealing = Some(toml::from_str("dir = 'keys'").unwrap());
    let err = no_listener.validate_secrets().unwrap_err().to_string();
    assert!(err.contains("needs [tee.sealing.secrets]") && err.contains("checkpoints.rpc_url"), "{err}");
}

#[test]
fn an_alert_route_names_exactly_one_source() {
    for (route, valid) in [
        (WebhookRoute { url_env: Some("HOOK".into()), url_secret: None }, true),
        (WebhookRoute { url_env: None, url_secret: Some(name_of("hook")) }, true),
        (WebhookRoute { url_env: None, url_secret: None }, false),
        (WebhookRoute { url_env: Some("HOOK".into()), url_secret: Some(name_of("hook")) }, false),
    ] {
        let mut cfg = naming_each_kind();
        cfg.alerts.warning = Some(route);
        assert_eq!(cfg.validate_secrets().is_ok(), valid);
    }
}

#[test]
fn a_production_build_refuses_each_secret_from_the_environment_and_a_dev_build_does_not() {
    let delivered = naming_each_kind();
    delivered.refuse_environment_secrets(true).unwrap();

    let mut critical = naming_each_kind();
    critical.alerts.critical = Some(WebhookRoute::from_env("HOOK"));
    let mut warning = naming_each_kind();
    warning.alerts.warning = Some(WebhookRoute::from_env("HOOK"));
    let mut vendor = naming_each_kind();
    let connections = &mut vendor.feeds.venues.as_mut().unwrap().connections;
    connections[0].api_key_secret = None;
    connections[0].api_key_env = Some("MASSIVE_API_KEY".into());
    for (what, cfg) in
        [("alerts.critical.url_env", critical), ("alerts.warning.url_env", warning), ("massive api_key_env", vendor)]
    {
        let err = cfg.refuse_environment_secrets(true).unwrap_err().to_string();
        assert!(err.contains(what) && err.contains("delivered bundle"), "{what}: {err}");
        cfg.refuse_environment_secrets(false).unwrap();
    }
}

#[test]
fn the_config_load_checks_a_named_url_as_it_checks_a_written_one() {
    let mut cfg = naming_each_kind();
    cfg.custody.provider = crate::config::CustodyProvider::Evm;
    let providers = |one: &str, two: &str, three: &str| {
        format!(
            "usdc = '0x0000000000000000000000000000000000000002'\nfinality_tag = 'latest'\n\
             withdrawal_finality_tag = 'finalized'\nconfirmations = 0\nstart_block = 0\nmax_block_range = 10\n\
             poll_interval = '1s'\nrpc_timeout = '1s'\nstall_critical_after = '1s'\nfinality_stalled_after = '1s'\n\
             cursor_path = 'cursor.json'\n\
             [[providers]]\nname = 'a'\nrpc_url = '{one}'\n[[providers]]\nname = 'b'\nrpc_url = '{two}'\n\
             [[providers]]\nname = 'c'\nrpc_url = '{three}'\n"
        )
    };
    cfg.custody.evm = Some(
        toml::from_str(&providers("https://a.example/{a_key}", "https://b.example/{b_key}", "https://c.example/c"))
            .unwrap(),
    );
    cfg.genesis.house_capital = engine_core::Fixed::ZERO;
    cfg.validate_custody().unwrap();
    let names: Vec<String> = cfg.named_secrets().unwrap().iter().map(|field| field.name.to_string()).collect();
    assert_eq!(names[..3], ["chain_key".to_owned(), "a_key".to_owned(), "b_key".to_owned()]);

    // Plain HTTP to another machine, and one provider listed twice under two keys, are as wrong as
    // they are written out.
    for (one, why) in [("http://a.example/{a_key}", "must use https"), ("https://b.example/{a_key}", "distinct URLs")] {
        cfg.custody.evm =
            Some(toml::from_str(&providers(one, "https://b.example/{b_key}", "https://c.example/c")).unwrap());
        let err = cfg.validate_custody().unwrap_err().to_string();
        assert!(err.contains(why), "{one}: {err}");
    }
}

#[test]
fn delivering_sets_each_named_url_and_leaves_the_rest_undelivered() {
    let mut cfg = naming_each_kind();
    cfg.deliver_urls(&values(&[("critical_url", "https://hooks.example/x")])).unwrap();
    assert!(cfg.checkpoints.as_ref().unwrap().rpc_url.expose().is_err(), "a key it does not hold");
    cfg.deliver_urls(&values(&[("chain_key", "FAKE-chain-key")])).unwrap();
    let exposed = cfg.checkpoints.as_ref().unwrap().rpc_url.expose().unwrap();
    assert_eq!(exposed.as_str(), "https://rpc.example/v2/FAKE-chain-key");
    assert!(!format!("{cfg:?}").contains("FAKE-chain-key"), "the config's Debug printed a key");
}

#[test]
fn a_vendor_names_its_key_one_way_and_a_public_venue_names_none() {
    let cfg = with_vendor(dev_config(), "massive_key");
    let venues = cfg.feeds.venues.as_ref().unwrap();
    venues.validate().unwrap();

    let mut both = with_vendor(dev_config(), "massive_key");
    both.feeds.venues.as_mut().unwrap().connections[0].api_key_env = Some("MASSIVE_API_KEY".into());
    let err = both.feeds.venues.as_ref().unwrap().validate().unwrap_err().to_string();
    assert!(err.contains("names both"), "{err}");

    let mut neither = with_vendor(dev_config(), "massive_key");
    neither.feeds.venues.as_mut().unwrap().connections[0].api_key_secret = None;
    let err = neither.feeds.venues.as_ref().unwrap().validate().unwrap_err().to_string();
    assert!(err.contains("api_key_env must be set"), "{err}");

    let mut public = with_vendor(dev_config(), "massive_key");
    let connection = &mut public.feeds.venues.as_mut().unwrap().connections[0];
    connection.venue = crate::config::Venue::Coinbase;
    connection.url = "wss://ws-feed.exchange.coinbase.com".into();
    let err = public.feeds.venues.as_ref().unwrap().validate().unwrap_err().to_string();
    assert!(err.contains("coinbase api_key_env must be set exactly for a vendor"), "{err}");
}
