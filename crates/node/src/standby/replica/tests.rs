//! A follower's start from its replica: the copy is not read again.

use std::sync::Arc;

use engine_core::command::Command;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use super::*;
use crate::log::{Manifest, SegmentedLog};
use crate::sequencer::{Request, Sequencer};
use crate::tee::EnclaveKey;
use crate::test_support::{SEGMENT_COMMANDS, log_entries, sample_around, scratch_dir, sequencer_config};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// Once the replica is built, the sequencer starts from it and the copy's open writer: a host that
/// alters the copy's files after the build changes nothing the start reads, and the next command
/// follows the replica's last.
#[tokio::test]
async fn a_follower_starts_the_sequencer_from_its_replica_without_reading_the_copy_again() {
    let dir = scratch_dir("replica-start");
    crate::standby::tests::ticks(&dir, 10).await;
    let follower = Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    let last = follower.replica().last_seq().expect("the copy holds commands");
    let now = follower.replica().engine().now();

    // The host alters a completed segment: a start reading the copy again would refuse it.
    let listed = Manifest::load(&dir).unwrap().segments[0].clone();
    let mut bytes = std::fs::read(dir.join(&listed.file)).unwrap();
    bytes[1] ^= 0x01;
    std::fs::write(dir.join(&listed.file), bytes).unwrap();
    assert!(SegmentedLog::open(&dir, |_| Ok(())).is_err(), "reading the altered copy again succeeds");

    let log = follower.into_log(None, signer(), None).unwrap();
    let (manifest, published) = watch::channel(Manifest::default());
    let (stream, _) = broadcast::channel(64);
    let (withdrawals, _custody) = mpsc::unbounded_channel();
    let key = Arc::new(EnclaveKey::test_key());
    let mut sequencer = Sequencer::recover_from(log, sequencer_config(&dir, manifest), key, stream, withdrawals)
        .expect("the sequencer starts from the replica");
    sequencer.trust(sample_around(&crate::clock::SystemClock)).unwrap();
    let (tx, rx) = mpsc::channel(8);
    let task = tokio::spawn(sequencer.run(rx));
    let (reply, events) = oneshot::channel();
    let query = Box::new(move |engine: &engine_core::Engine| {
        let _ = reply.send((engine.last_seq(), engine.now()));
    });
    tx.send(Request::Query(query)).await.unwrap();
    assert_eq!(events.await.unwrap(), (Some(last), now), "the sequencer's engine is not the replica's");
    let (reply, applied) = oneshot::channel();
    tx.send(Request::Submit { cmd: Command::Tick, reply: Some(reply) }).await.unwrap();
    applied.await.unwrap();
    drop(tx);
    task.await.unwrap().unwrap();
    assert_eq!(published.borrow().flushed.map(|flushed| flushed.seq), Some(last + 1), "the writer published nothing");
    let appended = log_entries_after(&dir, &listed.file, last);
    assert!(
        appended.iter().any(|entry| matches!(entry, LogEntry::Command(command) if command.sequenced.seq == last + 1)),
        "the next command did not follow the replica's last: {appended:?}"
    );
}

