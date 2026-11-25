//! A copy of the operator's log that an archive or a host supplied: one that spells a seal or a
//! checkpoint's signer otherwise than the operator wrote it is the operator's history, with its
//! cumulative command prefix, and one that dropped or replaced a seal is refused before a node
//! takes it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::eip712::B256;
use engine_core::price::SourceQuote;
use engine_core::types::Millis;

use super::*;
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::history::marks::fold::MarkFold;
use crate::log::command_seal::{checkpoint_link, command_link};
use crate::log::{CommandEntry, MANIFEST, truncate_to_checkpoint};
use crate::sequencer::genesis_commands;
use crate::standby::replica::Follower;
use crate::standby::tests::{DOMAIN, copy_log, ticks};
use crate::tee::EnclaveKey;
use crate::test_support::{
    SEGMENT_COMMANDS, TestSequencer, dev_config, drop_last_checkpoint, log_entries, rewrite_command,
    rewrite_last_checkpoint_signer, sample_around, scratch_dir, unprefixed, upper_case, upper_case_seal,
};

/// A copy that spells a command's seal in upper case, or without its `0x`, at the log's first command
/// or at its last, builds its replica, replays as a copy, and holds the operator's head: the seals
/// verify, and each command commits to the prefix as its writer wrote it.
#[tokio::test]
async fn a_copy_whose_seals_are_spelt_otherwise_holds_its_writers_head() {
    let operator = scratch_dir("copies-spelt-operator");
    ticks(&operator, 10).await;
    let honest = replica(&operator).unwrap();
    let (last, head) = honest.tip().expect("the operator's log holds commands");
    for (case, seq) in [("first", 0), ("last", last)] {
        for (spelling, respell) in [("upper case", upper_case as fn(&str) -> String), ("no prefix", unprefixed)] {
            let what = format!("the {case} command's seal in {spelling}");
            let copy = scratch_dir(&format!("copies-spelt-{case}-{}", spelling.replace(' ', "-")));
            copy_log(&operator, &copy);
            rewrite_command(&copy, seq, |command| {
                let seal = command.seal.as_mut().expect("the sequencer seals every command");
                seal.signature = respell(&seal.signature);
            });
            let mut member = replica(&copy).unwrap_or_else(|e| panic!("{what}: the replica refused it: {e:#}"));
            assert_eq!(member.pin(last, head), Some(true), "{what}: the copy holds another head than the operator's");
            crate::sequencer::replay::replay_copy(&copy, DOMAIN, [enclave()].into(), Default::default())
                .unwrap_or_else(|e| panic!("{what}: the copy's replay refused it: {e:#}"));
        }
    }
}

/// A copy whose command carries no seal, or a seal by a key the vault never recorded, at the log's
/// first command or between two checkpoints, is refused by the replica and by a copy's replay, though
/// a later checkpoint covers it and the audit replay, which commits to no prefix, still verifies it.
#[tokio::test]
async fn a_copy_that_dropped_or_replaced_a_seal_is_refused() {
    let operator = scratch_dir("copies-unsealed-operator");
    ticks(&operator, 10).await;
    let other = EnclaveKey::generate();
    let between = SEGMENT_COMMANDS + 1;
    for (case, seq) in [("first", 0), ("between", between)] {
        let previous = link_before(&operator, seq);
        let dropped = |command: &mut CommandEntry| command.seal = None;
        let replaced = |command: &mut CommandEntry| {
            command.seal = Some(other.seal_command(DOMAIN, command_link(previous, command).unwrap()));
        };
        for (how, change) in [("dropped", &dropped as &dyn Fn(&mut CommandEntry)), ("replaced", &replaced)] {
            let what = format!("the {case} command's seal {how}");
            let copy = scratch_dir(&format!("copies-unsealed-{case}-{how}"));
            copy_log(&operator, &copy);
            rewrite_command(&copy, seq, change);
            crate::sequencer::replay_path_with_attested(&copy, Some(DOMAIN), [enclave()].into(), Default::default())
                .unwrap_or_else(|e| panic!("{what}: a later checkpoint no longer covers it: {e:#}"));
            for (taker, refused) in [
                ("the replica", replica(&copy).err()),
                (
                    "a copy's replay",
                    crate::sequencer::replay::replay_copy(&copy, DOMAIN, [enclave()].into(), Default::default()).err(),
                ),
            ] {
                let refused = refused.unwrap_or_else(|| panic!("{what}: {taker} took the copy"));
                assert!(
                    format!("{refused:#}").contains(&format!("seq {seq} carries no seal by an accepted signer")),
                    "{what}: {taker}: {refused:#}"
                );
            }
        }
    }
}

