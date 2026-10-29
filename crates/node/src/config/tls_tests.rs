//! The `[api.tls.acme]` section: what it takes, what it refuses, and a production build's refusal of
//! the self-signed certificate alone.

use super::*;

use crate::test_support::DEV_CONFIG as DEV;

const ACME: &str = r#"
[api.tls.acme]
domain = "api.example.com"
directory_url = "https://acme-staging-v02.api.letsencrypt.org/directory"
contact = "mailto:ops@example.com"
"#;

#[test]
fn the_acme_section_takes_its_keys_and_renews_at_a_third_by_default() {
    let cfg = with(ACME);
    cfg.validate().unwrap();
    let acme = &cfg.api.tls.as_ref().expect("[api.tls] from its sub-table").acme;
    assert_eq!(acme.domain, "api.example.com");
    assert_eq!(acme.contact, "mailto:ops@example.com");
    assert_eq!(acme.renew_before, None, "a third of each certificate's life");
    assert!(acme.test_ca.is_none());
    let acme = with(&format!("{ACME}renew_before = \"30d\"\n")).api.tls.unwrap().acme;
    assert_eq!(acme.renew_before, Some(Duration::from_secs(30 * 86_400)));
    assert!(toml::from_str::<NodeConfig>(&dev_with(&format!("{ACME}renewal = \"30d\"\n"))).is_err(), "unknown key");
    assert!(with("").api.tls.is_none(), "the dev config serves its self-signed certificate");
}

#[test]
fn acme_settings_a_ca_would_refuse_or_that_would_fail_every_renewal_are_refused() {
    let root = test_root();
    let cases = [
        ("lowercase DNS name", ACME.replace("api.example.com", "API.example.com")),
        ("lowercase DNS name", ACME.replace("api.example.com", "*.example.com")),
        ("lowercase DNS name", ACME.replace("api.example.com", "localhost")),
        ("lowercase DNS name", ACME.replace("api.example.com", "192.0.2.1")),
        ("lowercase DNS name", ACME.replace("api.example.com", "api..example.com")),
        ("directory_url must be https", ACME.replace("https://acme-staging", "http://acme-staging")),
        ("api.tls.acme.directory_url", ACME.replace("https://acme-staging-v02.api.letsencrypt.org", "not a url")),
        ("contact must be a mailto: URL", ACME.replace("mailto:ops@example.com", "ops@example.com")),
        ("contact must be a mailto: URL", ACME.replace("mailto:ops@example.com", "mailto:")),
        ("renew_before must be non-zero", format!("{ACME}renew_before = \"0s\"\n")),
        ("test_ca.root must be one PEM certificate", format!("{ACME}[api.tls.acme.test_ca]\nroot = \"not a PEM\"\n")),
        (
            "test_ca.root must be one PEM certificate",
            format!("{ACME}[api.tls.acme.test_ca]\nroot = '''\n{root}{root}'''\n"),
        ),
    ];
    for (expected, section) in cases {
        let err = format!("{:#}", with(&section).validate().unwrap_err());
        assert!(err.contains(expected), "expected {expected:?}, got {err}");
    }
    // A test CA's root replaces the public roots under the mock TEE only.
    let mut cfg = with(&format!("{ACME}[api.tls.acme.test_ca]\nroot = '''\n{root}'''\n"));
    cfg.validate().unwrap();
    cfg.tee.provider = TeeProvider::ConfigfsTsm;
    let err = format!("{:#}", cfg.api.tls.as_ref().unwrap().acme.validate(cfg.tee.provider).unwrap_err());
    assert!(err.contains("test_ca requires tee.provider = \"mock\""), "{err}");
}

// Runs in the production lane too, which filters on `production_build`.
#[test]
fn a_production_build_refuses_the_self_signed_certificate_alone() {
    let err = with("").api.refuse_self_signed(true).unwrap_err().to_string();
    assert!(err.contains("a production build refuses the self-signed API certificate"), "{err}");
    with(ACME).api.refuse_self_signed(true).unwrap();
    with("").api.refuse_self_signed(false).unwrap();
}

// The production lane loads a config with every provider real: refused without the section, taken
// with it, so no check before this one stops a production config.
#[cfg(feature = "production")]
#[test]
fn a_production_build_loads_a_real_provider_config_only_with_a_public_certificate() {
    let dir = crate::test_support::scratch_dir("production-acme");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("node.toml");
    std::fs::write(&path, real_providers()).unwrap();
    let err = format!("{:#}", NodeConfig::load(&path).unwrap_err());
    assert!(err.contains("a production build refuses the self-signed API certificate"), "{err}");
    std::fs::write(&path, format!("{}{ACME}", real_providers())).unwrap();
    NodeConfig::load(&path).unwrap();
}

// ── Helpers ──

/// The committed dev config with `section` appended.
fn with(section: &str) -> NodeConfig {
    toml::from_str(&dev_with(section)).unwrap()
}

/// The committed dev config's text with `section` appended.
fn dev_with(section: &str) -> String {
    format!("{}\n{section}", std::fs::read_to_string(DEV).unwrap())
}

/// A root certificate, PEM.
fn test_root() -> String {
    let key = rcgen::KeyPair::generate().unwrap();
    rcgen::CertificateParams::new(Vec::<String>::new()).unwrap().self_signed(&key).unwrap().pem()
}

/// `config/anvil.toml` with every provider real, as a production node runs: a configfs-tsm TEE, an
/// operator key nobody publishes, credit at latest, live venues, and checkpoints.
#[cfg(feature = "production")]
fn real_providers() -> String {
    let anvil = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/anvil.toml");
    let mut cfg: toml::Table = toml::from_str(&std::fs::read_to_string(anvil).unwrap()).unwrap();
    let dev: toml::Table = toml::from_str(&std::fs::read_to_string(DEV).unwrap()).unwrap();
    let tee = cfg["tee"].as_table_mut().unwrap();
    tee.insert("provider".into(), "configfs_tsm".into());
    tee.remove("use_published_mock_key");
    cfg["operator"]["public_key"] = crate::test_support::own_operator_key().into();
    cfg["custody"]["evm"]["finality_tag"] = "latest".into();
    let feeds = cfg["feeds"].as_table_mut().unwrap();
    feeds.insert("provider".into(), "venues".into());
    feeds.insert("venues".into(), dev["feeds"]["venues"].clone());
    let checkpoints = r#"
        rpc_url = "http://127.0.0.1:8545"
        relayer = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
        retry_interval = "5s"
        rpc_timeout = "5s"
    "#;
    cfg.insert("checkpoints".into(), toml::from_str::<toml::Table>(checkpoints).unwrap().into());
    toml::to_string(&cfg).unwrap()
}
