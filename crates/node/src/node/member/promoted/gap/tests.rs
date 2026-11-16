//! The gap fetch's bound: every member is asked at once each round, a silent one costs the round no more
//! than one ask's wait and none while another gives, and the fetch stops at its deadline.

use std::collections::BTreeMap;

use engine_core::command::Command;
use engine_core::types::AccountId;

use super::*;
use crate::log::{CheckpointDomain, LogEntry};
use crate::standby_key::chain::Promotion;
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
    // Two rounds: the first is taken from the giving member at once, the second finds nothing more and
    // waits one ask's time for the silent members, not one each.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= ASK && elapsed < ASK * 2,
        "silent members cost one wait, only once nothing is given: {elapsed:?}"
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

/// The archive's lines for a segment: all of them for an empty copy, and for one holding entries through
/// `after` only those past it, in order.
#[tokio::test]
async fn an_archived_segment_gives_an_empty_copy_every_line_and_a_trailing_one_those_past_its_last_entry() {
    let dir = scratch_dir("gap-past");
    ticks(&dir, 6).await;
    let entries = with_offsets(&dir);
    let bytes: Vec<u8> =
        entries.iter().flat_map(|(_, e)| [serde_json::to_vec(e).unwrap(), vec![b'\n']].concat()).collect();
    let mut all = Vec::new();
    past(&bytes, 0, None, &mut all).unwrap();
    assert_eq!(all, bytes, "an empty copy takes the whole segment");
    let after =
        entries.iter().find(|(_, e)| !matches!(e, LogEntry::Command(_))).map(|(at, _)| *at).expect("a checkpoint");
    let mut rest = Vec::new();
    past(&bytes, 0, Some(after), &mut rest).unwrap();
    let taken: Vec<LogEntry> =
        rest.split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(|l| serde_json::from_slice(l).unwrap()).collect();
    let expected: Vec<LogEntry> = entries.iter().filter(|(at, _)| *at > after).map(|(_, e)| e.clone()).collect();
    assert_eq!(taken, expected, "a trailing copy takes the entries past its last one, in order");
    assert!(!taken.is_empty() && taken.len() < entries.len());
}

/// The archive's completed segments are taken one after another onto an empty copy and onto a trailing
/// one, though each ends with a checkpoint after its last command: the next segment's first command follows
/// the copy's last entry.
#[tokio::test]
async fn the_archives_segments_are_taken_in_turn_onto_an_empty_copy_and_a_trailing_one() {
    let (full, trailing, empty) =
        (scratch_dir("gap-seg-full"), scratch_dir("gap-seg-trail"), scratch_dir("gap-seg-empty"));
    ticks(&full, 4).await;
    copy_dir(&full, &trailing);
    ticks(&full, 12).await;
    let mut files: Vec<(u64, Vec<u8>)> = std::fs::read_dir(&full)
        .unwrap()
        .filter_map(|file| {
            let file = file.unwrap();
            let first = crate::log::segment_seq(&file.file_name().to_string_lossy())?;
            Some((first, std::fs::read(file.path()).unwrap()))
        })
        .collect();
    files.sort();
    assert!(files.len() > 2, "the log spans several segments");
    let segments: Vec<(u64, u64)> = files
        .iter()
        .enumerate()
        .map(|(i, (first, _))| (*first, files.get(i + 1).map_or(u64::MAX, |(next, _)| next - 1)))
        .collect();
    for (name, dir) in [("trailing", &trailing), ("empty", &empty)] {
        let mut follower = open(dir);
        let taken = take_segments(&mut follower, &segments, async |index| Ok(files[index].1.clone())).await.unwrap();
        assert!(taken > 0, "{name}");
        drop(follower);
        assert_eq!(log_entries(dir), log_entries(&full), "{name}: the copy took every segment");
    }
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

/// Answers busy for its first asks, as a donor does while still reading for questions the fetch dropped,
/// then gives the log.
struct Busy {
    log: Vec<(Offset, LogEntry)>,
    busy_for: std::sync::atomic::AtomicU32,
}

impl Asking for Busy {
    async fn ask(&self, endpoint: &str, request: &Request) -> anyhow::Result<Reply> {
        use std::sync::atomic::Ordering;
        if self.busy_for.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1)).is_ok() {
            return Ok(Reply::Refused { reason: "busy answering entries and vouches: ask again".into() });
        }
        Paging(self.log.clone()).ask(endpoint, request).await
    }
}