/// A replica cuts a tail no checkpoint covers that holds a seal by a key it does not accept, a reorged
/// promotion's or one an archive forged, or a command with no seal, back to the copy's last checkpoint,
/// which an accepted signer's seals reach, for the stream to refill: its link and its last command are
/// then its writer's. It pages Critical `copy_tail_unaccepted` naming the tail's first seq and its key,
/// if it has one, and keeps the tail aside.
#[tokio::test]
async fn a_replica_drops_an_unaccepted_tail_back_to_its_last_checkpoint() {
    let operator = scratch_dir("copies-tail-operator");
    ticks(&operator, 10).await;
    // The copy's last checkpoints dropped, as an archive whose segments end between checkpoints lists.
    let uncut = scratch_dir("copies-tail-uncut");
    copy_log(&operator, &uncut);
    while uncovered(&uncut).len() < 2 {
        drop_last_checkpoint(&uncut);
    }
    let tail = uncovered(&uncut);
    let foreign = EnclaveKey::generate();
    for (case, key, sealed) in [
        ("first of the tail", Some(&foreign), &tail[..1]),
        ("the whole tail", Some(&foreign), &tail[..]),
        ("no seal on the first of the tail", None, &tail[..1]),
    ] {
        let copy = scratch_dir(&format!("copies-tail-{}", case.replace(' ', "-")));
        copy_log(&uncut, &copy);
        match key {
            Some(key) => seal_with(&copy, key, sealed),
            None => rewrite_command(&copy, tail[0], |command| command.seal = None),
        }
        let signature = sealed_by(&copy, *tail.last().unwrap());
        let (_guard, mut alerts) = crate::test_support::capture_alerts();
        let mut follower = replica(&copy).unwrap_or_else(|e| panic!("{case}: the replica refused the copy: {e:#}"));
        let kept = tail[0] - 1;
        assert_eq!(follower.replica().last_seq(), Some(kept), "{case}: the tail was not dropped");
        assert_eq!(follower.replays(), 2, "{case}: the open counts other than its two replays");
        assert_eq!(follower.replica().tip_sealer(), Some(enclave()), "{case}: the copy ends on a foreign seal");
        let honest =
            replica(&operator).unwrap().replica().link_at(kept).expect("the operator's replica holds the link");
        assert_eq!(follower.pin(kept, honest.link), Some(true), "{case}: another link");
        assert!(
            matches!(log_entries(&copy).last(), Some(LogEntry::Checkpoint(checkpoint)) if checkpoint.seq == kept),
            "{case}: the copy's files still hold the tail"
        );
        let alert = alerts.try_recv().unwrap_or_else(|_| panic!("{case}: no page"));
        assert_eq!(
            (alert["alert"].as_str(), alert["level"].as_str(), alert["seq"].as_str()),
            ("copy_tail_unaccepted", "ERROR", tail[0].to_string().as_str()),
            "{case}: {alert:?}"
        );
        let named = key.map_or_else(|| "none".to_owned(), |key| AccountId(key.address()).to_string());
        assert_eq!(alert["key"], named, "{case}: the page names another key");
        let mut aside = copy.as_os_str().to_owned();
        aside.push(format!(".unaccepted-{}", tail[0]));
        let held = std::fs::read_dir(&aside)
            .unwrap_or_else(|e| panic!("{case}: no tail kept aside: {e}"))
            .map(|file| std::fs::read_to_string(file.unwrap().path()).unwrap())
            .collect::<String>();
        assert!(held.contains(&signature), "{case}: the tail kept aside lacks its last command");
    }
}

