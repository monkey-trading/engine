use super::*;

use std::path::Path;

use crate::archive::read::origin::{log_of, upload};
use crate::archive::stand_in::{StandIn, checkpoints, tail_key, test_log};
use crate::archive::{compress, piece_bytes, range_bytes};
use crate::log::Manifest;

/// The log reads through a checkpoint from the bucket's ranges until the first it lacks, then from the checkpoint's
/// tail, the bytes of the log's lines up to and including the checkpoint's entry.
#[tokio::test]
async fn a_log_reads_through_a_checkpoint_from_the_ranges_and_then_its_tail() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("tail-read", 5).await);
    let (seq, root) = checkpoint_in(&dir, 3);
    upload(&bucket, &dir, &view, 2);
    upload_tail(&bucket, (&dir, &view), (seq, root), 1);
    let mut read = Vec::new();
    read_through(&bucket.client, &log(&bucket), (seq, root), &mut read).await.unwrap();
    assert_eq!(read, through(&dir, &view, seq));
    assert_eq!(
        bucket.with(|b| (b.read, b.tail_read)),
        (3, 2),
        "ranges 0 and 1 and the first lacked, then pieces 2 and 3"
    );
}

/// Where the bucket's ranges already hold the checkpoint's range, the log reads from them alone, cut after the
/// checkpoint's entry.
#[tokio::test]
async fn a_log_whose_ranges_reach_the_checkpoint_reads_from_them_alone() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("tail-read-ranges", 5).await);
    let (seq, root) = checkpoint_in(&dir, 3);
    upload(&bucket, &dir, &view, 4);
    let mut read = Vec::new();
    read_through(&bucket.client, &log(&bucket), (seq, root), &mut read).await.unwrap();
    assert_eq!(read, through(&dir, &view, seq));
    assert_eq!(bucket.with(|b| b.tail_read), 0);
}

/// A bucket that holds neither a range nor the tail's piece of it does not hold the log through the checkpoint, and
/// one whose tail holds the checkpoint at another root than the vault's is refused.
#[tokio::test]
async fn a_bucket_lacking_a_piece_or_holding_another_root_is_refused() {
    let (bucket, (dir, view)) = (StandIn::start().await, log_of("tail-read-refused", 5).await);
    let (seq, root) = checkpoint_in(&dir, 3);
    upload(&bucket, &dir, &view, 2);
    upload_tail(&bucket, (&dir, &view), (seq, root), 3);
    let e = read_through(&bucket.client, &log(&bucket), (seq, root), &mut Vec::new()).await.unwrap_err();
    let piece = log(&bucket).tail(seq, &root).unwrap().object(2).unwrap();
    assert!(format!("{e:#}").contains("holds neither range 2 at"), "{e:#}");
    assert!(format!("{e:#}").contains(piece.as_str()), "{e:#}");

    // The vault's tail, read under another root.
    upload_tail(&bucket, (&dir, &view), (seq, root), 2);
    let other = B256::repeat_byte(0xab);
    for index in 2..=seq / RANGE_SEQS {
        let object = bucket.with(|b| b.objects[&tail_key(seq, &root, index)].clone());
        bucket.with(|b| b.objects.insert(tail_key(seq, &other, index), object));
    }
    let e = read_through(&bucket.client, &log(&bucket), (seq, other), &mut Vec::new()).await.unwrap_err();
    let held = format!("at seq {seq} at root {}, not at {other}", root_at(&dir, seq));
    assert!(format!("{e:#}").contains(&held), "{e:#}");
    read_through(&bucket.client, &log(&bucket), (seq, root), &mut Vec::new()).await.unwrap();
}

// ── Helpers ──

/// The test log in `bucket`.
pub(crate) fn log(bucket: &StandIn) -> Bucket {
    Bucket::new(&bucket.url, &test_log()).unwrap()
}

/// The seq and root of the first checkpoint the log in `dir` holds in range `index`.
pub(crate) fn checkpoint_in(dir: &Path, index: u64) -> (u64, B256) {
    let (seq, root) = checkpoints(dir).into_iter().find(|(seq, _)| seq / RANGE_SEQS == index).unwrap();
    (seq, root.parse().unwrap())
}

/// The root the log in `dir` holds at `seq`, as its checkpoint spells it.
fn root_at(dir: &Path, seq: u64) -> String {
    checkpoints(dir).into_iter().find(|(at, _)| *at == seq).unwrap().1
}

/// Puts the pieces of the tail of the checkpoint at `seq` rooted at `root` into `bucket`, from piece `from`, built from
/// the log in `dir` as the uploader builds them.
pub(crate) fn upload_tail(bucket: &StandIn, (dir, view): (&Path, &Manifest), (seq, root): (u64, B256), from: u64) {
    for index in from..=seq / RANGE_SEQS {
        let object = compress(&piece_bytes(dir, view, index, seq).unwrap()).unwrap();
        bucket.with(|b| b.objects.insert(tail_key(seq, &root, index), object));
    }
}

/// The log in `dir`'s lines through the checkpoint at `seq`: its ranges before the checkpoint's, then the tail's
/// piece of the checkpoint's own.
pub(crate) fn through(dir: &Path, view: &Manifest, seq: u64) -> Vec<u8> {
    let last = seq / RANGE_SEQS;
    let mut lines: Vec<u8> = (0..last).flat_map(|index| range_bytes(dir, view, index).unwrap()).collect();
    lines.extend(piece_bytes(dir, view, last, seq).unwrap());
    lines
}
