//! The posted-head check's binding to the replayed bytes: the captured cumulative command-prefix
//! digest, its continuity across a shutdown checkpoint (checkpoints do not reset it), the
//! host-tamper proof, the poster's evidence and stall alarm seeded from the replay, and startup's
//! check of the newest head this key's epochs hold.
use alloy::primitives::B256;
use engine_core::Engine;
use engine_core::command::{Command, Sequenced};
use engine_core::types::AccountId;

use super::super::{Head, check_rollback};
use crate::log::command_seal::checkpoint_link;
use crate::log::{Checkpoint, CheckpointDomain, CommandEntry, LogEntry, SegmentedLog};
use crate::sequencer::Sequencer;
use crate::tee::EnclaveKey;

const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([7; 20]) };

/// One tick, applied to the oracle so its events and the checkpoint root match the replay.
fn command(oracle: &mut Engine, seq: u64) -> LogEntry {
    let sequenced = Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick };
    let events = oracle.apply(&sequenced).unwrap();
    LogEntry::Command(CommandEntry { sequenced, events: Some(events), seal: None })
}

/// Writes `[command 0, checkpoint 0, command 1]` — a command, the checkpoint a clean shutdown
/// writes at its seq, and one more command — to a fresh log directory, returning the cumulative
/// command-prefix digests after the first and second commands.
fn log_with_shutdown_checkpoint(label: &str) -> (std::path::PathBuf, B256, B256, Checkpoint) {
    let key = EnclaveKey::test_key();
    let mut oracle = Engine::new();
    let mut command_prefix = [0u8; 32];
    let mut prefixes = Vec::new();
    let first = command(&mut oracle, 0);
    command_prefix = crate::log::extend_command_prefix(
        command_prefix,
        crate::log::command_digest(match &first {
            LogEntry::Command(entry) => entry,
            _ => unreachable!(),
        })
        .unwrap(),
    );
    prefixes.push(B256::from(command_prefix));
    let root = oracle.state_root();
    let checkpoint = Checkpoint {
        parent: None,
        seq: 0,
        root: format!("0x{}", hex::encode(root)),
        signature: key.sign_checkpoint(DOMAIN.chain_id, &DOMAIN.vault.0, 0, &root),
        signer: key.address_hex(),
        domain: Some(DOMAIN),
        deferred_withdrawals: false,
    };
    let second = command(&mut oracle, 1);
    command_prefix = crate::log::extend_command_prefix(
        command_prefix,
        crate::log::command_digest(match &second {
            LogEntry::Command(entry) => entry,
            _ => unreachable!(),
        })
        .unwrap(),
    );
    prefixes.push(B256::from(command_prefix));
    let dir = crate::test_support::scratch_dir(label);
    std::fs::create_dir_all(&dir).unwrap();
    let entries = vec![first, LogEntry::Checkpoint(checkpoint.clone()), second];
    let log = SegmentedLog::open(&dir, |_| Ok(())).unwrap();
    let mut writer = log.into_writer(10, tokio::sync::watch::channel(Default::default()).0).unwrap();
    for entry in &entries {
        writer.append(entry).unwrap();
    }
    writer.flush().unwrap();
    (dir, prefixes[0], prefixes[1], checkpoint)
}

/// Reads `dir` as recovery does, capturing the link at `target` from the replayed bytes.
fn recover(dir: &std::path::Path, target: u64) -> crate::sequencer::ReplayedLog {
    recover_from(dir, None, Some(target))
}

/// Reads `dir` as recovery does from the vault-finalized `parent`, capturing at `target`.
fn recover_from(
    dir: &std::path::Path,
    parent: Option<crate::log::CheckpointParent>,
    target: Option<u64>,
) -> crate::sequencer::ReplayedLog {
    let key = EnclaveKey::test_key();
    Sequencer::read_log_with_authorities(
        dir,
        Some(DOMAIN),
        Some(AccountId(key.address())),
        parent,
        Default::default(),
        |_| {},
        None,
        None,
        target,
    )
    .unwrap()
}

