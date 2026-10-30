//! Verifying an enclave's attestation against published releases, with no IO and no clock: the
//! caller passes in the attestation `/v1/attestation` served, the release manifests it trusts, the
//! vendor collateral (`/v1/attestation/collateral` or the vendor's service), an optional extra
//! root, and the time to check dates at. `engine verify-attestation` and `engine replay
//! --attestation` fetch these and call this crate, and the browser build (`engine-verify-wasm`)
//! runs the same code. Verification proves the quote's certificate chain up to a pinned vendor
//! root, the launch measurement and guest policy against a release, the reported TCB against the
//! release's minimum, and the report data's binding of the enclave address and a configuration the
//! release lists. A mock quote never verifies; `verify_mock_attestation` reads one for development.

use anyhow::{Context, bail, ensure};
use sha2::Digest as _;
use x509_parser::time::ASN1Time;

mod attestation;
pub use attestation::{Attestation, AttestationBundle, MOCK_QUOTE_MARKER};
mod chain;
use chain::{QuoteChain, Roots};
pub use chain::{Window, pem_certificates};
mod collateral;
pub use collateral::RelayedCollateral;
#[cfg(any(test, feature = "test-fixtures"))]
pub mod fixtures;
mod release;
pub use release::{MinimumTcb, ReleaseManifest};
mod report;
pub use report::{SnpReport, Tcb};
mod signature;
#[cfg(test)]
mod tests;

/// What a verified attestation proves.
#[derive(Debug)]
pub struct VerifiedAttestation {
    /// The release manifest the quote verified against.
    pub release_name: String,
    /// The enclave address the report data binds, which signs checkpoints, withdrawals, and API
    /// answers.
    pub enclave_address: [u8; 20],
    /// The published configuration the enclave's config file hash matches.
    pub config_name: String,
    /// SHA-256 of the DER SubjectPublicKeyInfo of the API's TLS key, which the report data binds.
    pub tls_public_key_sha256: [u8; 32],
    /// The Unix seconds at which every date check passes: from the latest of the VCEK's and the
    /// intermediate's not-before and the revocation list's this-update, to the earliest of their
    /// not-after and the list's next-update. The verdict holds for a time inside it only, so a
    /// caller can check a second clock, such as the node's signed time, against it.
    pub validity: Window,
}

/// Verifies `attestation` against `releases` (see the crate docs), with the vendor `collateral`
/// for its quote, the compiled-in vendor roots plus `root_pem` when given, and every date checked
/// at `now` in Unix seconds. On success the quote proved the enclave address runs the named release
/// with a listed configuration; the first release that admits the quote is named.
/// Error cases:
/// - `now` is out of the certificate time range
/// - the quote is not an SEV-SNP attestation report, its chain does not verify to a pinned root,
///   its signature does not verify, or its report data does not bind the served bundle
/// - no release admits the quote: with one release, the first of its measurement, policy, TCB, and
///   configuration checks that fails; with several, each one's
pub fn verify_attestation(
    attestation: &Attestation,
    releases: &[ReleaseManifest],
    collateral: &RelayedCollateral,
    root_pem: Option<&str>,
    now: i64,
) -> anyhow::Result<VerifiedAttestation> {
    let quote = unhex(&attestation.quote).context("decoding the quote")?;
    let (report, validity) = signed_report(&quote, collateral, root_pem, now)?;

    let (bundle, report_data) = served_bundle(attestation)?;
    ensure!(report.report_data[..] == report_data[..], "the quote's report data differs from the served one");

    let (release_name, config_name) = match_release(&report, &bundle, releases)?;
    Ok(VerifiedAttestation {
        release_name,
        enclave_address: bundle.enclave_address,
        config_name,
        tls_public_key_sha256: bundle.tls_public_key_sha256,
        validity,
    })
}

/// Verifies that `quote` is an SEV-SNP report from a chip whose certificate chain in `collateral`
/// verifies to the compiled-in vendor roots, plus `root_pem` when given, unrevoked at `now` in Unix
/// seconds, and returns what it reports. Unlike `verify_attestation` it matches no release: the
/// caller decides whether the measurement, policy, TCB, and report data are ones it accepts.
/// Error cases:
/// - as `verify_attestation`, up to and including the quote's signature
pub fn verify_quote(
    quote: &[u8],
    collateral: &RelayedCollateral,
    root_pem: Option<&str>,
    now: i64,
) -> anyhow::Result<VerifiedQuote> {
    let (report, validity) = signed_report(quote, collateral, root_pem, now)?;
    Ok(VerifiedQuote {
        measurement: report.measurement,
        policy: report.policy,
        reported_tcb: report.reported_tcb,
        report_data: report.report_data,
        validity,
    })
}

