//! The release manifest a verifier checks an attestation against: what a published release
//! measured at launch and the SEV features that measurement covers, the guest policy it requires,
//! each CPU model it accepts with the minimum TCB it takes from that model, and the configuration
//! hashes it runs with. A release publishes it alongside its image files.

use std::collections::BTreeMap;

use anyhow::{bail, ensure};
use serde::Deserialize;

use crate::cpu::CpuModel;
use crate::report::Tcb;

/// The manifest format this verifier reads. Format 1 had no `format` key and set one minimum TCB
/// for every CPU model; format 2 lists each accepted model with its own.
pub const MANIFEST_FORMAT: i64 = 2;

/// A release's published verification collateral.
///
/// ```toml
/// format = 2
/// name = "v0.1.0"
/// launch_digest = "0x…"    # 48 bytes, measurement.json's launch_digest
/// policy = "0x30000"       # the guest policy, measurement.json's policy
/// guest_features = "0x201" # each vCPU's SEV_FEATURES, measurement.json's guest_features
///
/// [[cpu_models]]           # one per CPU model the release accepts
/// product_name = "Genoa-B1"
/// family = 0x19
/// model = 0x11
/// stepping = 0x01
/// minimum_tcb = { boot_loader = 0, tee = 0, snp = 0x1D, microcode = 0x58 }
///
/// [config_hashes]
/// "node.toml" = "0x…"
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    /// The manifest format, `MANIFEST_FORMAT`.
    pub format: i64,
    /// The release this manifest verifies, named on success.
    pub name: String,
    /// The SEV-SNP launch digest the release's image builds to.
    #[serde(deserialize_with = "deserialize_digest48")]
    pub launch_digest: [u8; 48],
    /// The guest policy the release launches with, `measurement.json`'s policy.
    #[serde(deserialize_with = "deserialize_u64_hex")]
    pub policy: u64,
    /// The SEV_FEATURES every vCPU launches with, `measurement.json`'s guest features. The report
    /// does not carry them; the launch digest covers each vCPU's, so it holds them to this value.
    #[serde(deserialize_with = "deserialize_u64_hex")]
    pub guest_features: u64,
    /// Each CPU model a quote from this release is accepted from, with the lowest TCB accepted from
    /// that model.
    pub cpu_models: Vec<CpuModel>,
    /// SHA-256 of each configuration file the release runs with, by the file's published name.
    #[serde(deserialize_with = "deserialize_config_hashes")]
    pub config_hashes: BTreeMap<String, [u8; 32]>,
}

/// The lowest security-version levels a reported TCB must carry, per component.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
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
    /// - the text is not TOML, or it names no format or another than `MANIFEST_FORMAT`
    /// - the text does not parse as the format, or a field fails its structural checks (see `validate`)
    pub fn from_toml(text: &str) -> anyhow::Result<Self> {
        let table: toml::Table = toml::from_str(text)?;
        match table.get("format") {
            None => bail!(
                "the release manifest names no format, so it is format 1, which sets one minimum TCB for \
                 every CPU model; this verifier reads format {MANIFEST_FORMAT}, which lists each CPU model \
                 the release accepts with its own"
            ),
            Some(toml::Value::Integer(format)) if *format == MANIFEST_FORMAT => {}
            Some(format) => bail!("unsupported release manifest format {format}, want {MANIFEST_FORMAT}"),
        }
        let manifest: Self = toml::from_str(text)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// The structural checks a parsed manifest must pass.
    /// Error cases:
    /// - the format is not `MANIFEST_FORMAT`
    /// - the manifest names no release, or lists no configuration hash
    /// - its CPU models fail `CpuModel::validate_list`
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.format == MANIFEST_FORMAT,
            "unsupported release manifest format {}, want {MANIFEST_FORMAT}",
            self.format
        );
        ensure!(!self.name.is_empty(), "release manifest names no release");
        ensure!(!self.config_hashes.is_empty(), "release manifest lists no configuration hashes");
        CpuModel::validate_list(&self.cpu_models, "the release manifest")
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
format = 2
name = "test-release-v1"
launch_digest = "0x0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f30"
policy = "0x30000"
guest_features = "0x201"