fn one_member() -> SetMember {
    SetMember {
        candidate: AccountId([1; 20]),
        endpoint: "busy:1".into(),
        standby: None,
        admin_key: AccountId([1; 20]),
        held: false,
    }
}

/// A donor that answers busy is asked again shortly rather than read as out of entries, and one that stays
/// busy ends the fetch after a bounded wait.
#[tokio::test]
async fn a_busy_donor_is_asked_again_and_a_donor_that_stays_busy_ends_the_fetch() {
    let (full, empty) = (scratch_dir("gap-busy-full"), scratch_dir("gap-busy-empty"));
    ticks(&full, 6).await;
    let log = with_offsets(&full);
    tokio::time::pause();
    let started = Instant::now();
    let busy = Busy { log: log.clone(), busy_for: 3.into() };
    let (taken, refused) = from_members(&busy, &[one_member()], &mut open(&empty), started + BUDGET).await;
    assert!(taken as usize == log.len() && refused.is_none(), "taken {taken}, refused {refused:?}");
    assert!(started.elapsed() >= BUSY_WAIT * 3, "the fetch waited between asks: {:?}", started.elapsed());
    let started = Instant::now();
    let stuck = Busy { log, busy_for: u32::MAX.into() };
    let (taken, _) =
        from_members(&stuck, &[one_member()], &mut open(&scratch_dir("gap-busy-stuck")), started + BUDGET).await;
    assert_eq!(taken, 0);
    assert!(started.elapsed() <= BUSY_WAIT * (BUSY_RETRIES + 1), "{:?}", started.elapsed());
}

/// A gap longer than the replica retains keeps the prefixes the promotion fixed: the head's link is still
/// there once the copy is far past it.
#[tokio::test]
async fn a_long_gap_leaves_the_promotions_fixed_head_in_the_replica() {
    let full = scratch_dir("gap-keep-full");
    ticks(&full, 300).await;
    let log = with_offsets(&full);
    let promotion = Promotion {
        turn: 1,
        carried: 1,
        position: 0,
        signer_epoch: 2,
        generation: 1,
        head_seq: 5,
        head_link: Default::default(),
        checkpoint_seq: 0,
        checkpoint_root: Default::default(),
        at: 0,
        block: 0,
    };
    let member = one_member();
    for kept in [true, false] {
        let mut follower = open(&scratch_dir(&format!("gap-keep-{kept}")));
        if kept {
            keep_fixed(&mut follower, &promotion);
        }
        let asking = Busy { log: log.clone(), busy_for: 0.into() };
        let (taken, _) =
            from_members(&asking, std::slice::from_ref(&member), &mut follower, Instant::now() + BUDGET).await;
        assert!(taken as usize == log.len(), "the whole log was taken");
        assert_eq!(follower.replica().link_at(5).is_some(), kept, "kept {kept}");
    }
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

/// A silent member does not hold up a page another gives: a gap of many pages from one member that answers
/// at once is taken whole, in less than one ask's wait, though a silent member is asked every round.
#[tokio::test]
async fn a_silent_member_does_not_delay_the_pages_another_gives() {
    let (full, empty) = (scratch_dir("gap-silent-full"), scratch_dir("gap-silent-empty"));
    ticks(&full, 40).await;
    let entries = with_offsets(&full);
    let mut follower = open(&empty);
    let member = |byte: u8, endpoint: &str| SetMember {
        candidate: AccountId([byte; 20]),
        endpoint: endpoint.into(),
        standby: None,
        admin_key: AccountId([byte; 20]),
        held: false,
    };
    let members = [member(1, "silent:1"), member(2, "paging:1")];
    struct Mixed(Vec<(Offset, LogEntry)>);
    impl Asking for Mixed {
        async fn ask(&self, endpoint: &str, request: &Request) -> anyhow::Result<Reply> {
            if endpoint.starts_with("silent") {
                return std::future::pending().await;
            }
            Paging(self.0.clone()).ask(endpoint, request).await
        }
    }
    tokio::time::pause();
    let started = Instant::now();
    let deadline = started + BUDGET;
    let (taken, refused) = from_members(&Mixed(entries.clone()), &members, &mut follower, deadline).await;
    assert!(refused.is_none());
    assert_eq!(taken as usize, entries.len(), "every page was taken");
    // One wait, in the last round, which finds nothing more.
    assert!(started.elapsed() < ASK * 2, "{:?}", started.elapsed());
}
