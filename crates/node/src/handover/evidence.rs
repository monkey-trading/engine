//! A handover peer's evidence: a quote over the report data the step binds, with the vendor chain
//! that verifies it, or, between mock enclaves, a mock quote and a claimed stand-in measurement.

use anyhow::{Context, ensure};
use engine_verify::{RelayedCollateral, SnpReport, Tcb, verify_quote};
use serde::{Deserialize, Serialize};

use crate::config::{TeeProvider, TeeSection};
use crate::tee::sealing::Platform;
use crate::tee::{Attestor, MOCK_QUOTE_MARKER};
use crate::verify::Collateral;

/// A quote, and what verifies it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Evidence {
    /// The attestor's provider name.
    provider: String,
    /// The quote, hex.
    quote: String,
    /// The vendor collateral for a hardware quote, as `/v1/attestation/collateral` serves it.
    #[serde(default)]
    collateral: Option<String>,
    /// The mock TEE's stand-in measurement, hex: a claim nothing proves.
    #[serde(default)]
    mock_launch_digest: Option<String>,
}

/// What verified evidence shows about the peer's release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Peer {
    pub launch_digest: [u8; 48],
    pub policy: u64,
    /// The TCB a hardware quote was issued under.
    pub tcb: Option<Tcb>,
}

/// Where this enclave's quotes come from, and their collateral.
pub(crate) struct Quoting<'a> {
    pub provider: TeeProvider,
    /// The vendor's processor name and key-distribution service, for hardware collateral.
    pub processor: Option<&'a str>,
    pub kds_base: &'a str,
}

impl<'a> From<&'a TeeSection> for Quoting<'a> {
    /// The quoting `[tee]` configures.
    fn from(tee: &'a TeeSection) -> Self {
        Self { provider: tee.provider, processor: tee.processor.as_deref(), kds_base: &tee.kds_base }
    }
}

/// This enclave's evidence over `report_data`, fetching the vendor chain for a hardware quote.
/// Error cases:
/// - the attestor cannot quote, or a hardware quote is not an SEV-SNP report, `tee.processor` is
///   unset, or its collateral cannot be fetched
pub(crate) async fn produce(
    quoting: &Quoting<'_>,
    attestor: &dyn Attestor,
    platform: &Platform,
    report_data: [u8; 64],
) -> anyhow::Result<Evidence> {
    let (provider, quote) = attestor.attest(report_data)?;
    let (collateral, mock_launch_digest) = match quoting.provider {
        TeeProvider::Mock => (None, Some(hex::encode(platform.launch_digest))),
        TeeProvider::ConfigfsTsm => {
            let report = SnpReport::parse(&quote).context("a handover needs an SEV-SNP quote")?;
            let processor = quoting.processor.context("tee.processor is required to fetch the quote's collateral")?;
            let collateral = Collateral::new(quoting.kds_base, processor)?.relayed(&report).await?;
            (Some(String::from_utf8(collateral.to_json()).context("collateral is JSON")?), None)
        }
    };
    Ok(Evidence { provider, quote: hex::encode(quote), collateral, mock_launch_digest })
}

/// What `evidence` shows, once it binds `report_data`. A real enclave takes a hardware quote whose
/// chain verifies to the vendor roots, plus `root_pem` in tests, unrevoked at `now_seconds`; a mock
/// enclave takes only a mock quote.
/// Error cases:
/// - the evidence is the other kind than `own` takes, or does not bind `report_data`
/// - a hardware quote does not verify (see `engine_verify::verify_quote`), or has no time to check
///   its chain at; a mock claim is not a 48-byte measurement
pub(crate) fn verify(
    own: TeeProvider,
    evidence: &Evidence,
    report_data: &[u8; 64],
    now_seconds: Option<i64>,
    root_pem: Option<&str>,
) -> anyhow::Result<Peer> {
    let quote = hex::decode(&evidence.quote).context("the peer's quote is not hex")?;
    match own {
        TeeProvider::Mock => {
            ensure!(evidence.provider == "mock", "a mock enclave hands over to mock enclaves only");
            ensure!(quote == [MOCK_QUOTE_MARKER, &report_data[..]].concat(), "the peer's mock quote binds other data");
            let digest = evidence.mock_launch_digest.as_deref().context("the mock peer names no measurement")?;
            let launch_digest = hex::decode(digest)
                .ok()
                .and_then(|digest| digest.try_into().ok())
                .context("the mock peer's measurement is not 48 bytes of hex")?;
            Ok(Peer { launch_digest, policy: 0, tcb: None })
        }
        TeeProvider::ConfigfsTsm => {
            ensure!(evidence.provider != "mock", "a real enclave never hands over to a mock one");
            let collateral = evidence.collateral.as_deref().context("the peer's quote comes without collateral")?;
            let collateral = RelayedCollateral::from_json(collateral.as_bytes())?;
            let now = now_seconds.context("no trusted time to check the peer's certificates at")?;
            let verified = verify_quote(&quote, &collateral, root_pem, now)?;
            ensure!(verified.report_data == *report_data, "the peer's quote binds other data");
            Ok(Peer { launch_digest: verified.measurement, policy: verified.policy, tcb: Some(verified.reported_tcb) })
        }
    }
}
