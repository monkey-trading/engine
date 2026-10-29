//! `[api.proxy_protocol]`: off by default, and refused when it would trust more than a proxy.

use super::*;

use crate::test_support::DEV_CONFIG as DEV;

fn with_proxy(trusted: &str) -> Result<NodeConfig, toml::de::Error> {
    let dev = std::fs::read_to_string(DEV).unwrap();
    toml::from_str(&dev.replacen("[logging]", &format!("[api.proxy_protocol]\ntrusted = {trusted}\n\n[logging]"), 1))
}

#[test]
fn no_proxy_is_trusted_by_default() {
    let cfg: NodeConfig = toml::from_str(&std::fs::read_to_string(DEV).unwrap()).unwrap();
    assert!(cfg.api.proxy_protocol.is_none());
    cfg.validate().unwrap();
}

#[test]
fn trusted_ranges_parse_and_match_by_range() {
    let cfg = with_proxy(r#"["198.51.100.0/24", "2001:db8::/32"]"#).unwrap();
    cfg.validate().unwrap();
    let proxy = cfg.api.proxy_protocol.unwrap();
    for (ip, trusted) in [
        ("198.51.100.255", true),
        ("198.51.101.0", false),
        ("2001:db8:ffff::1", true),
        ("2001:db9::1", false),
        // A dual-stack listener reports an IPv4 peer in its IPv6 form.
        ("::ffff:198.51.100.7", true),
        ("::ffff:198.51.101.7", false),
    ] {
        assert_eq!(proxy.trusts(ip.parse().unwrap()), trusted, "{ip}");
    }
}

#[test]
fn a_range_that_is_not_one_fails_to_parse() {
    for bad in [r#"["not-a-range"]"#, r#"["198.51.100.0/33"]"#, r#"["198.51.100.1"]"#] {
        assert!(with_proxy(bad).is_err(), "{bad}");
    }
    let dev = std::fs::read_to_string(DEV).unwrap();
    let typo = dev.replacen("[logging]", "[api.proxy_protocol]\ntrusted = []\ntrusting = []\n\n[logging]", 1);
    assert!(toml::from_str::<NodeConfig>(&typo).unwrap_err().to_string().contains("unknown field `trusting`"));
}

#[test]
fn a_section_that_trusts_no_proxy_or_every_address_is_refused() {
    let err = with_proxy("[]").unwrap().validate().unwrap_err().to_string();
    assert!(err.contains("api.proxy_protocol.trusted must list at least one range"), "{err}");
    for all in [r#"["0.0.0.0/0"]"#, r#"["198.51.100.0/24", "::/0"]"#] {
        let err = with_proxy(all).unwrap().validate().unwrap_err().to_string();
        assert!(err.contains("must not list every address"), "{all}: {err}");
    }
}

#[test]
fn a_production_build_refuses_a_loopback_or_wide_range_but_a_dev_build_takes_them() {
    for unsafe_range in
        [r#"["127.0.0.0/8"]"#, r#"["127.0.0.1/32"]"#, r#"["::1/128"]"#, r#"["0.0.0.0/7"]"#, r#"["2001::/16"]"#]
    {
        let cfg = with_proxy(unsafe_range).unwrap();
        cfg.validate().unwrap();
        cfg.api.refuse_unsafe_proxy(false).unwrap();
        let err = cfg.api.refuse_unsafe_proxy(true).unwrap_err().to_string();
        assert!(err.contains("a production build trusts no"), "{unsafe_range}: {err}");
    }
    for fine in [r#"["198.51.100.0/24", "2001:db8::/32"]"#, r#"["10.0.0.0/8"]"#] {
        with_proxy(fine).unwrap().api.refuse_unsafe_proxy(true).unwrap();
    }
    // Nothing configured, nothing refused.
    let dev: NodeConfig = toml::from_str(&std::fs::read_to_string(DEV).unwrap()).unwrap();
    dev.api.refuse_unsafe_proxy(true).unwrap();
}
