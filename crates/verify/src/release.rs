//! The release manifest a verifier checks an attestation against: what a published release
//! measured at launch, the guest policy and minimum TCB it requires, and the configuration
//! hashes it runs with. A release publishes it alongside its image files.

use std::collections::BTreeMap;

use anyhow::ensure;
use serde::Deserialize;

use crate::report::Tcb;

/// A release's published verification collateral.
///
/// ```toml
/// name = "v0.1.0"
/// launch_digest = "0x…"    # 48 bytes, measurement.json's launch_digest
/// policy = "0x30000"       # the guest policy, measurement.json's policy
///
/// [minimum_tcb]
/// boot_loader = 0
/// tee = 0
/// snp = 8
/// microcode = 115
///
/// [config_hashes]
/// "node.toml" = "0x…"
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    /// The release this manifest verifies, named on success.
    pub name: String,
    /// The SEV-SNP launch digest the release's image builds to.
    #[serde(deserialize_with = "deserialize_digest48")]
    pub launch_digest: [u8; 48],
    /// The guest policy the release launches with, `measurement.json`'s policy.
    #[serde(deserialize_with = "deserialize_u64_hex")]
    pub policy: u64,
    /// The lowest reported TCB a quote from this release is accepted at.
    pub minimum_tcb: MinimumTcb,
    /// SHA-256 of each configuration file the release runs with, by the file's published name.
    #[serde(deserialize_with = "deserialize_config_hashes")]
    pub config_hashes: BTreeMap<String, [u8; 32]>,
}

/// The lowest security-version levels a reported TCB must carry, per component.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinimumTcb {
    pub boot_loader: u8,
    pub tee: u8,
    pub snp: u8,
    pub microcode: u8,
}

impl From<MinimumTcb> for Tcb {
    fn from(minimum: MinimumTcb) -> Self {
        Self { boot_loader: minimum.boot_loader, tee: minimum.tee, snp: minimum.snp, microcode: minimum.microcode }
    }
}

impl ReleaseManifest {
    /// Parses and validates the manifest's published TOML text.
    /// Error cases:
    /// - the text does not parse, or a field fails its structural checks (see `validate`)
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        let manifest: Self = toml::from_str(text)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// The structural checks a parsed manifest must pass.
    /// Error cases:
    /// - the manifest names no release, or lists no configuration hash
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(!self.name.is_empty(), "release manifest names no release");
        ensure!(!self.config_hashes.is_empty(), "release manifest lists no configuration hashes");
        Ok(())
    }

    /// The published configuration whose hash is `sha256`, if one matches.
    pub fn config_name(&self, sha256: &[u8; 32]) -> Option<&str> {
        self.config_hashes.iter().find(|(_, hash)| *hash == sha256).map(|(name, _)| name.as_str())
    }
}

fn hex_bytes<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    let hex = text.strip_prefix("0x").unwrap_or(&text);
    hex::decode(hex).map_err(serde::de::Error::custom)
}

fn deserialize_digest48<'de, D>(deserializer: D) -> Result<[u8; 48], D::Error>
where
    D: serde::Deserializer<'de>,
{
    let bytes = hex_bytes(deserializer)?;
    bytes.try_into().map_err(|_| serde::de::Error::custom("launch_digest must be 48 bytes"))
}

fn deserialize_u64_hex<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    let hex = text.strip_prefix("0x").unwrap_or(&text);
    u64::from_str_radix(hex, 16).map_err(serde::de::Error::custom)
}

fn deserialize_config_hashes<'de, D>(deserializer: D) -> Result<BTreeMap<String, [u8; 32]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: BTreeMap<String, String> = BTreeMap::deserialize(deserializer)?;
    raw.into_iter()
        .map(|(name, text)| {
            let bytes = hex::decode(text.strip_prefix("0x").unwrap_or(&text)).map_err(serde::de::Error::custom)?;
            let hash: [u8; 32] =
                bytes.try_into().map_err(|_| serde::de::Error::custom("config hash must be 32 bytes"))?;
            Ok((name, hash))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
name = "test-release-v1"
launch_digest = "0x0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f30"
policy = "0x30000"

[minimum_tcb]
boot_loader = 0
tee = 0
snp = 8
microcode = 115

[config_hashes]
"node.toml" = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
"#;

    fn parsed() -> ReleaseManifest {
        toml::from_str(MANIFEST).unwrap()
    }

    #[test]
    fn a_manifest_parses_into_typed_fields() {
        let manifest = parsed();
        assert_eq!(manifest.name, "test-release-v1");
        assert_eq!(manifest.launch_digest[0], 1);
        assert_eq!(manifest.launch_digest[47], 0x30);
        assert_eq!(manifest.policy, 0x30000);
        assert_eq!(manifest.minimum_tcb.snp, 8);
        assert_eq!(manifest.config_name(&[0xaa; 32]), Some("node.toml"));
        assert_eq!(manifest.config_name(&[0xbb; 32]), None);
    }

    #[test]
    fn bad_hex_wrong_lengths_and_unknown_keys_are_rejected() {
        let long_digest = MANIFEST.replacen("launch_digest = \"0x01", "launch_digest = \"0x0102", 1);
        let bad_policy = MANIFEST.replacen("policy = \"0x30000\"", "policy = \"0xzz\"", 1);
        let renamed_key = MANIFEST.replacen("name = ", "title = ", 1);
        let extra_key = MANIFEST.replacen("snp = 8", "snp = 8\nother = 1", 1);
        let bad_hex = MANIFEST.replacen("launch_digest = \"0x01", "launch_digest = \"0xzz", 1);
        for manifest in
            [long_digest.as_str(), bad_policy.as_str(), renamed_key.as_str(), extra_key.as_str(), bad_hex.as_str()]
        {
            assert!(toml::from_str::<ReleaseManifest>(manifest).is_err(), "accepted {manifest}");
        }
    }

    #[test]
    fn an_empty_config_list_or_name_is_rejected() {
        let without_configs = MANIFEST.split("[config_hashes]").next().expect("manifest has the section");
        let err = ReleaseManifest::from_toml(&format!("{without_configs}[config_hashes]\n")).unwrap_err();
        assert_eq!(err.to_string(), "release manifest lists no configuration hashes");
        let unnamed = MANIFEST.replacen("name = \"test-release-v1\"", "name = \"\"", 1);
        assert_eq!(ReleaseManifest::from_toml(&unnamed).unwrap_err().to_string(), "release manifest names no release");
        assert_eq!(ReleaseManifest::from_toml(MANIFEST).unwrap().name, "test-release-v1");
    }
}