[[cpu_models]]
product_name = "Genoa-A2"
family = 0x19
model = 0xA0
stepping = 0x02
minimum_tcb = { boot_loader = 0, tee = 0, snp = 8, microcode = 115 }

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
        assert_eq!(manifest.guest_features, 0x201);
        let siena = &manifest.cpu_models[0];
        assert_eq!(
            (siena.product_name.as_str(), siena.family, siena.model, siena.stepping),
            ("Genoa-A2", 0x19, 0xA0, 2)
        );
        assert_eq!(siena.minimum_tcb, MinimumTcb { boot_loader: 0, tee: 0, snp: 8, microcode: 115 });
        assert_eq!(manifest.config_name(&[0xaa; 32]), Some("node.toml"));
        assert_eq!(manifest.config_name(&[0xbb; 32]), None);
    }

    #[test]
    fn bad_hex_wrong_lengths_and_unknown_keys_are_rejected() {
        let long_digest = MANIFEST.replacen("launch_digest = \"0x01", "launch_digest = \"0x0102", 1);
        let bad_policy = MANIFEST.replacen("policy = \"0x30000\"", "policy = \"0xzz\"", 1);
        let renamed_key = MANIFEST.replacen("name = ", "title = ", 1);
        let extra_key = MANIFEST.replacen("snp = 8,", "snp = 8, other = 1,", 1);
        let extra_model_key = MANIFEST.replacen("stepping = 0x02", "stepping = 0x02\nprocessor = \"Genoa\"", 1);
        let bad_hex = MANIFEST.replacen("launch_digest = \"0x01", "launch_digest = \"0xzz", 1);
        let no_features = MANIFEST.replacen("guest_features = \"0x201\"\n", "", 1);
        let bad_features = MANIFEST.replacen("guest_features = \"0x201\"", "guest_features = \"0xsecure\"", 1);
        for manifest in [
            long_digest.as_str(),
            bad_policy.as_str(),
            renamed_key.as_str(),
            extra_key.as_str(),
            extra_model_key.as_str(),
            bad_hex.as_str(),
            no_features.as_str(),
            bad_features.as_str(),
        ] {
            assert!(toml::from_str::<ReleaseManifest>(manifest).is_err(), "accepted {manifest}");
        }
    }

    #[test]
    fn a_manifest_of_another_format_is_refused_with_its_format_named() {
        let format_1 = MANIFEST.replacen("format = 2\n", "", 1).replacen(
            "minimum_tcb = { boot_loader = 0, tee = 0, snp = 8, microcode = 115 }",
            "",
            1,
        );
        let err = ReleaseManifest::from_toml(&format_1).unwrap_err();
        assert!(err.to_string().starts_with("the release manifest names no format, so it is format 1"), "{err:#}");
        for (format, named) in [("3", "3"), ("1", "1"), ("\"2\"", "\"2\"")] {
            let text = MANIFEST.replacen("format = 2", &format!("format = {format}"), 1);
            let err = ReleaseManifest::from_toml(&text).unwrap_err();
            assert_eq!(err.to_string(), format!("unsupported release manifest format {named}, want 2"));
        }
        let mut parsed = parsed();
        parsed.format = 1;
        assert_eq!(parsed.validate().unwrap_err().to_string(), "unsupported release manifest format 1, want 2");
    }

    #[test]
    fn a_cpu_model_list_must_name_each_model_once_under_its_own_product_line() {
        let model = "[[cpu_models]]\nproduct_name = \"Genoa-A2\"\nfamily = 0x19\nmodel = 0xA0\nstepping = 0x02\n\
                     minimum_tcb = { boot_loader = 0, tee = 0, snp = 8, microcode = 115 }\n";
        let section = |models: &str| {
            let (head, tail) = MANIFEST.split_once("[[cpu_models]]").unwrap();
            let tail = tail.split_once("[config_hashes]").unwrap().1;
            format!("{head}{models}\n[config_hashes]{tail}")
        };
        for (models, message) in [
            ("cpu_models = []\n".to_owned(), "the release manifest lists no CPU model, so it would accept no quote"),
            (format!("{model}{model}"), "the release manifest lists family 0x19 model 0xa0 stepping 0x02 twice"),
            (
                model.replace("Genoa-A2", "Genoa"),
                "the release manifest: CPU model \"Genoa\" is not a product line and a stepping, such as Genoa-B1",
            ),
            (
                model.replace("Genoa-A2", "Genoa-A 2"),
                "the release manifest: CPU model \"Genoa-A 2\" is not a product line and a stepping, such as Genoa-B1",
            ),
            (
                model.replace("Genoa-A2", "Milan-B0"),
                "the release manifest: CPU model Milan-B0: family 0x19 model 0xa0 stepping 0x02 is certified under \
                 Genoa",
            ),
            (
                model.replace("model = 0xA0", "model = 0x50"),
                "the release manifest: CPU model Genoa-A2: family 0x19 model 0x50 stepping 0x02 is in no product line",
            ),
            (
                model.replace("family = 0x19", "family = 0x1A"),
                "the release manifest: CPU model Genoa-A2: family 0x1a model 0xa0 stepping 0x02 is in no product line",
            ),
        ] {
            let err = ReleaseManifest::from_toml(&section(&models)).unwrap_err();
            assert_eq!(format!("{err:#}"), message);
        }
        let genoa = model.replace("Genoa-A2", "Genoa-B1").replace("model = 0xA0", "model = 0x11");
        let manifest = ReleaseManifest::from_toml(&section(&format!("{model}{genoa}"))).unwrap();
        assert_eq!(manifest.cpu_models.len(), 2);
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
