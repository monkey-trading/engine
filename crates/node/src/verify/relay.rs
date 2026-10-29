//! The vendor collateral a verifier needs beyond the quote, relayed by the node so a browser that
//! may call only the node's origin can verify the attestation: the chip's certificate (VCEK), the
//! vendor intermediate and root, and the root's revocation list.
//!
//! The relay is safe only because a verifier never takes the node's word for any of it. It checks
//! the chain to a root pinned in its own binary, the VCEK against the quote's chip id and
//! reported TCB, the revocation list against that root, and the quote's signature against the
//! VCEK (see `verify_attestation`). A node that served other bytes could make a verifier refuse,
//! but never accept a chain the vendor did not issue. It could withhold a newer revocation list:
//! a served list stays valid until its `nextUpdate`, so revocation reaches a verifier as fast as
//! the node's fetches do. The node treats what it fetches as untrusted input, and checks it the
//! same way short of the root pin: a chain that does not verify for the node's own quote is never
//! served.

use anyhow::{Context, bail, ensure};
use engine_core::types::Millis;
use serde::{Deserialize, Serialize};
use x509_parser::prelude::FromDer as _;

use super::chain::{Collateral, QuoteChain, check_revocation};
use super::report::SnpReport;

/// The most bytes one relayed certificate or revocation list may carry. Vendor certificates are
/// about 1.3 kB and the root's list under 1 kB; a larger answer is not the vendor's. The sum,
/// hex-encoded, stays inside the verifier's fetch limit for the whole document.
const MAX_CERT_BYTES: usize = 16 * 1024;
const MAX_CRL_BYTES: usize = 256 * 1024;

/// The collateral for one quote, as DER.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayedCollateral {
    pub vcek: Vec<u8>,
    pub intermediate: Vec<u8>,
    pub root: Vec<u8>,
    pub crl: Vec<u8>,
    /// When the node fetched it, by the host's clock: informational, never a freshness proof.
    pub fetched_at: Millis,
}

/// What `GET /v1/attestation/collateral` serves: each item as `0x` hex DER.
#[derive(Serialize, Deserialize)]
struct CollateralDocument {
    vcek: String,
    intermediate: String,
    root: String,
    crl: String,
    fetched_at: Millis,
}

impl RelayedCollateral {
    /// Fetches `report`'s collateral through `collateral`, which bounds each answer's size and time
    /// and follows no redirect, then bounds each item (see `ensure_bounded`).
    /// Error cases:
    /// - the service does not answer, or an item exceeds its bound
    pub async fn fetch(collateral: &Collateral<'_>, report: &SnpReport<'_>) -> anyhow::Result<Self> {
        let (intermediate, root) = collateral.issuer_chain().await.context("fetching the issuer chain")?;
        let vcek = collateral.vcek(report).await.context("issuing the VCEK")?;
        let crl = collateral.crl().await.context("fetching the certificate revocation list")?;
        let fetched = Self { vcek, intermediate, root, crl, fetched_at: crate::clock::now_ms() };
        fetched.ensure_bounded()?;
        Ok(fetched)
    }

    /// Requires the collateral to link, bind to `report`, and sign it, as a verifier does except
    /// for the root pin: the chain is in date and linked, the VCEK certifies the report's chip and
    /// TCB, the list is the root's, current, and does not revoke the intermediate, and the VCEK
    /// signed the report. The dates are read from the host's clock, so a skewed clock can only make
    /// the node refuse collateral, never accept something a verifier would refuse.
    /// Error cases:
    /// - any of the checks above fails
    pub fn check(&self, report: &SnpReport<'_>) -> anyhow::Result<()> {
        let chain = QuoteChain::link(self.vcek.clone(), self.intermediate.clone(), self.root.clone())?;
        check_collateral(&chain, &self.crl, report)
    }

    /// The revocation list's `nextUpdate` in Unix seconds, if it names one.
    /// Error cases:
    /// - the list does not parse
    pub fn crl_next_update(&self) -> anyhow::Result<Option<i64>> {
        let (_, crl) = x509_parser::revocation_list::CertificateRevocationList::from_der(&self.crl)
            .map_err(|err| anyhow::anyhow!("parsing the certificate revocation list: {err:?}"))?;
        Ok(crl.next_update().map(|time| time.timestamp()))
    }

    /// The JSON the route serves.
    pub fn to_json(&self) -> Vec<u8> {
        let hex = |bytes: &[u8]| format!("0x{}", hex::encode(bytes));
        let document = CollateralDocument {
            vcek: hex(&self.vcek),
            intermediate: hex(&self.intermediate),
            root: hex(&self.root),
            crl: hex(&self.crl),
            fetched_at: self.fetched_at,
        };
        serde_json::to_vec(&document).expect("a document of strings serializes")
    }

    /// Reads the JSON a node served. The reader treats it as untrusted: every item is
    /// bounded here, and the verifier checks its content.
    /// Error cases:
    /// - the JSON is not a collateral document, an item is not hex, or an item exceeds its bound
    pub fn from_json(bytes: &[u8]) -> anyhow::Result<Self> {
        let document: CollateralDocument = serde_json::from_slice(bytes).context("parsing the collateral document")?;
        let unhex = |text: &str, what: &str| {
            hex::decode(text.strip_prefix("0x").unwrap_or(text)).with_context(|| format!("decoding the {what}"))
        };
        let collateral = Self {
            vcek: unhex(&document.vcek, "VCEK")?,
            intermediate: unhex(&document.intermediate, "intermediate")?,
            root: unhex(&document.root, "root")?,
            crl: unhex(&document.crl, "revocation list")?,
            fetched_at: document.fetched_at,
        };
        collateral.ensure_bounded()?;
        Ok(collateral)
    }

    /// Requires each item to be non-empty and within its size bound.
    fn ensure_bounded(&self) -> anyhow::Result<()> {
        for (what, bytes, max) in [
            ("VCEK", &self.vcek, MAX_CERT_BYTES),
            ("intermediate", &self.intermediate, MAX_CERT_BYTES),
            ("root", &self.root, MAX_CERT_BYTES),
            ("revocation list", &self.crl, MAX_CRL_BYTES),
        ] {
            if bytes.is_empty() {
                bail!("the {what} is empty");
            }
            ensure!(bytes.len() <= max, "the {what} is {} bytes, over the {max} limit", bytes.len());
        }
        Ok(())
    }
}

/// Checks `crl` against `chain`'s root and intermediate, the VCEK against `report`'s chip and
/// reported TCB, and `report`'s signature against the VCEK.
/// Error cases:
/// - the list is not current, not the root's, or revokes the intermediate, the VCEK does not
///   certify the report's chip and TCB, or the report's signature does not verify
pub(super) fn check_collateral(chain: &QuoteChain, crl: &[u8], report: &SnpReport<'_>) -> anyhow::Result<()> {
    chain.bind_vcek(report)?;
    check_revocation(crl, chain)?;
    report.verify_signature(&chain.vcek_public_key()?)
}