/// A replica that cuts an unaccepted tail back folds none of its marks: its fold is the one a replica of
/// the copy cut back to that checkpoint folds, which the stream then extends, as recovery's would be.
#[tokio::test]
async fn a_replica_cut_back_from_an_unaccepted_tail_folds_none_of_its_marks() {
    let operator = scratch_dir("copies-marks-operator");
    let clock = Arc::new(TestClock::at(1_792_489_800_000));
    let node = TestSequencer::start_at(&operator, 1024, clock.clone(), genesis_commands(&dev_config().genesis));
    // A rising mark, a minute apart, so each command of the tail moves a bucket and the newest mark.
    for step in 0..9 {
        node.submit(prices(clock.now_ms(), 67_000 + 100 * step)).await;
        clock.advance(Duration::from_secs(61));
        node.tx.send(crate::sequencer::Request::TimeSample(sample_around(clock.as_ref()))).await.unwrap();
    }
    node.stop().await;
    let copy = scratch_dir("copies-marks-copy");
    copy_log(&operator, &copy);
    while uncovered(&copy).len() < 3 {
        drop_last_checkpoint(&copy);
    }
    let checkpoint = last_checkpoint(&copy);
    let cut = scratch_dir("copies-marks-cut");
    copy_log(&copy, &cut);
    truncate_to_checkpoint(&cut, checkpoint.seq, &checkpoint.root).unwrap();
    // Each command after a dropped checkpoint was sealed over that checkpoint: the whole tail is resealed.
    seal_with(&copy, &EnclaveKey::generate(), &uncovered(&copy));
    let (ours, theirs) = (folded(&copy), folded(&cut));
    assert_eq!(ours, theirs, "the replica folded marks of the tail it dropped");
    assert_ne!(folded(&operator), theirs, "the tail moved no mark");
}

/// A crash part way through a replica's cut leaves its marker, which a cut that fails keeps too, and the
/// next open finishes the cut before it replays anything: after the truncation listed the copy's end
/// state, where the copy does not open by itself, and after it removed the later segments but before it
/// trimmed its checkpoint's, where the copy opens with part of the tail. Neither pages again, nor
/// replaces the evidence the first open kept.
#[tokio::test]
async fn a_cut_a_crash_interrupted_is_finished_at_the_next_open() {
    let operator = scratch_dir("copies-interrupted-operator");
    ticks(&operator, 10).await;
    let base = scratch_dir("copies-interrupted-base");
    copy_log(&operator, &base);
    // A tail past two segments, from a checkpoint inside its segment, so the cut trims that segment.
    while uncovered(&base).len() <= 2 * SEGMENT_COMMANDS as usize
        || (last_checkpoint(&base).seq + 1).is_multiple_of(SEGMENT_COMMANDS)
    {
        drop_last_checkpoint(&base);
    }
    let tail = uncovered(&base);
    seal_with(&base, &EnclaveKey::generate(), &tail);
    let checkpoint = last_checkpoint(&base);
    let to = Takeover { seq: checkpoint.seq, root: checkpoint.root.clone(), signer: enclave() };
    // A cut that fails keeps its marker: here one to a checkpoint the copy does not hold.
    let failed = scratch_dir("copies-interrupted-failed");
    copy_log(&base, &failed);
    let unheld = Takeover { seq: u64::MAX, ..to.clone() };
    cut::cut_marked(&failed, &unheld).expect_err("a cut to a checkpoint the copy does not hold");
    let marker: Takeover = serde_json::from_slice(&std::fs::read(failed.join("takeover.json")).unwrap()).unwrap();
    assert_eq!(marker, unheld, "the failed cut left no marker");
    // The copy the cut leaves when it completes.
    let cut = scratch_dir("copies-interrupted-cut");
    copy_log(&base, &cut);
    truncate_to_checkpoint(&cut, checkpoint.seq, &checkpoint.root).unwrap();
    for (case, later_removed) in [("after the manifest", false), ("after the later segments", true)] {
        let copy = scratch_dir(&format!("copies-interrupted-{}", case.replace(' ', "-")));
        copy_log(&base, &copy);
        std::fs::copy(cut.join(MANIFEST), copy.join(MANIFEST)).unwrap();
        if later_removed {
            for file in std::fs::read_dir(&copy).unwrap().map(Result::unwrap) {
                if !cut.join(file.file_name()).exists() {
                    std::fs::remove_file(file.path()).unwrap();
                }
            }
        }
        std::fs::write(copy.join("takeover.json"), serde_json::to_vec(&to).unwrap()).unwrap();
        let opens = SegmentedLog::open(&copy, |_| Ok(())).is_ok();
        assert_eq!(opens, later_removed, "{case}: whether the interrupted cut opens by itself");
        if later_removed {
            assert!(log_entries(&copy).len() > log_entries(&cut).len(), "{case}: the copy holds none of the tail");
        }
        let mut evidence = copy.as_os_str().to_owned();
        evidence.push(format!(".unaccepted-{}", tail[0]));
        std::fs::create_dir_all(&evidence).unwrap();
        std::fs::write(std::path::Path::new(&evidence).join("kept"), "the first open's evidence").unwrap();
        // A marker a crash cut short is only ever its staging file, which no open reads.
        std::fs::write(copy.join("takeover.json.staging"), "{\"seq\":").unwrap();
        let (_guard, mut alerts) = crate::test_support::capture_alerts();
        let follower = replica(&copy).unwrap_or_else(|e| panic!("{case}: the next open did not finish the cut: {e:#}"));
        assert_eq!(follower.replica().last_seq(), Some(checkpoint.seq), "{case}: the cut ended elsewhere");
        assert!(!copy.join("takeover.json").exists(), "{case}: the finished cut left its marker");
        assert!(alerts.try_recv().is_err(), "{case}: the finished cut paged again");
        assert!(
            std::path::Path::new(&evidence).join("kept").exists(),
            "{case}: the first open's evidence was replaced"
        );
    }
}

