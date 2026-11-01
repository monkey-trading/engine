//! Row authentication for the marks cache: the coverage a backfill recorded, the HMAC that
//! binds every stored row to the enclave key that wrote it, and the chained-index head that
//! anchors each market/resolution chain at recovery re-derivation.

use engine_core::types::{MarketId, Millis};

use crate::history::marks::Resolution;

/// What one backfill recorded as its coverage: the depths the config asked for and the retentions
/// that pruned the seed. A later config that asks for more re-runs the backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Coverage {
    pub depths: (Millis, Millis, Millis),
    pub retentions: (Millis, Millis, Millis, Option<Millis>),
}

/// One chain's committed head: the newest bucket's window start and its authentication tag, as
/// anchored in memory at recovery re-derivation and advanced by every write. A file whose head
/// differs from the anchor is a rollback the reader refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChainHead {
    pub bucket: Millis,
    pub tag: [u8; 16],
}

/// The row fields a tag covers: the OHLC, provenance, and the chained index's predecessor bucket,
/// as stored. `prev` is `None` for the chain's floor row.
pub(crate) struct Row<'a> {
    pub open: &'a str,
    pub high: &'a str,
    pub low: &'a str,
    pub close: &'a str,
    pub provenance: &'a str,
    pub prev: Option<Millis>,
}

/// The authentication tag of one row: the first half of an HMAC of every stored field under the
/// enclave key's marks derivation, so a row the host rewrote fails verification.
pub(crate) fn tag(
    mac: &hmac::Hmac<sha2::Sha256>,
    market: MarketId,
    resolution: Resolution,
    bucket: Millis,
    row: &Row,
) -> [u8; 16] {
    use hmac::Mac;
    let mut mac = mac.clone();
    for number in [i64::from(market), resolution as i64, bucket] {
        mac.update(&number.to_be_bytes());
        mac.update(&[0]);
    }
    for text in [row.open, row.high, row.low, row.close, row.provenance] {
        mac.update(text.as_bytes());
        mac.update(&[0]);
    }
    match row.prev {
        Some(prev) => {
            mac.update(&[1]);
            mac.update(&prev.to_be_bytes());
        }
        None => mac.update(&[0]),
    }
    mac.finalize().into_bytes()[..16].try_into().expect("16 bytes from a 32-byte digest")
}