#[test]
fn the_replay_captures_the_cumulative_prefix_never_the_checkpoint_link() {
    let (dir, first_prefix, last_prefix, checkpoint) = log_with_shutdown_checkpoint("log-head-capture");
    let log = recover(&dir, 0);
    assert_eq!(log.replayed.link_at, Some(first_prefix), "the capture is the cumulative prefix");
    assert_ne!(
        log.replayed.link_at,
        Some(checkpoint_link(&checkpoint).unwrap()),
        "a checkpoint arm at the same seq must not move the captured head prefix"
    );
    // Checkpoints do not reset the cumulative prefix: the head after command 1 still binds
    // command 0, across the checkpoint between them.
    assert_eq!(log.head_link(), last_prefix, "the head is the cumulative prefix after the last command");
    assert_ne!(log.head_link(), checkpoint_link(&checkpoint).unwrap());
}

#[test]
fn a_shutdown_checkpoint_at_the_posted_seq_keeps_the_posted_head_accepted() {
    let (dir, first_prefix, _, checkpoint) = log_with_shutdown_checkpoint("log-head-shutdown-checkpoint");
    let log = recover(&dir, 0);
    let posted = Head { seq: 0, link: first_prefix };
    // What the poster released at seq 0 was the cumulative prefix; the shutdown checkpoint the
    // log then gained at seq 0 does not reset it: a healthy log restarts.
    check_rollback(posted, log.last_seq().unwrap_or(0), |seq| {
        assert_eq!(seq, 0);
        Ok(log.replayed.link_at)
    })
    .unwrap();
    let error = check_rollback(Head { seq: 0, link: checkpoint_link(&checkpoint).unwrap() }, 1, |seq| {
        assert_eq!(seq, 0);
        Ok(log.replayed.link_at)
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("differs"), "{error:#}");
}

#[test]
fn the_startup_check_binds_to_the_replayed_bytes_not_the_hosts_files() {
    let (dir, _, last_prefix, _) = log_with_shutdown_checkpoint("log-head-binding");
    let log = recover(&dir, 1);
    assert_eq!(log.replayed.link_at, Some(last_prefix));
    // The host cannot split recovery from the check: it swaps the on-disk log after the
    // replay, and the check still answers from the bytes recovery authenticated.
    std::fs::remove_dir_all(&dir).unwrap();
    let posted = Head { seq: 1, link: last_prefix };
    check_rollback(posted, 1, |seq| {
        assert_eq!(seq, 1);
        Ok(log.replayed.link_at)
    })
    .unwrap();
    let error = check_rollback(Head { seq: 1, link: B256::from([4; 32]) }, 1, |seq| {
        assert_eq!(seq, 1);
        Ok(log.replayed.link_at)
    })
    .unwrap_err();
    assert!(format!("{error:#}").contains("differs"), "{error:#}");
}

#[test]
fn startup_seeds_the_poster_with_each_replayed_commands_prefix() {
    // After a restart, the poster's evidence that this log holds an on-chain head is what the
    // replay authenticated: each command's cumulative prefix, across the checkpoint between them,
    // with nothing recorded at a seq the log does not reach.
    let (dir, first_prefix, last_prefix, _) = log_with_shutdown_checkpoint("log-head-ring-seed");
    let log = recover(&dir, 0);
    let rpc_dir = crate::test_support::scratch_dir("log-head-ring-seed-rpc");
    std::fs::create_dir_all(&rpc_dir).unwrap();
    let setup =
        crate::log_heads::setup::startup(Some(crate::log_heads::setup::Pending::for_test(None, &rpc_dir)), &log)
            .unwrap()
            .expect("[log_heads] is configured");
    let ring = &setup.poster.prefix_ring;
    assert_eq!(ring.prefix_at(0), Some(first_prefix));
    assert_eq!(ring.prefix_at(1), Some(last_prefix));
    assert_eq!(ring.prefix_at(2), None);
}

#[test]
fn startup_compares_a_posted_head_above_any_snapshot_with_the_replayed_link() {
    // A full replay has no snapshot base: the gate compares the posted head with the link the
    // replay captured at its seq, accepting this log's own and refusing a foreign one.
    let (dir, _, last_prefix, _) = log_with_shutdown_checkpoint("log-head-startup-gate");
    let rpc_dir = crate::test_support::scratch_dir("log-head-startup-gate-rpc");
    std::fs::create_dir_all(&rpc_dir).unwrap();
    let log = recover(&dir, 1);
    let start = |posted| {
        crate::log_heads::setup::startup(Some(crate::log_heads::setup::Pending::for_test(Some(posted), &rpc_dir)), &log)
    };
    assert!(start(Head { seq: 1, link: last_prefix }).is_ok(), "this log's own head is accepted");
    let error = start(Head { seq: 1, link: B256::from([4; 32]) }).err().expect("a foreign link refuses the start");
    assert!(format!("{error:#}").contains("differs"), "{error:#}");
}

#[test]
fn startup_starts_the_stall_alarm_from_the_checked_heads_sequenced_time() {
    // The checked head is seq 1, which the fixture stamps at 1 ms: the poster starts with that
    // head as its on-chain seq and that stamp as the time the alarm counts from.
    let (dir, _, last_prefix, _) = log_with_shutdown_checkpoint("log-head-stall-seed");
    let rpc_dir = crate::test_support::scratch_dir("log-head-stall-seed-rpc");
    std::fs::create_dir_all(&rpc_dir).unwrap();
    let log = recover(&dir, 1);
    let posted = Head { seq: 1, link: last_prefix };
    let setup = crate::log_heads::setup::startup(
        Some(crate::log_heads::setup::Pending::for_test(Some(posted), &rpc_dir)),
        &log,
    )
    .unwrap()
    .expect("[log_heads] is configured");
    assert_eq!(setup.poster.anchored, Some((1, Some(1))));
}

#[test]
fn startup_checks_the_newest_head_of_this_keys_epochs_and_parents_only_on_the_current_ones() {
    // `setSigner` reinstalled this key: the current epoch three is empty and an earlier epoch holds
    // this log's head at seq 1. Startup checks that head, and the first post names no parent.
    let (dir, _, last_prefix, _) = log_with_shutdown_checkpoint("log-head-reinstalled");
    let rpc_dir = crate::test_support::scratch_dir("log-head-reinstalled-rpc");
    std::fs::create_dir_all(&rpc_dir).unwrap();
    let log = recover(&dir, 1);
    let start = |current, newest: Option<Head>| {
        let heads = crate::log_heads::rpc::StartupHeads { epoch: 3, current, newest: newest.map(|head| (2, head)) };
        crate::log_heads::setup::startup(Some(crate::log_heads::setup::Pending::for_test_heads(heads, &rpc_dir)), &log)
    };
    let earlier = Head { seq: 1, link: last_prefix };
    let setup = start(None, Some(earlier)).unwrap().expect("[log_heads] is configured");
    assert_eq!(setup.poster.confirmed, None, "an earlier epoch's head is no parent");
    assert_eq!(setup.poster.anchored, Some((1, Some(1))), "the stall alarm counts from the earlier head");
    // A log cut below the earlier epoch's head is refused.
    let error =
        start(None, Some(Head { seq: 5, link: B256::from([5; 32]) })).err().expect("the cut log refuses the start");
    assert!(format!("{error:#}").contains("lacks the posted head's seqs 2..=5"), "{error:#}");
    // The current epoch's own head parents the first post.
    let setup = start(Some(earlier), Some(earlier)).unwrap().expect("[log_heads] is configured");
    assert_eq!(setup.poster.confirmed, Some((3, earlier)));
}

#[test]
fn startup_anchors_the_stall_alarm_on_the_finalized_checkpoint_with_no_head_posted() {
    // The vault finalized the checkpoint at seq 0, whose command is stamped at 0 ms. With no head
    // of this key posted, the alarm counts from that stamp; with this log's head at seq 1, stamped
    // at 1 ms, from the later of the two.
    let (dir, _, last_prefix, checkpoint) = log_with_shutdown_checkpoint("log-head-finalized-anchor");
    let rpc_dir = crate::test_support::scratch_dir("log-head-finalized-anchor-rpc");
    std::fs::create_dir_all(&rpc_dir).unwrap();
    let parent = crate::log::CheckpointParent { has_checkpoint: true, seq: 0, root: checkpoint.root.parse().unwrap() };
    let start = |log: &crate::sequencer::ReplayedLog, posted| {
        let pending = crate::log_heads::setup::Pending::for_test(posted, &rpc_dir);
        crate::log_heads::setup::startup(Some(pending), log)
            .unwrap()
            .expect("[log_heads] is configured")
            .poster
            .anchored
    };
    let unanchored = recover_from(&dir, None, None);
    assert_eq!(start(&unanchored, None), Some((0, None)), "nothing anchors a log the vault has not finalized");
    let finalized = recover_from(&dir, Some(parent), None);
    assert_eq!(start(&finalized, None), Some((0, Some(0))), "the finalized checkpoint anchors it");
    let posted = recover_from(&dir, Some(parent), Some(1));
    assert_eq!(start(&posted, Some(Head { seq: 1, link: last_prefix })), Some((1, Some(1))));
}