/// The host of an enclave spells the seal of the log's last command, past every posted head,
/// otherwise and restarts it: recovery under the enclave's own key takes the same commands and state,
/// and the head the poster then signs at that seq names the link it named before.
#[tokio::test]
async fn a_restart_from_a_respelt_tail_signs_the_head_link_it_signed_before() {
    let dir = scratch_dir("copies-restart");
    ticks(&dir, 10).await;
    let recovered = |dir: &std::path::Path| {
        let mut log =
            crate::sequencer::Sequencer::read_log_for_recovery(dir, Some(DOMAIN), Some(enclave()), None, |_| {})
                .expect("the log replays under the enclave's own key");
        log.replayed.origin.require_recovery(DOMAIN, enclave()).expect("every command has authenticated coverage");
        (log.last_seq(), log.head_link(), log.replayed.engine.state_root())
    };
    let (seq, before, root) = recovered(&dir);
    rewrite_command(&dir, seq.unwrap(), |command| {
        let seal = command.seal.as_mut().expect("the sequencer seals every command");
        seal.signature = upper_case(&seal.signature);
    });
    let (seq_after, after, root_after) = recovered(&dir);
    assert_eq!((seq_after, root_after), (seq, root), "the restart recovers other commands or state");
    assert_eq!(after, before, "the restarted node's head at seq {seq:?} names another link for the same history");
}

/// At a whole-log handover the successor confirms only a log it then starts from: a copy whose
/// archive spelt a seal otherwise starts against the predecessor's posted head, and one whose archive
/// dropped a seal is refused by the replay before the confirmation that ends the predecessor.
#[tokio::test]
async fn a_handover_confirms_only_a_log_the_successor_starts_from() {
    let predecessor = scratch_dir("copies-handover-predecessor");
    ticks(&predecessor, 10).await;
    let (seq, link) = replica(&predecessor).unwrap().tip().expect("the predecessor's log holds commands");
    let posted = crate::log_heads::Head { seq, link };
    for (case, change) in [
        ("an exact copy", None),
        ("a seal in upper case", Some(&upper_case_seal as &dyn Fn(&mut CommandEntry))),
        ("a dropped seal", Some(&|command: &mut CommandEntry| command.seal = None)),
    ] {
        let successor = scratch_dir(&format!("copies-handover-{}", case.replace(' ', "-")));
        copy_log(&predecessor, &successor);
        if let Some(change) = change {
            rewrite_command(&successor, 0, change);
        }
        match (case, confirmed_then_started(&successor, posted)) {
            ("a dropped seal", Err(refused)) => {
                assert!(refused.contains("carries no seal by an accepted signer"), "{case}: {refused}");
            }
            ("a dropped seal", Ok(_)) => panic!("{case}: the successor confirmed a copy without its writer's seals"),
            (_, Ok(None)) => {}
            (_, Ok(Some(refused))) => panic!("{case}: the successor confirmed, then refused to start: {refused}"),
            (_, Err(refused)) => panic!("{case}: the successor refused the copy: {refused}"),
        }
    }
}

