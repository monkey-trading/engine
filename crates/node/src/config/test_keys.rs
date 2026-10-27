//! The keys this repository publishes for local development, listed in `config/test-keys.toml`
//! next to the dev configs that use them. Anyone can read their private keys, so a node with real
//! attestation refuses a config that holds one.

use anyhow::{Context, bail};
use serde::Deserialize;

use crate::config::{NodeConfig, TeeProvider, public_key_address};

/// `config/test-keys.toml`, compiled in so that the list does not come from host storage.
const TEST_KEYS: &str = include_str!("../../../../config/test-keys.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TestKeys {
    /// Operator public keys, SEC1-encoded hex, as `operator.public_key` takes them.
    operator_public_keys: Vec<String>,
}

impl TestKeys {
    /// The keys `config/test-keys.toml` lists.
    /// Error cases:
    /// - the list does not parse
    fn published() -> anyhow::Result<Self> {
        toml::from_str(TEST_KEYS).context("config/test-keys.toml")
    }
}

impl NodeConfig {
    /// Refuses a published test key unless the TEE is the mock. Keys compare by the address they
    /// sign as, so another encoding of a listed key is refused too.
    /// Error cases:
    /// - `tee.provider` is not `mock` and `operator.public_key` or `access.source` is a published test key
    /// - `operator.public_key`, or a key `config/test-keys.toml` lists, does not parse
    pub(super) fn refuse_test_keys(&self) -> anyhow::Result<()> {
        if self.tee.provider == TeeProvider::Mock {
            return Ok(());
        }
        let operator = public_key_address(&self.operator.public_key).context("operator.public_key")?;
        for key in TestKeys::published()?.operator_public_keys {
            let published = public_key_address(&key).with_context(|| format!("config/test-keys.toml: {key}"))?;
            if self.access.as_ref().is_some_and(|access| access.source == published) {
                bail!(
                    "access.source is a published test key (config/test-keys.toml); real attestation needs an independent screening authority"
                );
            }
            if published == operator {
                bail!(
                    "operator.public_key is a published test key (config/test-keys.toml) whose private key anyone \
                     can read; with tee.provider other than \"mock\", use the operator's own key"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use k256::ecdsa::{SigningKey, VerifyingKey};

    use crate::test_support::{DEV_CONFIG, own_operator_key};

    #[test]
    fn every_published_test_key_is_refused_with_real_attestation_in_either_encoding() {
        let keys = TestKeys::published().unwrap().operator_public_keys;
        assert!(!keys.is_empty());
        for key in keys {
            let point = VerifyingKey::from_sec1_bytes(&hex::decode(key.trim_start_matches("0x")).unwrap()).unwrap();
            let compressed = format!("0x{}", hex::encode(point.to_sec1_point(true).as_bytes()));
            assert_ne!(compressed, key);
            for form in [key.clone(), compressed] {
                let err = attested(&form).validate().unwrap_err();
                assert!(format!("{err:#}").contains("is a published test key"), "{form}: {err:#}");
            }
        }
    }

    #[test]
    fn the_dev_config_with_real_attestation_is_refused_and_an_own_key_or_the_mock_tee_passes() {
        let err = attested_dev().validate().unwrap_err();
        assert!(format!("{err:#}").contains("operator.public_key is a published test key"), "{err:#}");
        attested(&own_operator_key()).validate().unwrap();
        NodeConfig::load(Path::new(DEV_CONFIG)).unwrap();
    }

    #[test]
    fn the_anvil_accounts_the_dev_scenario_signs_with_are_refused_with_real_attestation() {
        // Accounts 0, 1, and 2 of Anvil's default mnemonic: the deployer, trader, and house funder
        // in scripts/dev-vault-deposits.sh. Account 0 also signs crates/core/tests/fixtures.
        let private_keys = [
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
            "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
            "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
        ];
        for private_key in private_keys {
            let key = SigningKey::from_slice(&hex::decode(private_key).unwrap()).unwrap();
            let public_key = format!("0x{}", hex::encode(key.verifying_key().to_sec1_point(false).as_bytes()));
            let err = attested(&public_key).validate().unwrap_err();
            assert!(format!("{err:#}").contains("is a published test key"), "{private_key}: {err:#}");
        }
    }

    #[test]
    fn every_operator_key_a_committed_config_uses_is_listed() {
        let listed: Vec<_> = TestKeys::published()
            .unwrap()
            .operator_public_keys
            .iter()
            .map(|key| public_key_address(key).unwrap())
            .collect();
        let dir = Path::new(DEV_CONFIG).parent().unwrap();
        // Every full node config in `config/`; the genesis and key lists alone are not configs.
        let configs: Vec<NodeConfig> = std::fs::read_dir(dir)
            .unwrap()
            .map(|file| std::fs::read_to_string(file.unwrap().path()).unwrap())
            .filter_map(|text| toml::from_str::<NodeConfig>(&text).ok())
            .collect();
        assert!(configs.len() >= 2, "dev.toml and anvil.toml at least");
        for cfg in configs {
            assert_eq!(cfg.tee.provider, TeeProvider::Mock);
            let operator = cfg.operator().unwrap().address;
            assert!(listed.contains(&operator), "operator {operator} is not in config/test-keys.toml");
        }
    }

    #[test]
    fn real_attestation_refuses_a_published_or_plain_http_access_source_and_passes_an_own_https_one() {
        let with_access = |source: &str, endpoint: &str| {
            let mut cfg = attested(&own_operator_key());
            cfg.access = Some(
                toml::from_str(&format!(
                    "source='{source}'\nendpoint='{endpoint}'\ntimeout='1s'\nmax_age='30s'\nmax_response_bytes=4096\nmax_in_flight=2"
                ))
                .unwrap(),
            );
            cfg
        };
        let https = "https://screen.example/api";
        for key in TestKeys::published().unwrap().operator_public_keys {
            let published = public_key_address(&key).unwrap().to_string();
            let err = with_access(&published, https).validate().unwrap_err();
            assert!(format!("{err:#}").contains("access.source is a published test key"), "{key}: {err:#}");
        }
        let own = "0x4646464646464646464646464646464646464646";
        let err = with_access(own, "http://127.0.0.1:18846").validate().unwrap_err();
        assert!(format!("{err:#}").contains("access HTTP fixtures require the mock TEE"), "{err:#}");
        with_access(own, https).validate().unwrap();
    }

    // ── Helpers ──

    /// The dev config under real attestation, with the signed requests and Critical route that needs.
    fn attested_dev() -> NodeConfig {
        let text = std::fs::read_to_string(DEV_CONFIG).unwrap();
        let attested = text
            .replace("provider = \"mock\" # \"configfs_tsm\"", "provider = \"configfs_tsm\" #")
            .replace("auth = \"none\"", "auth = \"eip712\"");
        assert_ne!(attested, text, "the replacement must change the config");
        let mut cfg: NodeConfig =
            toml::from_str(&format!("{attested}\n[alerts.critical]\nurl_env = \"PAGER\"\n")).unwrap();
        cfg.tee.use_published_mock_key = false;
        cfg
    }

    /// `attested_dev` with `key` as the operator's public key.
    fn attested(key: &str) -> NodeConfig {
        let mut cfg = attested_dev();
        cfg.operator.public_key = key.to_owned();
        cfg
    }
}
