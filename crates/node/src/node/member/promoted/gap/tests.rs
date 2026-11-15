//! The gap fetch's bound: every member is asked at once each round, a silent one costs one ask's wait
//! per round, and the fetch stops at its deadline.

use std::collections::BTreeMap;

use engine_core::command::Command;
use engine_core::types::AccountId;

use super::*;
use crate::log::{CheckpointDomain, LogEntry};
use crate::tee::EnclaveKey;
use crate::test_support::{SEGMENT_COMMANDS, TestSequencer, log_entries, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// Members by endpoint: the log one holds, each entry at its offset, of which it gives every entry past
/// the offset it is asked after, or `None` for one that never answers.
struct Members(BTreeMap<String, Option<Vec<(Offset, LogEntry)>>>);

impl Asking for Members {
    async fn ask(&self, endpoint: &str, request: &Request) -> anyhow::Result<Reply> {
        let Request::Entries { after } = request else { anyhow::bail!("only entries") };
        match self.0.get(endpoint).cloned().flatten() {
            Some(entries) => Ok(Reply::Entries { entries: entries.into_iter().filter(|(at, _)| at > after).collect() }),
            None => std::future::pending().await,
        }
    }
}

/// A member that gives at most one entry an answer: the first of its log past the offset it is asked
/// after.
struct Paging(Vec<(Offset, LogEntry)>);

impl Asking for Paging {
    async fn ask(&self, _: &str, request: &Request) -> anyhow::Result<Reply> {
        let Request::Entries { after } = request else { anyhow::bail!("only entries") };
        Ok(Reply::Entries { entries: self.0.iter().filter(|(at, _)| at > after).take(1).cloned().collect() })
    }
}

#[tokio::test]
async fn the_gap_fetch_asks_every_member_at_once_and_stops_at_its_deadline() {
    let (full, copy) = (scratch_dir("gap-full"), scratch_dir("gap-copy"));
    ticks(&full, 4).await;
    copy_dir(&full, &copy);
    ticks(&full, 3).await;
    let mut follower = open(&copy);
    // The logs are written: from here the test's clock runs only as the fetch waits.
    tokio::time::pause();
    let member = |byte: u8, endpoint: &str| SetMember {
        candidate: AccountId([byte; 20]),
        endpoint: endpoint.into(),
        standby: None,
        admin_key: AccountId([byte; 20]),
        held: false,
    };
    let members = [member(1, "silent:1"), member(2, "silent:2"), member(3, "giving:1")];
    let asking = Members(BTreeMap::from([
        ("silent:1".into(), None),
        ("silent:2".into(), None),
        ("giving:1".into(), Some(with_offsets(&full))),
    ]));
    let started = Instant::now();
    let (taken, refused) = from_members(&asking, &members, &mut follower, started + BUDGET).await;
    assert!(taken > 0 && refused.is_none());
    assert_eq!(follower.replica().last_seq(), open(&full).replica().last_seq(), "the copy took the whole gap");
    // Two rounds, the second finding nothing more: each waits one ask's time for the silent members.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= ASK * 2 && elapsed < ASK * 3,
        "silent members cost one wait per round, not one each: {elapsed:?}"
    );
    // With every member silent, the fetch ends at its deadline.
    let silent = Members(BTreeMap::from([("silent:1".into(), None)]));
    let started = Instant::now();
    let deadline = started + Duration::from_secs(3);
    assert_eq!(from_members(&silent, &members[..1], &mut open(&copy), deadline).await, (0, None));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_secs(3) && elapsed < Duration::from_secs(4), "{elapsed:?}");
}