/// A takeover replays its copy as a copy before it truncates it: one whose command dropped its seal, or
/// holds another key's, which a checkpoint covers, is refused and left as it was.
#[tokio::test]
async fn a_takeover_refuses_a_copy_that_dropped_or_replaced_a_seal() {
    let operator = scratch_dir("copies-takeover-operator");
    ticks(&operator, 6).await;
    let other = EnclaveKey::generate();
    for how in ["dropped", "replaced"] {
        let dir = scratch_dir(&format!("copies-takeover-{how}"));
        copy_log(&operator, &dir);
        let previous = link_before(&dir, 0);
        rewrite_command(&dir, 0, |command| {
            command.seal = match how {
                "dropped" => None,
                _ => Some(other.seal_command(DOMAIN, command_link(previous, command).unwrap())),
            }
        });
        let before = log_entries(&dir);
        let last = before
            .iter()
            .rev()
            .find_map(|entry| match entry {
                LogEntry::Checkpoint(checkpoint) => Some(checkpoint.clone()),
                _ => None,
            })
            .expect("the log holds a checkpoint");
        let takeover = Takeover { seq: last.seq, root: last.root, signer: enclave() };
        let (archive, origins) = (archive::archive_client(false).unwrap(), ["http://127.0.0.1:9".to_owned()]);
        let copied = Chain::new(DOMAIN, enclave());
        let refused = finalize(
            &archive,
            (DOMAIN, [enclave()].into(), Default::default()),
            &origins,
            (&dir, None),
            &takeover,
            copied,
        )
        .await
        .expect_err("the takeover truncated a copy without its writer's seals");
        assert!(format!("{refused:#}").contains("seq 0 carries no seal by an accepted signer"), "{how}: {refused:#}");
        assert_eq!(log_entries(&dir), before, "{how}: the refused copy changed");
    }
}

/// A takeover finds its anchor checkpoint in a copy that spells the anchor's signer in mixed case.
#[tokio::test]
async fn the_takeover_finds_an_anchor_whose_signer_is_spelt_otherwise() {
    let dir = scratch_dir("copies-takeover-anchor");
    ticks(&dir, 6).await;
    let anchor = rewrite_last_checkpoint_signer(&dir);
    let takeover = Takeover { seq: anchor.seq, root: anchor.root.clone(), signer: enclave() };
    let resume = resume_checkpoint(&dir, &takeover).unwrap_or_else(|e| panic!("the takeover lost its anchor: {e:#}"));
    assert_eq!(resume, (anchor.seq, anchor.root.to_lowercase()));
}

// ── Helpers ──

/// The test sequencer's enclave address, which seals its commands and signs its checkpoints.
fn enclave() -> AccountId {
    AccountId(EnclaveKey::test_key().address())
}

/// A member's replica of the copy in `dir`, as `Member` builds it over a backfilled copy.
fn replica(dir: &std::path::Path) -> anyhow::Result<Follower> {
    Follower::open(dir, SEGMENT_COMMANDS, DOMAIN, enclave(), [enclave()].into(), None, |_| {})
}

/// The seqs of the commands after the last checkpoint of the log in `dir`.
pub(crate) fn uncovered(dir: &std::path::Path) -> Vec<u64> {
    let entries = log_entries(dir);
    let last = entries.iter().rposition(|entry| matches!(entry, LogEntry::Checkpoint(_))).unwrap_or(0);
    entries[last..]
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(command) => Some(command.sequenced.seq),
            _ => None,
        })
        .collect()
}