/// `quote` parsed as an SEV-SNP report once its chain, revocation list, and signature verify, and
/// the window in which those date checks pass.
/// Error cases:
/// - `now` is out of the certificate time range
/// - the quote is not an SEV-SNP attestation report, its chain does not verify to a pinned root, the
///   list revokes it, or its signature does not verify
fn signed_report<'a>(
    quote: &'a [u8],
    collateral: &RelayedCollateral,
    root_pem: Option<&str>,
    now: i64,
) -> anyhow::Result<(SnpReport<'a>, Window)> {
    let now = ASN1Time::from_timestamp(now).map_err(|_| anyhow::anyhow!("the time {now} is out of range"))?;
    let report = SnpReport::parse(quote)
        .context("the quote is not an SEV-SNP attestation report; a mock quote never verifies")?;
    let chain = QuoteChain::verify(
        collateral.vcek.clone(),
        collateral.intermediate.clone(),
        collateral.root.clone(),
        &roots(root_pem)?,
        now,
    )?;
    let current = collateral::check_collateral(&chain, &collateral.crl, &report, now)?;
    let certificates = chain.validity()?;
    let validity = Window { from: certificates.from.max(current.from), until: certificates.until.min(current.until) };
    Ok((report, validity))
}

/// What a verified quote reports about the enclave that produced it.
#[derive(Debug)]
pub struct VerifiedQuote {
    /// The launch measurement the firmware recorded.
    pub measurement: [u8; 48],
    /// The guest policy the launch ran under.
    pub policy: u64,
    /// The TCB the report was issued under.
    pub reported_tcb: Tcb,
    /// The report data the quote binds.
    pub report_data: [u8; 64],
    /// The Unix seconds in which every date check passes (see `VerifiedAttestation::validity`).
    pub validity: Window,
}

/// Reads a mock attestation, for development only: the bundle its report data binds, when the quote
/// is the mock marker followed by that report data. Nothing proves the bundle came from an enclave,
/// so a caller that accepts one says so wherever it uses the result.
/// Error cases:
/// - the bundle is not bound by the report data, or the quote is not the mock marker and the report
///   data
pub fn verify_mock_attestation(attestation: &Attestation) -> anyhow::Result<AttestationBundle> {
    let (bundle, report_data) = served_bundle(attestation)?;
    let quote = unhex(&attestation.quote).context("quote")?;
    ensure!(
        quote == [MOCK_QUOTE_MARKER, &report_data[..]].concat(),
        "the quote is not a mock quote for the report data"
    );
    Ok(bundle)
}

/// The bundle `attestation` serves and the report data that binds it, after checking that the
/// report data is the bundle's SHA-256 and that the served fields are the bundle's. The caller
/// still checks that a quote vouches for the report data.
/// Error cases:
/// - the bundle does not decode, the report data is not the bundle's hash padded with zeros, or a
///   served field differs from the bundle's
fn served_bundle(attestation: &Attestation) -> anyhow::Result<(AttestationBundle, Vec<u8>)> {
    let bundle = AttestationBundle::decode(&unhex(&attestation.bundle)?)
        .context("the served bundle is not an attestation bundle")?;
    let report_data = unhex(&attestation.report_data)?;
    ensure!(report_data.len() == 64, "the served report data is not 64 bytes");
    ensure!(
        report_data[..32] == sha2::Sha256::digest(bundle.encode())[..],
        "the served report data is not the served bundle's SHA-256"
    );
    ensure!(report_data[32..] == [0u8; 32], "the served report data's reserved half is not zero");
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
    Ok((bundle, report_data))
}

/// The name of the first release in `releases` that admits the quote, and of the configuration it
/// runs.
/// Error cases:
/// - `releases` is empty
/// - no release admits the quote: one release's own refusal, or every release's refusal by name
fn match_release(
    report: &SnpReport<'_>,
    bundle: &AttestationBundle,
    releases: &[ReleaseManifest],
) -> anyhow::Result<(String, String)> {
    let mut refusals = Vec::new();
    for release in releases {
        match check_release(report, bundle, release) {
            Ok(config_name) => return Ok((release.name.clone(), config_name)),
            Err(err) => refusals.push((release.name.as_str(), err)),
        }
    }
    if refusals.len() > 1 {
        let each: Vec<String> = refusals.iter().map(|(name, err)| format!("{name}: {err:#}")).collect();
        bail!("the quote matches none of the {} release manifests: {}", refusals.len(), each.join("; "));
    }
    Err(refusals.pop().map(|(_, err)| err).unwrap_or_else(|| anyhow::anyhow!("no release manifest to verify against")))
}

/// The configuration `release` runs that the quote's bundle names, once the quote's measurement,
/// policy, and reported TCB match the release.
/// Error cases:
/// - the measurement, policy, or TCB does not match, or the bundle's configuration hash is not listed
fn check_release(
    report: &SnpReport<'_>,
    bundle: &AttestationBundle,
    release: &ReleaseManifest,
) -> anyhow::Result<String> {
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
    Ok(release
        .config_name(&bundle.config_sha256)
        .context("the bundle's configuration hash matches no release configuration")?
        .to_owned())
}

/// The compiled-in vendor roots, plus `root_pem` when given.
/// Error cases:
/// - `root_pem` holds no certificate
fn roots(root_pem: Option<&str>) -> anyhow::Result<Roots> {
    let mut roots = Roots::vendor();
    if let Some(pem) = root_pem {
        roots.add_pem(pem).context("loading the extra root")?;
    }
    Ok(roots)
}

/// Decodes hex with or without a `0x` prefix.
/// Error cases:
/// - the text is not hex
fn unhex(text: &str) -> anyhow::Result<Vec<u8>> {
    Ok(hex::decode(text.strip_prefix("0x").unwrap_or(text))?)
}