/// A handover's tail extends the copy and its replica, entry by entry, after the copy's tip; the
/// entries the copy already holds past its last command repeat it and are not appended again.
#[tokio::test]
async fn a_tail_extends_the_copy_and_the_replica_after_the_tip() {
    let operator = scratch_dir("replica-tail-operator");
    crate::standby::tests::ticks(&operator, 6).await;
    let dir = scratch_dir("replica-tail-copy");
    crate::standby::tests::copy_log(&operator, &dir);
    crate::standby::tests::ticks(&operator, 5).await;
    let mut follower =
        Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    let (tip, _) = follower.tip().unwrap();
    let held = log_entries(&dir).len();
    let tail = lines_after(&operator, tip);

    let taken = follower.take_tail(Offset::start_of(tip), &tail).unwrap();
    let expected = log_entries(&operator);
    assert_eq!(taken as usize, expected.len() - held, "the copy did not take exactly the entries it lacked");
    drop(follower);
    assert_eq!(log_entries(&dir), expected, "the copy is not the operator's log");

    // A tail whose entry the host altered does not extend the copy.
    let copy = scratch_dir("replica-tail-forged");
    crate::standby::tests::copy_log(&dir, &copy);
    crate::standby::tests::ticks(&operator, 2).await;
    let mut follower =
        Follower::open(&copy, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    let (tip, _) = follower.tip().unwrap();
    let forged = String::from_utf8(lines_after(&operator, tip)).unwrap().replacen("\"ts\":", "\"ts\":1", 1);
    let err =
        follower.take_tail(Offset::start_of(tip), forged.as_bytes()).expect_err("a forged entry extended the copy");
    assert!(crate::standby::rollback_of(&err).is_some(), "{err:#}");
}

/// Each command of a tail a follower takes reaches its stats worker, so the start that follows the take
/// finds the worker at the replica's last command.
#[tokio::test]
async fn a_tail_reaches_the_followers_stats_worker() {
    let operator = scratch_dir("replica-stats-operator");
    crate::standby::tests::ticks(&operator, 4).await;
    let dir = scratch_dir("replica-stats-copy");
    crate::standby::tests::copy_log(&operator, &dir);
    crate::standby::tests::ticks(&operator, 3).await;
    let stats = crate::test_support::stats();
    let records = Some(stats.clone());
    let mut follower =
        Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), records, |_| {}).unwrap();
    let (tip, _) = follower.tip().unwrap();
    follower.take_tail(Offset::start_of(tip), &lines_after(&operator, tip)).unwrap();
    let last = follower.replica().last_seq();
    assert!(last > Some(tip), "the tail extended the replica");
    assert_eq!(crate::stats::tests::worker_seq(&stats), last, "the worker is not at the replica's last command");
}

/// A tail taken in pages goes on from the copy's last entry, even one past its tip's command, as a gap
/// fetch's next page does when the last ended among the entries that follow a command: it extends the
/// copy exactly as one page would.
#[tokio::test]
async fn a_tail_goes_on_from_an_entry_past_the_tips_command() {
    let operator = scratch_dir("replica-tail-pages-operator");
    crate::standby::tests::ticks(&operator, 6).await;
    let dir = scratch_dir("replica-tail-pages-copy");
    crate::standby::tests::copy_log(&operator, &dir);
    crate::standby::tests::ticks(&operator, 9).await;
    let mut follower =
        Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    let (tip, _) = follower.tip().unwrap();
    // The first page ends at the first entry past a command: a checkpoint.
    let mut previous = None;
    let offsets: Vec<(Offset, LogEntry)> = log_entries(&operator)
        .into_iter()
        .map(|entry| {
            let offset = offset_of(previous, &entry).unwrap();
            previous = Some(offset);
            (offset, entry)
        })
        .collect();
    let split = offsets
        .iter()
        .find(|(offset, entry)| *offset > Offset::start_of(tip) && !matches!(entry, LogEntry::Command(_)))
        .map(|(offset, _)| *offset)
        .expect("a checkpoint past the tip");
    let page = |from: Offset, through: Offset| {
        let mut bytes = Vec::new();
        for (_, entry) in offsets.iter().filter(|(offset, _)| *offset > from && *offset <= through) {
            bytes.extend(serde_json::to_vec(entry).unwrap());
            bytes.push(b'\n');
        }
        bytes
    };
    follower.take_tail(Offset::start_of(tip), &page(Offset::start_of(tip), split)).unwrap();
    assert_eq!(follower.head(), Some(split), "the first page ended past a command");
    let last = offsets.last().unwrap().0;
    let taken = follower.take_tail(split, &page(split, last)).unwrap();
    assert!(taken > 0, "the next page took nothing");
    drop(follower);
    assert_eq!(log_entries(&dir), log_entries(&operator), "the copy is not the operator's log");
}

/// A follower opened from its copy serves, once its member link links it, the entries it replayed: a
/// restarted member answers a gap fetch from what it holds, not only from what arrives after.
#[tokio::test]
async fn a_reopened_follower_serves_the_entries_it_replayed() {
    let dir = scratch_dir("replica-served");
    crate::standby::tests::ticks(&dir, 4).await;
    let mut follower =
        Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    let copy = crate::liveness::link::Copy::default();
    follower.link_with(copy.clone());
    let mut previous = None;
    let held: Vec<(Offset, LogEntry)> = log_entries(&dir)
        .into_iter()
        .map(|entry| {
            let offset = offset_of(previous, &entry).unwrap();
            previous = Some(offset);
            (offset, entry)
        })
        .filter(|(offset, _)| *offset > Offset::start_of(1))
        .collect();
    assert!(!held.is_empty());
    assert_eq!(copy.entries_after(Some(Offset::start_of(1))), held, "the reopened copy serves what it holds");
}

