//! Verifying an enclave's attestation against a published release: `engine verify-attestation`
//! checks a quote served by `/v1/attestation`, and `engine replay --attestation` requires every
//! checkpoint signer to have passed it. This module fetches or loads what verification needs and
//! reads the clock; `engine_verify`, which a browser runs too, does the verifying. A mock quote
//! never verifies. `answers` checks the API responses and stream frames that enclave signed.

use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;
use engine_verify::SnpReport;
pub use engine_verify::{RelayedCollateral, ReleaseManifest};

use crate::tee::Attestation;

mod answers;
pub use answers::{
    Expected, SentRequest, VerifiedResponse, parse_challenge_arg, verify_frames_file, verify_response_files,
};
mod fetch;
use self::fetch::Collateral;
pub(crate) use fetch::CollateralUnavailable;
pub use fetch::KDS_BASE_URL;
mod refresh;
pub use refresh::{Relay, Served};
#[cfg(test)]
mod tests;

/// Where the quote's certificate chain comes from, and which processor's chain it is.
pub struct Options<'a> {
    /// The processor product name the key-distribution service's chain is keyed by, as the
    /// vendor publishes it (for example the codename of the CPU generation).
    pub processor: &'a str,
    /// The key-distribution service's base URL; the vendor's service unless overridden.
    pub kds_base: &'a str,
    /// An extra root certificate, ASCII PEM, pinning the chain in addition to the compiled-in
    /// vendor roots. Test and private chains verify against this; release chains do not need it.
    pub root_pem: Option<&'a str>,
    /// Collateral a node relayed (`GET /v1/attestation/collateral`), used in place of calls to
    /// the key-distribution service. It is checked exactly as fetched collateral is: against the
    /// pinned roots, so where it came from adds no trust.
    pub collateral: Option<&'a RelayedCollateral>,
}

/// What a verified attestation proves, for pinning a replay's checkpoint signers to it.
pub struct VerifiedAttestation {
    /// The release manifest the quote verified against.
    pub release_name: String,
    /// The enclave address the report data binds, which signs checkpoints and withdrawals.
    pub enclave_address: AccountId,
    /// The published configuration the enclave's config file hash matches.
    pub config_name: String,
    /// SHA-256 of the DER SubjectPublicKeyInfo of the API's TLS key, which the report data binds.
    pub tls_public_key_sha256: [u8; 32],
}

/// Loads the attestation JSON `/v1/attestation` serves at `path`.
/// Error cases:
/// - the file cannot be read or is not that JSON shape
pub fn load_attestation(path: &Path) -> anyhow::Result<Attestation> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// Loads the collateral a node relayed, from the file it was saved to or the URL of the node's
/// `/v1/attestation/collateral`. A URL is fetched with the same bounded client as vendor
/// collateral, over TLS to public roots, so a node with a self-signed certificate is read from a
/// saved file instead. Where it came from adds no trust: `verify_attestation` checks it against the
/// pinned roots.
/// Error cases:
/// - the file or URL cannot be read, is over the fetch size limit, or is not a collateral document
pub async fn load_relayed_collateral(source: &str, options: &Options<'_>) -> anyhow::Result<RelayedCollateral> {
    let bytes = if source.starts_with("https://") || source.starts_with("http://") {
        Collateral::new(options.kds_base, options.processor)?.document(source).await?
    } else {
        let limit = fetch::MAX_COLLATERAL_BYTES as u64;
        let file = std::fs::File::open(source).with_context(|| format!("reading {source}"))?;
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes).with_context(|| format!("reading {source}"))?;
        ensure!(bytes.len() as u64 <= limit, "{source} is over the {limit} byte limit");
        bytes
    };
    RelayedCollateral::from_json(&bytes)
}

/// Loads the attestation and release manifest files and verifies the attestation against the
/// release. This is what `engine verify-attestation` runs, so the scenario exercises the same
/// file-loading path the CLI takes.
/// Error cases:
/// - either file cannot be loaded, or verification fails (see `verify_attestation`)
pub async fn verify_attestation_files(
    attestation: &Path,
    release: &Path,
    options: &Options<'_>,
) -> anyhow::Result<VerifiedAttestation> {
    let release = load_release(release)?;
    verify_attestation(&load_attestation(attestation)?, &release, options).await
}

/// Loads and validates the release manifest at `path`.
/// Error cases:
/// - the file cannot be read or parsed, or a field fails its structural checks
pub fn load_release(path: &Path) -> anyhow::Result<ReleaseManifest> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let manifest: ReleaseManifest = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    manifest.validate()?;
    Ok(manifest)
}

/// Verifies `attestation` against `release` with `engine_verify`, taking the collateral from
/// `options` or fetching it from the key-distribution service, and checking dates on the system
/// clock. On success the quote proved the enclave address runs the named release with a listed
/// configuration.
/// Error cases:
/// - the quote is not an SEV-SNP attestation report, or its collateral cannot be fetched
/// - see `engine_verify::verify_attestation`
pub async fn verify_attestation(
    attestation: &Attestation,
    release: &ReleaseManifest,
    options: &Options<'_>,
) -> anyhow::Result<VerifiedAttestation> {
    let fetched;
    let collateral = match options.collateral {
        Some(relayed) => relayed,
        None => {
            let quote = hex::decode(attestation.quote.strip_prefix("0x").unwrap_or(&attestation.quote))
                .context("decoding the quote")?;
            let report = SnpReport::parse(&quote)
                .context("the quote is not an SEV-SNP attestation report; a mock quote never verifies")?;
            fetched = Collateral::new(options.kds_base, options.processor)?.relayed(&report).await?;
            &fetched
        }
    };
    let now = crate::clock::now_ms().div_euclid(1000);
    let verified = engine_verify::verify_attestation(
        attestation,
        std::slice::from_ref(release),
        collateral,
        options.root_pem,
        now,
    )?;
    Ok(VerifiedAttestation {
        release_name: verified.release_name,
        enclave_address: AccountId(verified.enclave_address),
        config_name: verified.config_name,
        tls_public_key_sha256: verified.tls_public_key_sha256,
    })
}

/// Reconciles an explicitly pinned signer with a verified attestation for replay: the verified
/// enclave address becomes the trusted signer, and an explicit pin must match it.
/// Error cases:
/// - `signer` and `verified` disagree
pub fn reconcile_signer(
    signer: Option<AccountId>,
    verified: Option<&VerifiedAttestation>,
) -> anyhow::Result<Option<AccountId>> {
    match (signer, verified) {
        (Some(signer), Some(verified)) => {
            ensure!(
                signer == verified.enclave_address,
                "--signer {signer} differs from the attested enclave address {}",
                verified.enclave_address
            );
            Ok(Some(signer))
        }
        (signer, verified) => Ok(signer.or_else(|| verified.map(|verified| verified.enclave_address))),
    }
}

/// Requires every checkpoint `signers` to be the enclave address `verified` attested, so a log
/// whose checkpoints any other key signed does not pass.
/// Error cases:
/// - a checkpoint signer differs from the verified enclave address
pub fn ensure_signers_verified(signers: &BTreeSet<AccountId>, verified: &VerifiedAttestation) -> anyhow::Result<()> {
    for signer in signers {
        ensure!(
            *signer == verified.enclave_address,
            "checkpoint signer {signer} did not pass attestation verification ({} attests {})",
            verified.release_name,
            verified.enclave_address
        );
    }
    Ok(())
}
