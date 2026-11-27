//! The log tail's dev scenario (`scripts/dev-log-tail.sh`): a dev node uploads its log to a loopback stand-in for the
//! bucket while a stand-in vault takes its checkpoints and finalizes each one's predecessor, the ranges below the
//! finalized checkpoint and the tail of each new one, before the vault finalizes it. A copy that parts from the vault's
//! history after an earlier checkpoint, as a returning operator's does, uploads its own tail under another key. Then,
//! with the node, the copy and every uploader stopped and no node or mirror serving anything, `engine exit-proof` and
//! `engine replay` read the log from the bucket alone through the vault's latest checkpoint, as from the node's disk.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use engine_core::command::Command;
use tokio::task::spawn_blocking;

use crate::PublishedLog;
use crate::archive::scenario::{until, uploading};
use crate::archive::stand_in::{StandIn, Vault, checkpoints, newest_checkpoint, test_log};
use crate::archive::{range_key, range_seqs};
use crate::exit::{ExitSubject, exit_bundle};
use crate::metrics::Metrics;
use crate::sequencer::replay_path_in_domain;
use crate::test_support::{TRADER, TestSequencer, deposit, scratch_dir};

#[tokio::test]
async fn dev_scenario_log_tail() {
    let (bucket, dir, vault) = (StandIn::start().await, scratch_dir("log-tail-scenario"), Vault::default());

    println!(
        "==> 1. a dev node posts its checkpoints to a stand-in vault, which finalizes each one's predecessor: the"
    );
    println!("       ranges below the finalized checkpoint go up, and each new checkpoint's tail before it is final");
    let node = TestSequencer::start(&dir, 256);
    node.submit(deposit(1_000, "log-tail-deposit")).await;
    let (uploader, sources, metrics) = uploading(&bucket, &dir, Duration::from_secs(3600), &vault);
    sources.send_replace(Some(node.manifest.clone()));
    let running = tokio::spawn(uploader.run());
    for _ in 0..3 {
        let finalized = vault.latest().map_or(0, |(seq, _)| seq);
        let (seq, _) = post_next(&node, &dir, &vault, &metrics).await;
        // A reader takes the ranges until the first the bucket lacks, then the tail's pieces from there.
        let last = seq / range_seqs();
        let first = bucket.with(|b| (0..=last).find(|index| !b.objects.contains_key(&range_key(&test_log(), *index))));
        let first = first.expect("the checkpoint's own range is not whole in the bucket");
        let pieces = tail_pieces(&bucket, seq);
        assert!((first..=last).all(|index| pieces.contains(&index)), "pieces {pieces:?} from range {first}");
        println!("    latest seq {seq}, finalized seq {finalized}: {first} ranges, then tail pieces {first} to {last}");
    }

    println!(
        "==> 2. a copy that parts from the vault's history after an earlier checkpoint, as a returning operator's"
    );
    println!("       does, uploads its own tail under another key, and the vault's tail stays as it was");
    node.stop().await;
    let copy = scratch_dir("log-tail-scenario-copy");
    copy_dir(&dir, &copy);
    let node = TestSequencer::start(&dir, 256);
    sources.send_replace(Some(node.manifest.clone()));
    let other = TestSequencer::start(&copy, 256);
    other.submit(deposit(2_000, "log-tail-other-deposit")).await;
    node.submit(Command::Tick).await;
    let (seq, root) = post_next(&node, &dir, &vault, &metrics).await;
    while other.next_seq().await <= seq {
        other.submit(Command::Tick).await;
    }
    let (others, other_root) =
        checkpoints(&copy).into_iter().find(|(at, _)| *at == seq).expect("the copy's checkpoint");
    assert_ne!(other_root, root, "the copy holds another history at the vault's checkpoint");
    let held = bucket.with(|b| b.objects.clone());
    let (copy_uploader, copy_sources, copy_metrics) = uploading(&bucket, &copy, Duration::from_secs(3600), &vault);
    copy_sources.send_replace(Some(other.manifest.clone()));
    let copy_running = tokio::spawn(copy_uploader.run());
    let (newest, _) = newest_checkpoint(&copy);
    until("the copy's tail", || copy_metrics.archive.tail_seq.get() as u64 == newest).await;
    let objects = bucket.with(|b| b.objects.clone());
    assert!(held.iter().all(|(key, object)| objects.get(key) == Some(object)), "every earlier object is as it was");
    let theirs = tail_dirs(&objects, seq);
    assert_eq!(theirs.len(), 2, "the vault's tail and the copy's at seq {seq}: {theirs:?}");
    println!("    at seq {others} the vault's tail is under {root} and the copy's under {other_root}");

    println!(
        "==> 3. the node, the copy and their uploaders stop, and no node or mirror serves the log: engine exit-proof"
    );
    println!(
        "       --log <the log's URL in the bucket> --seq <latestSeq> --state-root <latestRoot> prints the proofs"
    );
    println!("       the node's disk gives");
    running.abort();
    copy_running.abort();
    node.stop().await;
    other.stop().await;
    let (seq, root) = vault.latest().expect("the vault's latest checkpoint");
    let url = format!("{}{}", bucket.plain, test_log());
    let through = (seq, root.parse().unwrap());
    let read =
        spawn_blocking(move || url.parse::<PublishedLog>().unwrap().local(Some(through))).await.unwrap().unwrap();
    let proofs = |path: &Path| serde_json::to_string(&exit_bundle(path, seq, ExitSubject::Account(TRADER)).unwrap());
    assert_eq!(proofs(read.path()).unwrap(), proofs(&dir).unwrap());
    let house = |path: &Path| serde_json::to_string(&exit_bundle(path, seq, ExitSubject::House).unwrap());
    assert_eq!(house(read.path()).unwrap(), house(&dir).unwrap());
    println!("    the same proofs for {TRADER} and the house at seq {seq}, from the bucket alone");

    println!("==> 4. engine replay from the bucket alone reaches the vault's root, through the node's commands and");
    println!("       checkpoints up to it");
    let mut replayed = replay_path_in_domain(read.path(), None).unwrap();
    assert_eq!(format!("0x{}", hex::encode(replayed.engine.state_root())), root);
    assert_eq!(replayed.commands, seq + 1);
    let written = checkpoints(&dir).into_iter().filter(|(at, _)| *at <= seq).count() as u64;
    assert_eq!(replayed.checkpoints, written);
    println!("    {} commands, {} checkpoints, root {root}", replayed.commands, replayed.checkpoints);
    println!("PASS: the bucket alone holds the log through the vault's latest checkpoint");
}

