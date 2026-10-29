//! Writes the attestation fixtures `scripts/dev-wasm-verifier.sh` checks with both the CLI and the
//! browser build: a test-signed SEV-SNP report for the enclave address given, the release manifests
//! and collateral to verify it with, and the cases that must pass or fail.
//!
//! Usage: `verifier_fixtures <out-dir> <enclave address>`

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use engine_verify::fixtures::*;
use engine_verify::{AttestationBundle, MOCK_QUOTE_MARKER, RelayedCollateral};

/// Writes the fixtures into the directory the first argument names.
/// Error cases:
/// - the arguments are missing or the address is not `0x` and 40 hex digits
/// - a file cannot be written
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: verifier_fixtures <out-dir> <enclave address>";
    let dir = PathBuf::from(args.next().context(usage)?);
    let address = args.next().context(usage)?;
    let mut enclave_address = [0; 20];
    hex::decode_to_slice(address.strip_prefix("0x").context("the address has no 0x")?, &mut enclave_address)
        .context("the address is not 40 hex digits")?;
    std::fs::create_dir_all(&dir)?;

    let chip_id = enclave().chip_id;
    let bundle = AttestationBundle { enclave_address, config_sha256: [0xc0; 32], tls_public_key_sha256: [0x71; 32] };
    let chain = test_chain(chip_id, REPORTED_TCB);
    let report = |measurement, bundle: &AttestationBundle| {
        serde_json::to_string(&attestation(bundle, &forged_report(&chain, measurement, bundle.report_data(), chip_id)))
    };
    let unlisted = AttestationBundle { config_sha256: [0xc1; 32], ..bundle };
    let mut corrupted = forged_report(&chain, LAUNCH_DIGEST, bundle.report_data(), chip_id);
    corrupted[0x2A0] ^= 1;
    let mut mock_quote = MOCK_QUOTE_MARKER.to_vec();
    mock_quote.extend_from_slice(&bundle.report_data());
    let mut mock = attestation(&bundle, &mock_quote);
    mock.provider = "mock".into();

    let ask_serial = rcgen::SerialNumber::from_slice(&serial(chain.ask.der()));
    let mut other_chip = chip_id;
    other_chip[0] ^= 1;
    let good = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let foreign = RelayedCollateral { vcek: vcek_for(&chain, other_chip, REPORTED_TCB).der().to_vec(), ..good.clone() };
    let files: [(&str, Vec<u8>); 15] = [
        ("root.pem", pem(&chain.ark).into_bytes()),
        ("attestation.json", report(LAUNCH_DIGEST, &bundle)?.into_bytes()),
        ("mock.json", serde_json::to_vec(&mock)?),
        ("unlisted-config.json", report(LAUNCH_DIGEST, &unlisted)?.into_bytes()),
        ("altered-measurement.json", report([0x1e; 48], &bundle)?.into_bytes()),
        ("corrupted-signature.json", serde_json::to_vec(&attestation(&bundle, &corrupted))?),
        ("release.toml", release_toml().into_bytes()),
        ("previous-release.toml", previous_release().into_bytes()),
        ("high-tcb-release.toml", release_toml().replace("microcode = 115", "microcode = 206").into_bytes()),
        ("collateral.json", good.to_json()),
        ("collateral-revoked.json", collateral_of(&chain, &crl_revoking(&chain, &[ask_serial])).to_json()),
        ("collateral-foreign-vcek.json", foreign.to_json()),
        (
            "collateral-stale.json",
            collateral_of(&chain, &crl_signed(&chain, &[], (2024, 1, 1), (2025, 1, 1), None)).to_json(),
        ),
        (
            "collateral-early.json",
            collateral_of(&chain, &crl_signed(&chain, &[], (2090, 1, 1), (2091, 1, 1), None)).to_json(),
        ),
        ("cases.json", serde_json::to_vec_pretty(&cases())?),
    ];
    for (name, bytes) in files {
        write(&dir, name, &bytes)?;
    }
    Ok(())
}

/// Each case: the attestation, the manifests in order, the collateral, and whether it verifies.
fn cases() -> serde_json::Value {
    let case = |name: &str, attestation: &str, releases: &[&str], collateral: &str, verifies: bool| {
        serde_json::json!({
            "name": name, "attestation": attestation, "releases": releases, "collateral": collateral,
            "verifies": verifies,
        })
    };
    let release = &["release.toml"][..];
    serde_json::json!({
        "processor": PROCESSOR,
        "root": "root.pem",
        "cases": [
            case("the test-signed report", "attestation.json", release, "collateral.json", true),
            case("the previous and current releases", "attestation.json",
                &["previous-release.toml", "release.toml"], "collateral.json", true),
            case("a mock quote", "mock.json", release, "collateral.json", false),
            case("an unlisted configuration hash", "unlisted-config.json", release, "collateral.json", false),
            case("an altered measurement", "altered-measurement.json", release, "collateral.json", false),
            case("a corrupted report signature", "corrupted-signature.json", release, "collateral.json", false),
            case("a TCB below the minimum", "attestation.json", &["high-tcb-release.toml"], "collateral.json", false),
            case("a revoked intermediate", "attestation.json", release, "collateral-revoked.json", false),
            case("a VCEK for another chip", "attestation.json", release, "collateral-foreign-vcek.json", false),
            case("a stale revocation list", "attestation.json", release, "collateral-stale.json", false),
            case("a revocation list not yet valid", "attestation.json", release, "collateral-early.json", false),
            case("no release lists the quote", "attestation.json", &["previous-release.toml"], "collateral.json", false),
        ],
    })
}

/// A release the fixture quote does not run: another name and launch digest.
fn previous_release() -> String {
    release_toml()
        .replace("test-release-v1", "test-release-v0")
        .replace(&hex::encode(LAUNCH_DIGEST), &hex::encode([0x0d; 48]))
}

/// A certificate's serial content bytes.
fn serial(der: &[u8]) -> Vec<u8> {
    use x509_parser::prelude::FromDer as _;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der).expect("a fixture certificate parses");
    cert.serial.to_bytes_be()
}

/// Writes `bytes` to `name` in `dir`.
/// Error cases:
/// - the file cannot be written
fn write(dir: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::write(dir.join(name), bytes).with_context(|| format!("writing {name}"))
}
