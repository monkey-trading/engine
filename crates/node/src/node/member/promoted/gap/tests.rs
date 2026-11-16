//! The gap fetch's bound: every member is asked at once each round, a silent one costs one ask's wait
//! per round, and the fetch stops at its deadline.

use std::collections::BTreeMap;

use engine_core::command::Command;
use engine_core::types::AccountId;

use super::*;
use crate::log::{CheckpointDomain, LogEntry};
use crate::tee::EnclaveKey;
use crate::test_support::{SEGMENT_COMMANDS, TestSequencer, copy_dir, log_entries, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// Members by endpoint: the log one holds, each entry at its offset, of which it gives every entry past
/// the offset it is asked after, or `None` for one that never answers.
struct Members(BTreeMap<String, Option<Vec<(Offset, LogEntry)>>>);

impl Asking for Members {
    async fn ask(&self, endpoint: &str, request: &Request) -> anyhow::Result<Reply> {
        let Request::Entries { after } = request else { anyhow::bail!("only entries") };
        match self.0.get(endpoint).cloned().flatten() {
            Some(entries) => {
                Ok(Reply::Entries { entries: entries.into_iter().filter(|(at, _)| Some(*at) > *after).collect() })
            }
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
        Ok(Reply::Entries { entries: self.0.iter().filter(|(at, _)| Some(*at) > *after).take(1).cloned().collect() })
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

/// A member's link over `copy`: its ring, else its durable log.
struct Link(crate::liveness::link::Copy);

impl Asking for Link {
    async fn ask(&self, _: &str, request: &Request) -> anyhow::Result<Reply> {
        let Request::Entries { after } = request else { anyhow::bail!("only entries") };
        Ok(Reply::Entries { entries: self.0.entries(*after).await })
    }
}

/// A member that keeps none of its copy in its ring gives the gap from its durable log, to a copy that
/// trails it and to an empty one, which asks from the log's first entry.
#[tokio::test]
async fn a_member_gives_an_empty_copy_and_a_trailing_one_the_gap_from_its_log() {
    let (full, trailing, empty) =
        (scratch_dir("gap-log-full"), scratch_dir("gap-log-trail"), scratch_dir("gap-log-empty"));
    ticks(&full, 4).await;
    copy_dir(&full, &trailing);
    ticks(&full, 5).await;
    let member = crate::liveness::link::Copy::default();
    member.logged(full.clone(), crate::test_support::published_manifest(&full));
    let link = Link(member);
    let set_member = SetMember {
        candidate: AccountId([1; 20]),
        endpoint: "link:1".into(),
        standby: None,
        admin_key: AccountId([1; 20]),
        held: false,
    };
    let deadline = Instant::now() + BUDGET;
    for (name, dir) in [("trailing", &trailing), ("empty", &empty)] {
        let mut follower = open(dir);
        assert_eq!(follower.head().is_none(), name == "empty");
        let (taken, refused) = from_members(&link, std::slice::from_ref(&set_member), &mut follower, deadline).await;
        assert!(taken > 0 && refused.is_none(), "{name}: taken {taken}, refused {refused:?}");
        drop(follower);
        assert_eq!(log_entries(dir), log_entries(&full), "{name}: the copy took the whole gap");
    }
}

/// The archive's lines for a segment: all of them for an empty copy, and for one holding commands through
/// `tip` the entries at its seq and later, but not the command at `tip` or any before it.
#[tokio::test]
async fn an_archived_segment_gives_an_empty_copy_every_line_and_a_trailing_one_those_past_its_tip() {
    let dir = scratch_dir("gap-past-tip");
    ticks(&dir, 6).await;
    let entries = log_entries(&dir);
    let bytes: Vec<u8> = entries.iter().flat_map(|e| [serde_json::to_vec(e).unwrap(), vec![b'\n']].concat()).collect();
    let mut all = Vec::new();
    past_tip(&bytes, 0, None, &mut all).unwrap();
    assert_eq!(all, bytes, "an empty copy takes the whole segment");
    let mut after = Vec::new();
    past_tip(&bytes, 0, Some(2), &mut after).unwrap();
    let mut seq = 0;
    let expected: Vec<LogEntry> = entries
        .iter()
        .filter(|entry| {
            if let LogEntry::Command(command) = entry {
                seq = command.sequenced.seq;
            }
            if matches!(entry, LogEntry::Command(_)) { seq > 2 } else { seq >= 2 }
        })
        .cloned()
        .collect();
    let taken: Vec<LogEntry> =
        after.split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(|l| serde_json::from_slice(l).unwrap()).collect();
    assert_eq!(taken, expected, "a trailing copy takes the entries past its tip's command, in order");
    assert!(!taken.is_empty() && taken.len() < entries.len());
}

/// A gap of many pages is taken through every one: the fetch goes on while a round extends the copy, not
/// for a fixed number of rounds.
#[tokio::test]
async fn a_gap_of_more_than_sixty_four_pages_is_taken_whole() {
    let (full, empty) = (scratch_dir("gap-long-full"), scratch_dir("gap-long-empty"));
    ticks(&full, 80).await;
    let mut follower = open(&empty);
    let member = SetMember {
        candidate: AccountId([1; 20]),
        endpoint: "paging:1".into(),
        standby: None,
        admin_key: AccountId([1; 20]),
        held: false,
    };
    let entries = with_offsets(&full);
    assert!(entries.len() > 70, "the log is longer than the old round bound");
    let (taken, refused) =
        from_members(&Paging(entries.clone()), &[member], &mut follower, Instant::now() + BUDGET).await;
    assert!(refused.is_none());
    assert_eq!(taken as usize, entries.len(), "every page was taken");
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
