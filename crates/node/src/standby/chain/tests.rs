//! A copy's chain against a real sequencer's log: an operator that carries on extends it, one cut
//! back and restarted is caught at the first entry or heartbeat that shows it, and an entry that
//! does not chain leaves the copy as it was.

use std::path::Path;

use engine_core::command::{Command, Sequenced};

use super::*;
use crate::log::{Checkpoint, CommandEntry, CommandSeal, LogEntry, SegmentedLog};
use crate::sequencer::StreamMsg;
use crate::standby::tests::{DOMAIN, copy_log, cut_back, ticks};
use crate::tee::EnclaveKey;
use crate::test_support::{log_entries, log_messages, scratch_dir};

#[tokio::test]
async fn an_operator_that_carries_on_extends_the_copy() {
    let operator = scratch_dir("chain-carries-on");
    ticks(&operator, 5).await;
    let copy = scratch_dir("chain-carries-on-copy");
    copy_log(&operator, &copy);
    let mut copied = chain_of(&copy);
    // A restart, then more commands, checkpoints and segments than the copy has.
    ticks(&operator, 9).await;

    copied.begin();
    let mut steps = Vec::new();
    for (offset, entry) in streamed(&operator, copied.resume_seq()) {
        let step = copied.check(offset, &entry).unwrap();
        if let Step::Extends(link) = step {
            copied.extend(offset, entry, link);
        }
        steps.push(step);
    }
    assert!(steps.first() == Some(&Step::Repeated), "the stream repeats the copy's last command first");
    assert!(steps.iter().filter(|step| matches!(step, Step::Extends(_))).count() >= 9);
    assert!(copied.confirmed());
    // The links the copy computed from its files are the ones the sequencer sealed.
    let whole = chain_of(&operator);
    assert_eq!((copied.head(), copied.link), (whole.head(), whole.link));
}

#[tokio::test]
async fn an_operator_cut_back_and_restarted_is_caught_at_the_first_divergent_entry() {
    let operator = scratch_dir("chain-cut-back");
    ticks(&operator, 8).await;
    let copy = scratch_dir("chain-cut-back-copy");
    copy_log(&operator, &copy);
    let before = log_entries(&copy);
    let mut copied = chain_of(&copy);
    let head = copied.head().unwrap();
    // The host cuts the log back to an earlier checkpoint, and the restarted operator sequences
    // past the copy's head again, with other commands.
    cut_back(&operator, head.seq);
    ticks(&operator, 10).await;
    assert!(chain_of(&operator).head() > Some(head), "the cut-back log has grown past the copy");

    copied.begin();
    let (offset, entry) = streamed(&operator, copied.resume_seq()).into_iter().next().unwrap();
    let rollback = copied.check(offset, &entry).unwrap_err();
    let last_command = Offset::start_of(head.seq);
    assert_eq!(rollback.copy_head, Some(head));
    assert_eq!(
        rollback.evidence,
        format!(
            "its entry at {last_command} differs from the copy's, so it no longer holds the copy's entries from \
             {last_command} through {head}"
        )
    );
    assert!(!copied.confirmed());
    // The copy keeps what it had, and still replays to its roots.
    assert_eq!(copied.head(), Some(head));
    assert_eq!(log_entries(&copy), before);
    crate::sequencer::replay_path(&copy).unwrap();
}

#[tokio::test]
async fn an_empty_copy_is_confirmed_only_by_an_entry_that_extends_it() {
    let operator = scratch_dir("chain-empty");
    ticks(&operator, 1).await;
    let mut copied = Chain::new(DOMAIN, AccountId(EnclaveKey::test_key().address()));
    copied.begin();
    assert!(!copied.confirmed(), "nothing has arrived yet");
    let refused = copied.check(Offset::start_of(1), &command(1)).unwrap_err();
    assert_eq!(refused.evidence, "its entry at 1:0 does not extend the copy: the copy's next command is seq 0");
    copied.begin();
    let (offset, first) = streamed(&operator, 0).into_iter().next().unwrap();
    let Step::Extends(link) = copied.check(offset, &first).unwrap() else { panic!("a new entry extends the copy") };
    copied.extend(offset, first, link);
    assert!(copied.confirmed());
}