/// A member whose entries the copy refuses stops the fetch at once, naming it, before any later member
/// is taken from: the follower must be opened again. Opened again from the copy, with that member
/// passed over, the fetch takes the gap from the next.
#[tokio::test]
async fn a_member_whose_entries_the_copy_refuses_is_named_and_passed_over() {
    let (full, copy, other) =
        (scratch_dir("gap-refused-full"), scratch_dir("gap-refused-copy"), scratch_dir("gap-refused-other"));
    ticks(&full, 4).await;
    copy_dir(&full, &copy);
    ticks(&full, 3).await;
    // Another log's entries at the same seqs: their seals chain onto another history than the copy's.
    ticks(&other, 7).await;
    let member = |byte: u8, endpoint: &str| SetMember {
        candidate: AccountId([byte; 20]),
        endpoint: endpoint.into(),
        standby: None,
        admin_key: AccountId([byte; 20]),
        held: false,
    };
    let members = [member(1, "forged:1"), member(2, "giving:1")];
    let asking = Members(BTreeMap::from([
        ("forged:1".into(), Some(with_offsets(&other))),
        ("giving:1".into(), Some(with_offsets(&full))),
    ]));
    let deadline = Instant::now() + BUDGET;
    let (taken, refused) = from_members(&asking, &members, &mut open(&copy), deadline).await;
    assert_eq!((taken, refused), (0, Some(AccountId([1; 20]))), "the fetch went on past a refused member");
    let mut reopened = open(&copy);
    assert_eq!(from_members(&asking, &members[1..], &mut reopened, deadline).await.1, None);
    assert_eq!(reopened.replica().last_seq(), open(&full).replica().last_seq(), "the next member gave the gap");
}

/// A member that gives one entry an answer still gives the whole gap, one page a round: each round asks
/// from the copy's last entry, so a page that ended among the entries after a command is not asked for
/// again.
#[tokio::test]
async fn a_gap_given_one_entry_a_page_is_taken_page_by_page() {
    let (full, copy) = (scratch_dir("gap-paged-full"), scratch_dir("gap-paged-copy"));
    ticks(&full, 4).await;
    copy_dir(&full, &copy);
    ticks(&full, 3).await;
    let mut follower = open(&copy);
    let head = follower.head().expect("the copy holds entries");
    let entries = with_offsets(&full);
    assert!(
        entries.iter().any(|(at, entry)| *at > head && !matches!(entry, LogEntry::Command(_))),
        "the gap holds an entry past a command"
    );
    let member = SetMember {
        candidate: AccountId([1; 20]),
        endpoint: "paging:1".into(),
        standby: None,
        admin_key: AccountId([1; 20]),
        held: false,
    };
    let (taken, refused) = from_members(&Paging(entries), &[member], &mut follower, Instant::now() + BUDGET).await;
    assert!(taken > 1 && refused.is_none(), "taken {taken}, refused {refused:?}");
    drop(follower);
    assert_eq!(log_entries(&copy), log_entries(&full), "the copy took the whole gap");
}

// ── Helpers ──

/// Runs the test sequencer on `dir` for `count` ticks and stops it.
async fn ticks(dir: &std::path::Path, count: usize) {
    let node = TestSequencer::start(dir, 1024);
    for _ in 0..count {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
}

/// The log in `dir`, each entry at its offset: a command starts its seq, and each entry after it takes
/// the next index.
fn with_offsets(dir: &std::path::Path) -> Vec<(Offset, LogEntry)> {
    let mut at = Offset::start_of(0);
    log_entries(dir)
        .into_iter()
        .map(|entry| {
            at = match &entry {
                LogEntry::Command(command) => Offset::start_of(command.sequenced.seq),
                _ => at.next(),
            };
            (at, entry)
        })
        .collect()
}

/// The copy in `dir`, as a follower of the test sequencer's key opens it.
fn open(dir: &std::path::Path) -> Follower {
    let signer = AccountId(EnclaveKey::test_key().address());
    Follower::open(dir, SEGMENT_COMMANDS, DOMAIN, signer, [signer].into(), None, |_| {}).unwrap()
}

/// Copies the log directory `from` into `to`.
fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}