// ── Helpers ──

/// Ticks `node` across two ranges past the vault's latest checkpoint, takes its newest checkpoint as the vault's latest
/// and finalizes the one the vault held before, as a vault finalizes one checkpoint at a time, and waits for the tail of
/// the new one; returns its seq and root.
async fn post_next(node: &TestSequencer, dir: &Path, vault: &Vault, metrics: &Metrics) -> (u64, String) {
    let from = vault.latest().map_or(0, |(seq, _)| seq);
    while newest_checkpoint(dir).0 < from + 2 * range_seqs() {
        node.submit(Command::Tick).await;
    }
    let (seq, root) = checkpoints(dir).pop().expect("a checkpoint");
    if let Some((previous, previous_root)) = vault.latest() {
        vault.finalize(previous, &previous_root);
    }
    vault.post(seq, &root);
    until("the new checkpoint's tail", || metrics.archive.tail_seq.get() as u64 == seq).await;
    (seq, root)
}

/// The ranges whose pieces the tail of the checkpoint at `seq` holds in `bucket`, in order.
fn tail_pieces(bucket: &StandIn, seq: u64) -> Vec<u64> {
    let tail = format!("{}tail/{seq:020}-", test_log());
    bucket.with(|b| {
        b.objects
            .keys()
            .filter_map(|key| key.strip_prefix(&tail))
            .filter_map(|rest| rest.split_once('/'))
            .map(|(_, piece)| piece.trim_end_matches(".jsonl.zst").parse::<u64>().unwrap() / range_seqs())
            .collect()
    })
}

/// The tails under the checkpoint at `seq` in `objects`, by their key prefix.
fn tail_dirs(objects: &BTreeMap<String, Vec<u8>>, seq: u64) -> Vec<String> {
    let tail = format!("{}tail/{seq:020}-", test_log());
    let dirs = objects.keys().filter(|key| key.starts_with(&tail)).filter_map(|key| key.rsplit_once('/'));
    let mut dirs: Vec<String> = dirs.map(|(dir, _)| dir.to_owned()).collect();
    dirs.dedup();
    dirs
}

/// Copies the log directory `dir`, everything in it, into `copy`.
fn copy_dir(dir: &Path, copy: &Path) {
    std::fs::create_dir_all(copy).unwrap();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let to = copy.join(path.file_name().unwrap());
        match path.is_dir() {
            true => copy_dir(&path, &to),
            false => drop(std::fs::copy(&path, &to).unwrap()),
        }
    }
}
