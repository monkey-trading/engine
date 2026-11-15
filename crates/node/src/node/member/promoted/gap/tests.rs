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

/// Members by endpoint: the entries one gives past the offset it is asked after, or `None` for one that
/// never answers.
struct Members(BTreeMap<String, Option<(Offset, Vec<LogEntry>)>>);

impl Asking for Members {
    async fn ask(&self, endpoint: &str, request: &Request) -> anyhow::Result<Reply> {
        let Request::Entries { after } = request else { anyhow::bail!("only entries") };
        match self.0.get(endpoint).cloned().flatten() {
            Some((from, entries)) if from == *after => {
                Ok(Reply::Entries { entries: entries.into_iter().map(|entry| (*after, entry)).collect() })
            }
            Some(_) => Ok(Reply::Entries { entries: Vec::new() }),
            None => std::future::pending().await,
        }
    }
}

#[tokio::test]
async fn the_gap_fetch_asks_every_member_at_once_and_stops_at_its_deadline() {
    let (full, copy) = (scratch_dir("gap-full"), scratch_dir("gap-copy"));
    ticks(&full, 4).await;
    copy_dir(&full, &copy);
    ticks(&full, 3).await;
    let mut follower = open(&copy);
    let tip = follower.replica().last_seq().unwrap();
    // What a member answers: every entry after the command at the copy's tip, as `Entries` gives it.
    let mut past = log_entries(&full);
    let at = past.iter().position(|entry| matches!(entry, LogEntry::Command(c) if c.sequenced.seq == tip)).unwrap();
    let past = past.split_off(at + 1);
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
        ("giving:1".into(), Some((Offset::start_of(tip), past))),
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
    let tip = open(&copy).replica().last_seq().unwrap();
    let past = |dir: &std::path::Path| {
        let mut entries = log_entries(dir);
        let at = entries.iter().position(|entry| matches!(entry, LogEntry::Command(c) if c.sequenced.seq == tip));
        entries.split_off(at.expect("the log holds the copy's tip") + 1)
    };
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
        ("forged:1".into(), Some((Offset::start_of(tip), past(&other)))),
        ("giving:1".into(), Some((Offset::start_of(tip), past(&full)))),
    ]));
    let deadline = Instant::now() + BUDGET;
    let (taken, refused) = from_members(&asking, &members, &mut open(&copy), deadline).await;
    assert_eq!((taken, refused), (0, Some(AccountId([1; 20]))), "the fetch went on past a refused member");
    let mut reopened = open(&copy);
    assert_eq!(from_members(&asking, &members[1..], &mut reopened, deadline).await.1, None);
    assert_eq!(reopened.replica().last_seq(), open(&full).replica().last_seq(), "the next member gave the gap");
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