#[test]
fn a_signed_head_behind_the_copy_is_a_rollback() {
    let mut copied = Chain::new(DOMAIN, AccountId([0; 20]));
    assert!(copied.heartbeat(None).is_ok(), "an empty copy is behind nothing");
    copied.record(command(0)).unwrap();
    copied.record(command(1)).unwrap();
    copied.record(checkpoint(1)).unwrap();
    let head = Offset { seq: 1, index: 1 };
    for ahead in [head, Offset { seq: 1, index: 2 }, Offset::start_of(2)] {
        assert!(copied.heartbeat(Some(ahead)).is_ok(), "{ahead}");
    }
    assert_eq!(
        copied.heartbeat(Some(Offset::start_of(1))).unwrap_err().evidence,
        "its signed head is 1:0, so it no longer holds the copy's entries after 1:0 through 1:1"
    );
    assert_eq!(
        copied.heartbeat(None).unwrap_err().evidence,
        "its signed head is empty, so it no longer holds the copy's entries through 1:1"
    );
}

#[test]
fn a_repeat_that_skips_or_changes_a_copied_entry_is_a_rollback() {
    let mut copied = Chain::new(DOMAIN, AccountId([0; 20]));
    for entry in [command(0), command(1), checkpoint(1)] {
        copied.record(entry).unwrap();
    }
    assert_eq!(copied.resume_seq(), 1);
    copied.begin();
    assert_eq!(copied.check(Offset::start_of(1), &command(1)), Ok(Step::Repeated));
    let skipped = copied.check(Offset::start_of(2), &command(2)).unwrap_err();
    assert_eq!(
        skipped.evidence,
        "it sent 2:0 where the copy holds 1:1, so it no longer holds the copy's entries from 1:1 through 1:1"
    );
    copied.begin();
    let mut changed = command(1);
    let LogEntry::Command(entry) = &mut changed else { unreachable!() };
    entry.sequenced.ts += 1;
    let differs = copied.check(Offset::start_of(1), &changed).unwrap_err();
    assert_eq!(
        differs.evidence,
        "its entry at 1:0 differs from the copy's, so it no longer holds the copy's entries from 1:0 through 1:1"
    );
}

#[tokio::test]
async fn an_entry_that_does_not_chain_is_refused_and_leaves_the_copy_as_it_was() {
    let operator = scratch_dir("chain-refused");
    ticks(&operator, 4).await;
    let copy = scratch_dir("chain-refused-copy");
    copy_log(&operator, &copy);
    let mut copied = chain_of(&copy);
    ticks(&operator, 1).await;
    copied.begin();
    let (repeats, fresh): (Vec<_>, Vec<_>) =
        streamed(&operator, copied.resume_seq()).into_iter().partition(|(offset, _)| Some(*offset) <= copied.head());
    for (offset, entry) in repeats {
        assert_eq!(copied.check(offset, &entry), Ok(Step::Repeated));
    }
    assert!(copied.confirmed());
    let (offset, genuine) = fresh.into_iter().next().unwrap();
    let LogEntry::Command(command) = &genuine else { panic!("the operator's next entry is a command") };
    let link = command_link(copied.link, command).unwrap();
    let other_domain = CheckpointDomain { chain_id: 1, ..DOMAIN };
    let resealed = |seal: CommandSeal| {
        let mut entry = command.clone();
        entry.seal = Some(seal);
        LogEntry::Command(entry)
    };
    let mut skipped = command.clone();
    skipped.sequenced.seq += 1;
    skipped.seal = Some(EnclaveKey::test_key().seal_command(DOMAIN, command_link(copied.link, &skipped).unwrap()));
    let mut restamped = command.clone();
    restamped.sequenced.ts += 1;
    let cases = [
        (
            "no seal",
            offset,
            LogEntry::Command(CommandEntry { seal: None, ..command.clone() }),
            "it carries no command seal",
        ),
        (
            "another key",
            offset,
            resealed(EnclaveKey::generate().seal_command(DOMAIN, link)),
            "not by the followed enclave",
        ),
        (
            "another deployment",
            offset,
            resealed(EnclaveKey::test_key().seal_command(other_domain, link)),
            "for this deployment",
        ),
        ("another command", offset, LogEntry::Command(restamped), "its seal does not chain onto the copy's history"),
        (
            "a skipped seq",
            Offset::start_of(offset.seq + 1),
            LogEntry::Command(skipped),
            "the copy's next command is seq",
        ),
        (
            "a command off its offset",
            Offset { seq: offset.seq, index: 1 },
            genuine.clone(),
            "does not follow the copy's head",
        ),
        (
            "a checkpoint for another seq",
            copied.head().unwrap().next(),
            checkpoint(offset.seq + 7),
            "a checkpoint for seq",
        ),
    ];
    let (head, link_before) = (copied.head(), copied.link);
    for (what, at, entry, why) in cases {
        let refused = copied.check(at, &entry).unwrap_err();
        assert!(refused.evidence.contains(why), "{what}: {}", refused.evidence);
        assert_eq!((copied.head(), copied.link), (head, link_before), "{what} changed the copy's chain");
    }
    assert_eq!(copied.check(offset, &genuine), Ok(Step::Extends(link)), "the genuine entry still extends the copy");
}