/// What a follower takes after its link was made joins the entries the link's ring was given, so the ring
/// serves them without a hole.
#[tokio::test]
async fn a_followers_link_serves_the_entries_it_took_without_a_hole() {
    let operator = scratch_dir("replica-served-log-operator");
    crate::standby::tests::ticks(&operator, 6).await;
    let dir = scratch_dir("replica-served-log-copy");
    crate::standby::tests::copy_log(&operator, &dir);
    crate::standby::tests::ticks(&operator, 9).await;
    let mut follower =
        Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    let copy = crate::liveness::link::Copy::default();
    follower.link_with(copy.clone());
    let after = follower.head().unwrap();
    let (tip, _) = follower.tip().unwrap();
    assert!(follower.take_tail(Offset::start_of(tip), &lines_after(&operator, tip)).unwrap() > 0);
    let mut previous = None;
    let taken: Vec<(Offset, LogEntry)> = log_entries(&operator)
        .into_iter()
        .map(|entry| {
            let offset = offset_of(previous, &entry).unwrap();
            previous = Some(offset);
            (offset, entry)
        })
        .filter(|(offset, _)| *offset > after)
        .collect();
    assert_eq!(copy.entries_after(Some(after)), taken, "the ring was given what the copy took");
    assert_eq!(copy.entries(Some(after)).await, taken);
}

/// Entries that start the log take onto an empty copy and onto no other.
#[tokio::test]
async fn entries_from_the_first_take_onto_an_empty_copy_only() {
    let operator = scratch_dir("replica-first-operator");
    crate::standby::tests::ticks(&operator, 5).await;
    let mut whole = Vec::new();
    for entry in log_entries(&operator) {
        whole.extend(serde_json::to_vec(&entry).unwrap());
        whole.push(b'\n');
    }
    let dir = scratch_dir("replica-first-copy");
    let mut follower =
        Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    assert!(follower.take_entries(None, &whole).unwrap() > 0);
    let refused =
        follower.take_entries(None, &whole).expect_err("a copy that holds commands takes no start of the log");
    assert!(format!("{refused:#}").contains("start the log"), "{refused:#}");
    drop(follower);
    assert_eq!(log_entries(&dir), log_entries(&operator), "the empty copy took the whole log");
}

/// A copy that took entries before it was linked serves none of them from a ring: the ring holds what the
/// copy opened with and what it took after the link, which would leave the entries between out of a page.
/// The link serves the page from the copy's log on disk, which chains.
#[tokio::test]
async fn a_link_made_after_the_copy_took_entries_never_serves_a_ring_with_a_hole() {
    let operator = scratch_dir("replica-hole-operator");
    crate::standby::tests::ticks(&operator, 6).await;
    let dir = scratch_dir("replica-hole-copy");
    crate::standby::tests::copy_log(&operator, &dir);
    crate::standby::tests::ticks(&operator, 9).await;
    let mut follower =
        Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer(), [signer()].into(), None, |_| {}).unwrap();
    let after = follower.head().unwrap();
    let (tip, _) = follower.tip().unwrap();
    let mut previous = None;
    let log: Vec<(Offset, LogEntry)> = log_entries(&operator)
        .into_iter()
        .map(|entry| {
            let offset = offset_of(previous, &entry).unwrap();
            previous = Some(offset);
            (offset, entry)
        })
        .filter(|(offset, _)| *offset > after)
        .collect();
    let lines = |entries: &[(Offset, LogEntry)]| {
        entries
            .iter()
            .flat_map(|(_, entry)| [serde_json::to_vec(entry).unwrap(), vec![b'\n']].concat())
            .collect::<Vec<u8>>()
    };
    // The first page, taken unlinked, ends at the next command; the second after the link.
    let split = log
        .iter()
        .position(|(offset, entry)| *offset > Offset::start_of(tip) && matches!(entry, LogEntry::Command(_)))
        .unwrap()
        + 1;
    assert!(follower.take_tail(after, &lines(&log[..split])).unwrap() > 0);
    let copy = crate::liveness::link::Copy::default();
    follower.link_with(copy.clone());
    assert!(follower.take_tail(log[split - 1].0, &lines(&log[split..])).unwrap() > 0);
    assert_eq!(copy.entries_after(Some(after)), Vec::new(), "no ring is served across the entries it lacks");
    assert_eq!(copy.entries(Some(after)).await, log, "the copy's log serves them");
}

