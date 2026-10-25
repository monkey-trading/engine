//! Verifying an enclave's attestation against a published release: `engine verify-attestation`
//! checks a quote served by `/v1/attestation`, and `engine replay --attestation` requires every
//! checkpoint signer to have passed it. Verification proves the quote's certificate chain up to
//! a pinned vendor root, the launch measurement and guest policy against the release, the
//! reported TCB against the release's minimum, and the report data's binding of the enclave
//! address and a configuration the release lists. A mock quote never verifies.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::types::AccountId;
use sha2::Digest as _;

use crate::tee::Attestation;

mod chain;
use self::chain::{Collateral, QuoteChain, Roots};
pub use chain::KDS_BASE_URL;
mod release;
pub use release::ReleaseManifest;
mod report;
use self::report::{SnpReport, Tcb};
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
}

/// What a verified attestation proves, for pinning a replay's checkpoint signers to it.
pub struct VerifiedAttestation {
    /// The release manifest the quote verified against.
    pub release_name: String,
    /// The enclave address the report data binds, which signs checkpoints and withdrawals.
    pub enclave_address: AccountId,
    /// The published configuration the enclave's config file hash matches.
    pub config_name: String,
}

/// Loads the attestation JSON `/v1/attestation` serves at `path`.
/// Error cases:
/// - the file cannot be read or is not that JSON shape
pub fn load_attestation(path: &Path) -> anyhow::Result<Attestation> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
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
    let release = ReleaseManifest::load(release)?;
    verify_attestation(&load_attestation(attestation)?, &release, options).await
}

/// Verifies `attestation` against `release` (see the module docs). On success the quote proved
/// the enclave address runs the named release with a listed configuration.
/// Error cases:
/// - the quote is not an SEV-SNP attestation report, its chain does not verify to a pinned root,
///   its signature does not verify, its report data does not bind the served bundle, or the
///   measurement, policy, TCB, or configuration does not match the release
pub async fn verify_attestation(
    attestation: &Attestation,
    release: &ReleaseManifest,
    options: &Options<'_>,
) -> anyhow::Result<VerifiedAttestation> {
    let quote = unhex(&attestation.quote).context("decoding the quote")?;
    let report = SnpReport::parse(&quote)
        .context("the quote is not an SEV-SNP attestation report; a mock quote never verifies")?;

    let collateral = Collateral::new(options.kds_base, options.processor)?;
    let (intermediate, root) = collateral.issuer_chain().await.context("fetching the issuer chain")?;
    let chain = QuoteChain::verify(
        collateral.vcek(&report).await.context("issuing the VCEK")?,
        intermediate,
        root,
        &roots(options)?,
    )?;
    chain.bind_vcek(&report)?;
    report.verify_signature(&chain.vcek_public_key()?)?;

    let bundle = crate::tee::AttestationBundle::decode(&unhex(&attestation.bundle)?)
        .context("the served bundle is not an attestation bundle")?;
    let report_data = unhex(&attestation.report_data)?;
    ensure!(report_data.len() == 64, "the served report data is not 64 bytes");
    ensure!(
        report_data[..32] == sha2::Sha256::digest(bundle.encode())[..],
        "the served report data is not the served bundle's SHA-256"
    );
    ensure!(report_data[32..] == [0u8; 32], "the served report data's reserved half is not zero");
    ensure!(report.report_data[..] == report_data[..], "the quote's report data differs from the served one");
    let expected = |bytes: &[u8]| format!("0x{}", hex::encode(bytes));
    ensure!(
        attestation.enclave_address == expected(&bundle.enclave_address),
        "the served enclave address differs from the bundle's"
    );
    ensure!(
        attestation.config_sha256 == expected(&bundle.config_sha256),
        "the served config hash differs from the bundle's"
    );
    ensure!(
        attestation.tls_public_key_sha256 == expected(&bundle.tls_public_key_sha256),
        "the served TLS key hash differs from the bundle's"
    );

    ensure!(
        report.measurement == release.launch_digest,
        "the quote's launch measurement matches no release launch digest"
    );
    ensure!(
        report.policy == release.policy,
        "the quote's guest policy 0x{:016x} differs from the release policy 0x{:016x}",
        report.policy,
        release.policy
    );
    let minimum: Tcb = release.minimum_tcb.into();
    ensure!(
        report.reported_tcb.meets(minimum),
        "the quote's reported TCB {:?} is below the release minimum {:?}",
        report.reported_tcb,
        minimum
    );
    let config_name = release
        .config_name(&bundle.config_sha256)
        .context("the bundle's configuration hash matches no release configuration")?
        .to_owned();
    Ok(VerifiedAttestation {
        release_name: release.name.clone(),
        enclave_address: AccountId(bundle.enclave_address),
        config_name,
    })
}

/// Requires every checkpoint `signer` to be the enclave address `verified` attested, so a log
/// whose checkpoints any other key signed does not pass.
/// Error cases:
/// - a checkpoint signer differs from the verified enclave address
pub fn ensure_signers_verified(signer: &BTreeSet<AccountId>, verified: &VerifiedAttestation) -> anyhow::Result<()> {
    for signer in signer {
        ensure!(
            *signer == verified.enclave_address,
            "checkpoint signer {signer} did not pass attestation verification ({} attests {})",
            verified.release_name,
            verified.enclave_address
        );
    }
    Ok(())
}

fn roots(options: &Options<'_>) -> anyhow::Result<Roots> {
    let mut roots = Roots::vendor();
    if let Some(pem) = options.root_pem {
        roots.add_pem(pem).context("loading the extra root")?;
    }
    Ok(roots)
}

fn unhex(text: &str) -> anyhow::Result<Vec<u8>> {
    Ok(hex::decode(text.strip_prefix("0x").unwrap_or(text))?)
}
