//! The fold keeps each resolution for its own retention, as the cache does, and folds a bucket's
//! marks as the cache merges samples.

use super::*;

/// Each resolution drops the buckets older than its retention, counted back from the newest mark,
/// so a member that follows for long keeps what the cache would; a resolution kept unbounded keeps
/// every bucket.
#[test]
fn each_resolution_keeps_only_its_retention() {
    let retentions = Retentions { s1: 10_000, m1: 600_000, h1: 7_200_000, d1: None };
    let mut fold = MarkFold::new(retentions);
    let mark = |n: i64| Fixed::from_int(n);
    let day = 86_400_000;
    for (n, ts) in (0..).zip((0..3 * day).step_by(1_000 * 60 * 7)) {
        fold.sample(1, ts, mark(100 + n % 13));
    }
    let newest = fold.newest.unwrap();
    for resolution in Resolution::ALL {
        let buckets = &fold.buckets[resolution as usize];
        match retentions.of_resolution(resolution) {
            Some(retention) => assert!(
                buckets.keys().all(|(bucket, _)| *bucket >= newest - retention),
                "{resolution:?} kept a bucket past its retention"
            ),
            None => assert_eq!(buckets.len(), 3, "the daily buckets of three days"),
        }
    }
}

/// A bucket's marks fold as the cache merges them: the first opens it, the extremes widen it, and
/// the last closes it.
#[test]
fn a_bucket_opens_widens_and_closes_as_the_cache_merges() {
    let mut fold = MarkFold::default();
    for (ts, mark) in [(1_000, 5), (1_200, 9), (1_400, 2), (1_600, 6)] {
        fold.sample(7, ts, Fixed::from_int(mark));
    }
    let (buckets, newest) = fold.into_buckets().unwrap();
    assert_eq!(newest, 1_600);
    let second = buckets.iter().find(|(_, resolution, ..)| *resolution == Resolution::S1).unwrap();
    assert_eq!(second.3, [5, 9, 2, 6].map(Fixed::from_int));
}