/// An empty copy takes the log's first entries, sealed by a key an earlier signer change replaced, once
/// the vault's recorded signers are admitted, and its replica holds them.
#[tokio::test]
async fn an_empty_copy_admits_the_recorded_signers_for_the_first_entries() {
    let operator = scratch_dir("replica-first-signers-operator");
    crate::standby::tests::ticks(&operator, 4).await;
    let mut whole = Vec::new();
    for entry in log_entries(&operator) {
        whole.extend(serde_json::to_vec(&entry).unwrap());
        whole.push(b'\n');
    }
    // The followed enclave is another key than the one that sealed the log; the vault records both.
    let followed = AccountId([0x77; 20]);
    let open = |name: &str, signers: std::collections::BTreeSet<AccountId>| {
        let dir = scratch_dir(name);
        Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, followed, signers, None, |_| {}).unwrap()
    };
    let mut unrecorded = open("replica-first-signers-bare", [followed].into());
    assert!(unrecorded.take_entries(None, &whole).is_err(), "a seal of an unrecorded key extends nothing");
    let mut recorded = open("replica-first-signers-recorded", [followed, signer()].into());
    assert!(recorded.take_entries(None, &whole).unwrap() > 0);
}

/// A copy opened again between two pages of a log sealed by a key the vault recorded, before any checkpoint
/// of the copy names that key, takes the second page: the recorded signers are admitted again.
#[tokio::test]
async fn a_reopened_copy_takes_the_next_page_sealed_by_a_recorded_signer() {
    let operator = scratch_dir("replica-reopen-operator");
    crate::standby::tests::ticks(&operator, 6).await;
    let log: Vec<(Offset, LogEntry)> = {
        let mut previous = None;
        log_entries(&operator)
            .into_iter()
            .map(|entry| {
                let offset = offset_of(previous, &entry).unwrap();
                previous = Some(offset);
                (offset, entry)
            })
            .collect()
    };
    let lines = |entries: &[(Offset, LogEntry)]| {
        entries.iter().flat_map(|(_, e)| [serde_json::to_vec(e).unwrap(), vec![b'\n']].concat()).collect::<Vec<u8>>()
    };
    // The first page ends at a command, before the first checkpoint.
    let split = log.iter().position(|(offset, e)| offset.seq == 2 && matches!(e, LogEntry::Command(_))).unwrap() + 1;
    assert!(log[..split].iter().all(|(_, e)| !matches!(e, LogEntry::Checkpoint(_))), "no checkpoint names the key yet");
    let dir = scratch_dir("replica-reopen-copy");
    let followed = AccountId([0x77; 20]);
    let open = || Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, followed, [followed, signer()].into(), None, |_| {});
    let mut first = open().unwrap();
    assert!(first.take_entries(None, &lines(&log[..split])).unwrap() > 0);
    drop(first);
    let mut reopened = open().unwrap();
    let head = reopened.head();
    assert!(reopened.take_entries(head, &lines(&log[split..])).unwrap() > 0);
    drop(reopened);
    assert_eq!(log_entries(&dir), log_entries(&operator));
}

