//! The root over a frozen copy's rows binds each row's key and every column it stores, takes rows only in key
//! order, and stays the value an earlier build wrote into its snapshots.

use super::*;

const BASE: Millis = 1_792_489_860_000;

/// A change to one column of a row.
type Change = fn(&mut SnapRow);

/// A root over a minute row and the minute after it, which names it below and binds its tag, and
/// changing any column of the second row, or its key, changes the root.
#[test]
fn the_root_binds_each_rows_key_and_every_column_it_stores() {
    let next = || row(BASE + 60_000, Some(BASE), Some([1; 16]));
    let root_of = |market: MarketId, resolution: Resolution, change: Change| {
        let mut changed = next();
        change(&mut changed);
        let mut root = Root::default();
        root.add(1, Resolution::M1, &row(BASE, None, None)).unwrap();
        root.add(market, resolution, &changed).unwrap();
        root.finish()
    };
    let base = root_of(1, Resolution::M1, |_| {});
    let changes: [(&str, Change); 9] = [
        ("open", |row| row.open = "67001".into()),
        ("high", |row| row.high = "67101".into()),
        ("low", |row| row.low = "66901".into()),
        ("close", |row| row.close = "67051".into()),
        ("provenance", |row| row.provenance = "seeded_from_provider".into()),
        ("prev", |row| row.prev = None),
        ("tag", |row| row.tag = [9; 16]),
        ("prev_tag", |row| row.prev_tag = None),
        ("bucket", |row| row.bucket = BASE + 120_000),
    ];
    for (column, change) in changes {
        assert_ne!(root_of(1, Resolution::M1, change), base, "the root does not bind {column}");
    }
    assert_ne!(root_of(2, Resolution::S1, |_| {}), base, "the root does not bind the market");
    assert_ne!(root_of(1, Resolution::H1, |_| {}), base, "the root does not bind the resolution");
    // A text moved from one column into the next is another row: each column is length-prefixed.
    let moved = root_of(1, Resolution::M1, |row| (row.open, row.high) = ("6700067100".into(), String::new()));
    assert_ne!(moved, base);
}

/// A row that does not come after the last one in key order, a negative bucket's included, is refused, and the
/// builder never sees it.
#[test]
fn rows_out_of_key_order_are_refused() {
    let mut root = Root::default();
    root.add(1, Resolution::M1, &row(BASE, None, None)).unwrap();
    for (market, resolution, bucket) in [
        (1, Resolution::M1, BASE),
        (1, Resolution::M1, BASE - 60_000),
        (1, Resolution::S1, BASE),
        (0, Resolution::D1, BASE),
    ] {
        let error = root.add(market, resolution, &row(bucket, None, None)).unwrap_err().to_string();
        assert!(error.contains("out of order"), "{market} {resolution:?} {bucket}: {error}");
    }
    root.add(1, Resolution::M1, &row(BASE + 60_000, Some(BASE), None)).unwrap();

    let mut signed = Root::default();
    signed.add(1, Resolution::S1, &row(-1_000, None, None)).unwrap();
    signed.add(1, Resolution::S1, &row(0, Some(-1_000), None)).unwrap();
    assert!(signed.add(1, Resolution::S1, &row(-2_000, None, None)).is_err(), "a negative bucket orders below zero");
}

/// No rows give the empty trie's root, and two rows give the root this build's snapshots carry, which a start of a
/// later build checks their copies against.
#[test]
fn the_root_is_the_one_snapshots_carry() {
    assert_eq!(Root::default().finish(), alloy_trie::EMPTY_ROOT_HASH);
    let mut root = Root::default();
    root.add(1, Resolution::M1, &row(BASE, None, None)).unwrap();
    root.add(1, Resolution::M1, &row(BASE + 60_000, Some(BASE), Some([1; 16]))).unwrap();
    assert_eq!(root.finish().to_string(), "0xf28ebb3f1e4154eb645b0ac225c8035f30ece10a84ec3fd361ceed70b2fdc165");
}

// ── Helpers ──

/// A log-derived row at `bucket` naming `prev` below it and binding `prev_tag`.
fn row(bucket: Millis, prev: Option<Millis>, prev_tag: Option<[u8; 16]>) -> SnapRow {
    SnapRow {
        bucket,
        open: "67000".into(),
        high: "67100".into(),
        low: "66900".into(),
        close: "67050".into(),
        provenance: "log_derived".into(),
        prev,
        tag: [2; 16],
        prev_tag,
    }
}
