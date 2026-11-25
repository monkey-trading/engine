//! List and queue headers are stored as fixed-width fields and read back only from exactly those bytes.

use super::*;

use crate::history::authenticated::model::{Payload, Queued, Stored};

#[test]
fn each_header_reads_back_from_exactly_its_stored_bytes() {
    let metadata = Metadata { generation: 7, lo: 3, hi: u64::MAX };
    let row = Row {
        account: A,
        kind: 9,
        cursor: Offset { seq: u64::MAX - 1, index: 4 },
        ts: 1_795_520_414_000,
        payload: Payload { length: 312, chunks: 1 },
    };
    let queued = Queued {
        account: B,
        kind: 2,
        all: Position { generation: 5, rank: 11 },
        by_kind: Position { generation: 6, rank: 0 },
    };
    check(&metadata, 24);
    check(&row, 57);
    check(&queued, 53);
}

// ── Helpers ──

/// `value` encodes to `length` bytes, which decode back to it, while a byte fewer or a byte more is refused.
fn check<T: Stored + PartialEq + std::fmt::Debug>(value: &T, length: usize) {
    let bytes = value.encode();
    assert_eq!(bytes.len(), length, "{value:?}");
    assert_eq!(&T::decode(&bytes).expect("its own bytes"), value);
    assert!(T::decode(&bytes[..length - 1]).is_err(), "a byte short: {value:?}");
    assert!(T::decode(&[bytes.as_slice(), &[0]].concat()).is_err(), "a byte past: {value:?}");
}