/// A copy cut back over commands a recorded signer other than the followed enclave sealed after its last
/// checkpoint, as a promoted epoch's before its first checkpoint, takes them again from the followed
/// operator's stream, at the open that cut it and at any later one: every open admits the recorded
/// signers, as a tail does.
#[tokio::test]
async fn a_cut_copy_takes_the_commands_it_dropped_again_from_the_stream() {
    let operator = scratch_dir("replica-cut-refill-operator");
    crate::standby::tests::ticks(&operator, 10).await;
    let uncovered = |dir: &std::path::Path| {
        let entries = log_entries(dir);
        let from = entries.iter().rposition(|entry| matches!(entry, LogEntry::Checkpoint(_))).unwrap();
        let seqs = entries[from..].iter().filter_map(|entry| match entry {
            LogEntry::Command(command) => Some(command.sequenced.seq),
            _ => None,
        });
        seqs.collect::<Vec<_>>()
    };
    while uncovered(&operator).len() < 3 {
        crate::test_support::drop_last_checkpoint(&operator);
    }
    let tail = uncovered(&operator);
    let (promoted, forger) = (EnclaveKey::generate(), EnclaveKey::generate());
    // Each command after a dropped checkpoint was sealed over it: the tail is sealed again, its first
    // command by the promoted key and the rest by the followed enclave.
    reseal(&operator, &promoted, &tail[..1]);
    reseal(&operator, &EnclaveKey::test_key(), &tail[1..]);
    let copy = scratch_dir("replica-cut-refill-copy");
    crate::standby::tests::copy_log(&operator, &copy);
    reseal(&copy, &forger, &tail[tail.len() - 1..]);
    let recorded: std::collections::BTreeSet<_> = [signer(), AccountId(promoted.address())].into();
    let open = || Follower::open(&copy, SEGMENT_COMMANDS, DOMAIN, signer(), recorded.clone(), None, |_| {}).unwrap();
    for reopened in [false, true] {
        let mut follower = open();
        assert_eq!(follower.replica().last_seq(), Some(tail[0] - 1), "the copy was not cut back to its checkpoint");
        refill(&mut follower, &operator, &tail, reopened);
    }
}

/// Streams the log in `operator` to `follower` from the copy's resume point, as a follow stint's chain checks
/// it, requiring every command of `tail` to extend the copy.
fn refill(follower: &mut Follower, operator: &std::path::Path, tail: &[u64], reopened: bool) {
    follower.copied.begin();
    let (mut previous, mut extended) = (None, Vec::new());
    for entry in log_entries(operator) {
        let offset = offset_of(previous, &entry).unwrap();
        previous = Some(offset);
        if offset.seq < follower.copied.resume_seq() {
            continue;
        }
        let seq = offset.seq;
        match follower.copied.check(offset, &entry) {
            Ok(Step::Extends(link)) => {
                extended.push(seq);
                follower.copied.extend(offset, entry, link);
            }
            Ok(_) => {}
            Err(refused) => panic!("reopened {reopened}: the stream's refill at seq {seq} was refused: {refused:?}"),
        }
    }
    assert!(
        tail.iter().all(|seq| extended.contains(seq)),
        "reopened {reopened}: the refill took {extended:?} of {tail:?}"
    );
}

// ── Helpers ──

/// Seals the commands at `seqs` of the log in `dir` with `key`, in order, each over the link before it.
fn reseal(dir: &std::path::Path, key: &EnclaveKey, seqs: &[u64]) {
    for seq in seqs {
        let mut link = B256::default();
        for entry in log_entries(dir) {
            match entry {
                LogEntry::Command(command) if command.sequenced.seq == *seq => break,
                LogEntry::Command(command) => link = crate::log::command_seal::command_link(link, &command).unwrap(),
                LogEntry::Checkpoint(checkpoint) => {
                    link = crate::log::command_seal::checkpoint_link(&checkpoint).unwrap()
                }
                LogEntry::Withdrawal(_) => {}
            }
        }
        crate::test_support::rewrite_command(dir, *seq, |command| {
            command.seal =
                Some(key.seal_command(DOMAIN, crate::log::command_seal::command_link(link, command).unwrap()));
        });
    }
}

/// The test sequencer's enclave, which seals and signs its log.
fn signer() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}

/// The JSON lines the log in `dir` holds after the command at `after`, as a predecessor sends them.
fn lines_after(dir: &std::path::Path, after: u64) -> Vec<u8> {
    let mut previous = None;
    let mut bytes = Vec::new();
    for entry in log_entries(dir) {
        let offset = offset_of(previous, &entry).unwrap();
        previous = Some(offset);
        if offset > Offset::start_of(after) {
            bytes.extend(serde_json::to_vec(&entry).unwrap());
            bytes.push(b'\n');
        }
    }
    bytes
}

/// The entries of the segments in `dir` named after `altered`, the segment the host altered, or
/// holding commands after `last`.
fn log_entries_after(dir: &std::path::Path, altered: &str, last: u64) -> Vec<LogEntry> {
    let mut entries = Vec::new();
    for file in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
        let name = file.file_name().to_string_lossy().into_owned();
        if name == altered || crate::log::segment_seq(&name).is_none_or(|first| first > last + 1) {
            continue;
        }
        let _ = crate::log::read_file(&file.path(), |entry| {
            entries.push(entry);
            Ok(())
        });
    }
    entries
}