/// The last checkpoint of the log in `dir`.
fn last_checkpoint(dir: &std::path::Path) -> crate::log::Checkpoint {
    let entries = log_entries(dir);
    let last = entries.into_iter().rev().find_map(|entry| match entry {
        LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
        _ => None,
    });
    last.expect("the log holds a checkpoint")
}

/// Seals the commands at `seqs` of the log in `dir` with `key`, in order, each over its writer's link.
pub(crate) fn seal_with(dir: &std::path::Path, key: &EnclaveKey, seqs: &[u64]) {
    for seq in seqs {
        let previous = link_before(dir, *seq);
        rewrite_command(dir, *seq, |command| {
            command.seal = Some(key.seal_command(DOMAIN, command_link(previous, command).unwrap()));
        });
    }
}

/// The seal signature of the command at `seq` of the log in `dir`.
fn sealed_by(dir: &std::path::Path, seq: u64) -> String {
    let command = log_entries(dir).into_iter().find_map(|entry| match entry {
        LogEntry::Command(command) if command.sequenced.seq == seq => command.seal,
        _ => None,
    });
    command.expect("a sealed command at the seq").signature
}

/// What a member's replica of the copy in `dir` folds of its marks, with the seq it ends at.
fn folded(dir: &std::path::Path) -> (Option<u64>, Option<(Vec<crate::history::marks::fold::Folded>, Millis)>) {
    let marks = Rc::new(RefCell::new(MarkFold::default()));
    let kept = (None, Some(marks.clone()), None, None);
    let follower = Follower::open_pinned(
        dir,
        SEGMENT_COMMANDS,
        Default::default(),
        DOMAIN,
        enclave(),
        [enclave()].into(),
        kept,
        (None, &[]),
        None,
        |_| Ok(()),
    )
    .unwrap_or_else(|e| panic!("the replica refused the copy: {e:#}"));
    (follower.replica().last_seq(), marks.take().into_buckets())
}

/// Three fresh venue quotes for market 1 at `ts` around `mid`, which the engine's median takes.
fn prices(ts: i64, mid: i64) -> Command {
    let quotes = ["mock-a", "mock-b", "mock-c"].map(|source| SourceQuote {
        source: source.into(),
        mid: Fixed::from_int(mid),
        ts,
        contract: None,
    });
    Command::Prices { market: 1, quotes: quotes.to_vec() }
}

/// The seal chain's link before the command at `seq` in the log in `dir`.
fn link_before(dir: &std::path::Path, seq: u64) -> B256 {
    let mut link = B256::default();
    for entry in log_entries(dir) {
        match entry {
            LogEntry::Command(command) if command.sequenced.seq == seq => return link,
            LogEntry::Command(command) => link = command_link(link, &command).unwrap(),
            LogEntry::Checkpoint(checkpoint) => link = checkpoint_link(&checkpoint).unwrap(),
            LogEntry::Withdrawal(_) => {}
        }
    }
    panic!("no command at seq {seq}")
}

/// What a whole-log handover's successor does with `dir`: the replay `take_segments` runs before the
/// confirmation that ends the predecessor (its refusal, as `Err`), then the start `node::run` makes
/// against `posted`, the head the predecessor posted (its refusal, as `Ok(Some)`).
fn confirmed_then_started(dir: &std::path::Path, posted: crate::log_heads::Head) -> Result<Option<String>, String> {
    crate::sequencer::replay::replay_copy(dir, DOMAIN, [enclave()].into(), Default::default())
        .map_err(|refused| format!("{refused:#}"))?;
    let log = crate::sequencer::Sequencer::read_log_with_authorities(
        dir,
        Some(DOMAIN),
        Some(enclave()),
        None,
        Default::default(),
        |_| {},
        None,
        None,
        Some(posted.seq),
        Default::default(),
    )
    .expect("recovery under the handed-over key replays the log");
    let rpc_dir = dir.with_extension("heads-rpc");
    std::fs::create_dir_all(&rpc_dir).unwrap();
    let pending = crate::log_heads::setup::Pending::for_test(Some(posted), &rpc_dir);
    Ok(crate::log_heads::setup::startup(Some(pending), &log).err().map(|refusal| format!("{refusal:#}")))
}
