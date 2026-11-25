//! A replay in ranges from daily snapshots that carry the replay state: each range is an ordinary replay that
//! goes on from the snapshot's state, and the pass requires the state a snapshot carries to be the state the
//! log folds to there. Snapshots without it split the log as before.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use engine_core::types::AccountId;

use crate::daily_snapshots::{self, Signing};
use crate::log::{CheckpointDomain, SealChecks};
use crate::sequencer::replay::Replay;
use crate::sequencer::replay::ranges::fixture::{snapshot_log_under, without_carried};
use crate::sequencer::replay::ranges::{Trust, replay_ranges};
use crate::sequencer::replay::{Audited, replay_path_with_trust};
use crate::tee::EnclaveKey;
use crate::test_support::scratch_dir;

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

/// A funded log and the daily snapshots published at the checkpoints counted from 1 in `at`.
struct Logged {
    log: PathBuf,
    snapshots: PathBuf,
}

impl Logged {
    fn new(name: &str, at: &[usize]) -> Self {
        let run = scratch_dir(name);
        let (log, snapshots) = (run.join("log"), run.join("snapshots"));
        crate::sequencer::fixture_tests::write_funded_log(&log, 4);
        let signing = Signing::for_tests();
        let mut seen = 0;
        let trusting = Replay::with_trust(Some(DOMAIN), Some(AccountId(EnclaveKey::test_key().address())));
        let published =
            snapshot_log_under(trusting, engine_core::Engine::default(), &log, (&snapshots, &signing), |_, _| {
                seen += 1;
                at.contains(&seen)
            })
            .expect("the snapshots publish");
        assert_eq!(published, at.len());
        Self { log, snapshots }
    }

    fn files(&self) -> Vec<PathBuf> {
        daily_snapshots::list(&self.snapshots).unwrap().into_iter().map(|(_, file)| file).collect()
    }

    fn ranges(&self) -> anyhow::Result<Audited> {
        let signer = AccountId(EnclaveKey::test_key().address());
        let trust = Trust::Pinned { domain: Some(DOMAIN), signer: Some(signer) };
        replay_ranges(&self.log, &self.snapshots, trust, NonZeroUsize::new(2).unwrap())
    }

    fn whole(&self) -> Audited {
        let signer = AccountId(EnclaveKey::test_key().address());
        replay_path_with_trust(&self.log, Some(DOMAIN), Some(signer), SealChecks::RunEnds).expect("whole").into()
    }
}

/// Rewrites the snapshot at `file` with `alter` applied to its JSON.
fn edit(file: &Path, alter: impl FnOnce(&mut serde_json::Value)) {
    let mut snapshot: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    alter(&mut snapshot);
    std::fs::write(file, serde_json::to_vec(&snapshot).unwrap()).unwrap();
}

/// Requires the replay in ranges to agree with the whole replay on everything an audit reports.
fn agree(whole: &Audited, in_ranges: &Audited) {
    assert_eq!(
        (whole.commands, whole.checkpoints, whole.pending, &whole.activations, &whole.checkpoint_signers),
        (
            in_ranges.commands,
            in_ranges.checkpoints,
            in_ranges.pending,
            &in_ranges.activations,
            &in_ranges.checkpoint_signers
        )
    );
    let state = |audited: &Audited| serde_json::to_vec(&audited.engine).unwrap();
    assert_eq!(state(whole), state(in_ranges));
}

/// A log with snapshots that carry the replay state replays in ranges to what it replays whole.
#[tokio::test]
async fn snapshots_that_carry_the_replay_state_split_the_log_as_one_history() {
    let logged = Logged::new("ranges-carried", &[2, 4]);
    let in_ranges = logged.ranges().expect("the log replays in ranges");
    assert_eq!((in_ranges.ranges, in_ranges.skipped.len()), (3, 0));
    agree(&logged.whole(), &in_ranges);
}

/// A snapshot whose carried state is not the one the log folds to fails the replay naming the snapshot's
/// file, though its engine is the log's and its state passes the checks a replay makes of it.
#[tokio::test]
async fn a_snapshot_whose_carried_state_differs_from_the_logs_fold_is_named() {
    let logged = Logged::new("ranges-carried-differs", &[2, 4]);
    let file = logged.files().remove(1);
    edit(&file, |snapshot| {
        let commands = snapshot["carried"]["commands"].as_u64().unwrap();
        snapshot["carried"]["commands"] = (commands + 1).into();
    });
    let error = format!("{:#}", logged.ranges().err().expect("refused"));
    assert!(error.contains("range 3 of 3 starts from another checkpoint or state"), "{error}");
    assert!(error.contains(&format!("daily snapshot {} does not match the log", file.display())), "{error}");
}

/// Snapshots of the format before the replay state still split the log in ranges, each range applying the
/// log as one range of it.
#[tokio::test]
async fn snapshots_without_the_replay_state_still_replay_in_ranges() {
    let logged = Logged::new("ranges-uncarried", &[2, 4]);
    logged.files().iter().for_each(|file| without_carried(file));
    let in_ranges = logged.ranges().expect("the log replays in ranges");
    assert_eq!(in_ranges.ranges, 3);
    agree(&logged.whole(), &in_ranges);
}

/// A directory with snapshots of both formats splits the log at those that carry the replay state only.
#[tokio::test]
async fn a_directory_of_both_formats_splits_at_the_snapshots_that_carry_the_replay_state() {
    let logged = Logged::new("ranges-mixed", &[2, 4]);
    without_carried(&logged.files()[0]);
    let in_ranges = logged.ranges().expect("the log replays in ranges");
    assert_eq!((in_ranges.ranges, in_ranges.skipped.len()), (2, 0), "the older snapshot splits nothing");
    agree(&logged.whole(), &in_ranges);
}