#[tokio::test]
async fn an_archive_segment_is_taken_only_if_every_entry_extends_the_copy() {
    let operator = scratch_dir("chain-segment");
    ticks(&operator, 4).await;
    let copy = scratch_dir("chain-segment-copy");
    copy_log(&operator, &copy);
    let mut copied = chain_of(&copy);
    ticks(&operator, 3).await;
    let next: Vec<LogEntry> = streamed(&operator, copied.resume_seq())
        .into_iter()
        .filter(|(offset, _)| Some(*offset) > copied.head())
        .map(|(_, entry)| entry)
        .collect();
    let staged = scratch_dir("chain-segment-staged");
    std::fs::create_dir_all(&staged).unwrap();
    let file = staged.join("segment.jsonl");

    let mut broken = next.clone();
    let LogEntry::Command(last) = broken.iter_mut().rev().find(|e| matches!(e, LogEntry::Command(_))).unwrap() else {
        unreachable!()
    };
    last.sequenced.ts += 1;
    write_entries(&file, &broken);
    let (head, link) = (copied.head(), copied.link);
    let refused = copied.extend_with(&file).unwrap_err();
    assert!(rollback_of(&refused).is_some(), "{refused:#}");
    assert_eq!((copied.head(), copied.link), (head, link), "a refused segment adds nothing");

    write_entries(&file, &next);
    copied.extend_with(&file).unwrap();
    let whole = chain_of(&operator);
    assert_eq!((copied.head(), copied.link), (whole.head(), whole.link));
}

// ── Helpers ──

/// The chain of the copy in `dir`, as the standby opens it.
fn chain_of(dir: &Path) -> Chain {
    let mut copied = Chain::new(DOMAIN, AccountId(EnclaveKey::test_key().address()));
    SegmentedLog::open(dir, |entry| copied.record(entry)).unwrap();
    copied
}

/// What the operator's stream sends from the start of `seq`, as the standby decodes it.
fn streamed(dir: &Path, seq: u64) -> Vec<(Offset, LogEntry)> {
    log_messages(dir, Offset::start_of(seq))
        .iter()
        .map(|text| {
            let message: StreamMsg = serde_json::from_str(text).unwrap();
            (message.offset, LogEntry::from(message.body))
        })
        .collect()
}

/// Writes `entries` to `path` as log lines.
fn write_entries(path: &Path, entries: &[LogEntry]) {
    let text: String = entries.iter().map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap())).collect();
    std::fs::write(path, text).unwrap();
}

fn command(seq: u64) -> LogEntry {
    LogEntry::Command(CommandEntry {
        sequenced: Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick },
        events: Some(Vec::new()),
        seal: None,
    })
}

fn checkpoint(seq: u64) -> LogEntry {
    LogEntry::Checkpoint(Checkpoint {
        parent: None,
        seq,
        root: format!("0x{}", hex::encode([7; 32])),
        signature: format!("0x{}", "00".repeat(65)),
        signer: format!("0x{}", "11".repeat(20)),
        domain: None,
        deferred_withdrawals: false,
    })
}
