//! Daily snapshots on a log of launch flow across a UTC midnight, published by the node that writes the log,
//! and the log's replay in ranges from them, which `scripts/dev-daily-snapshots.sh` runs: the ranges replay
//! to what the whole log replays to, a snapshot whose resting-order index lost an order replays the same,
//! and one whose state or checkpoint the log's signed root does not vouch for is refused.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engine_core::types::AccountId;

use crate::clock::Clock;
use crate::common::launch_book::{NamedCounts, build};
use crate::daily_snapshots;
use crate::log::SealChecks;
use crate::sequencer::recovery_scale::flow::{Flow, Node};
use crate::sequencer::recovery_scale::orders::Traders;
use crate::sequencer::recovery_scale::{Base, mix};
use crate::sequencer::replay::ranges::{Trust, replay_ranges_from};
use crate::sequencer::replay::{Audited, replay_path_from};
use crate::tee::EnclaveKey;
use crate::test_support::scratch_dir;

/// A UTC day, and how long before midnight the flow starts and how long it runs.
const DAY_MS: i64 = 86_400_000;
const BEFORE_MIDNIGHT: Duration = Duration::from_secs(600);
const FLOW: Duration = Duration::from_secs(1_200);

#[test]
fn dev_scenario_daily_snapshots() {
    let counts = NamedCounts { adders: 1, openers: 8, closeouts: 0 };
    let mut book = build(&mix(200, 0, 1_200), counts);
    let engine = std::mem::take(&mut book.h.engine);
    let base = Base::new(&engine).expect("the book encodes");
    let traders = Traders::new(std::mem::take(&mut book.named.openers), &engine);
    drop((book, engine));
    let run = scratch_dir("daily-snapshots");
    let (log, snapshots) = (run.join("log"), run.join("snapshots"));
    let key = Arc::new(EnclaveKey::generate());
    let signer = AccountId(key.address());

    // The node publishes a snapshot at its first checkpoint, and at its first after midnight.
    let mut node = Node::publishing(base.engine(), &log, key, Some(&snapshots));
    let to_midnight = DAY_MS - node.clock.now_ms().rem_euclid(DAY_MS);
    node.clock.advance(Duration::from_millis(u64::try_from(to_midnight).unwrap()) - BEFORE_MIDNIGHT);
    let mut flow = Flow::new(traders, BTreeMap::from([(1, 11.0)]));
    flow.run(&mut node, FLOW, 1.0, |_, _, _| {});
    node.checkpoint_now();
    // Dropping the node joins the writer, which publishes what it holds first.
    drop(node);
    let listed = daily_snapshots::read_manifest(&snapshots).expect("the manifest");
    assert_eq!(listed.len(), 2, "{listed:?}");
    let days: Vec<i64> = listed.iter().map(|listing| daily_snapshots::day(listing.ts)).collect();
    assert_eq!(days[1], days[0] + 1, "the second snapshot is the next day's");
    assert!(listed[1].ts.rem_euclid(DAY_MS) < 60_000, "the second is at the first checkpoint past midnight");

    // The ranges replay to what the whole log replays to.
    let whole = Audited::from(replay_path_from(base.engine(), &log, signer, SealChecks::RunEnds).expect("whole"));
    let in_ranges = ranges(&base, &log, &snapshots, signer).expect("the log replays in ranges");
    assert_eq!(in_ranges.ranges, 3);
    agree(&whole, &in_ranges);

    // A snapshot whose index of resting orders lost an order still replays the same: the order's leaf
    // holds it, and the index is rebuilt from the orders.
    let later = snapshots.join(&listed[1].file);
    let written = std::fs::read(&later).unwrap();
    edit(&later, &written, |snapshot| {
        let markets = snapshot["engine"]["markets"].as_object_mut().unwrap();
        let resting = markets.values_mut().find_map(|market| {
            let resting = market["resting"].as_object_mut()?;
            (!resting.is_empty()).then_some(resting)
        });
        let resting = resting.expect("an order rests at the later snapshot");
        let order = resting.keys().next().unwrap().clone();
        resting.remove(&order);
    });
    agree(&whole, &ranges(&base, &log, &snapshots, signer).expect("a lost resting order is rebuilt"));

    // A balance the root does not vouch for, and a checkpoint the log does not hold there, are refused.
    let refusals: [(&str, Alter); 2] = [
        ("state root mismatch at seq", |snapshot| {
            let accounts = snapshot["engine"]["accounts"].as_object_mut().unwrap();
            let account = accounts.values_mut().next().unwrap();
            account["balance"] = serde_json::json!("1");
        }),
        ("the log holds another entry at", |snapshot| {
            snapshot["checkpoint"]["root"] = serde_json::json!(format!("0x{}", hex::encode([3; 32])));
        }),
    ];
    for (refused, alter) in refusals {
        edit(&later, &written, alter);
        let error = format!("{:#}", ranges(&base, &log, &snapshots, signer).err().expect("refused"));
        assert!(error.contains(refused), "{error}");
    }
}

// ── Helpers ──

/// A change to a daily snapshot's JSON.
type Alter = fn(&mut serde_json::Value);

/// The log in `log`, written from `base`, replayed in ranges from the daily snapshots in `snapshots`, two
/// at once, under `signer`.
fn ranges(base: &Base, log: &Path, snapshots: &Path, signer: AccountId) -> anyhow::Result<Audited> {
    let trust = Trust::Pinned { domain: None, signer: Some(signer) };
    replay_ranges_from(&|| base.engine(), log, snapshots, trust, NonZeroUsize::new(2).unwrap())
}

/// Requires the whole log's replay and the replay in ranges to agree on everything an audit reports.
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
    let root = |audited: &Audited| serde_json::to_vec(&audited.engine).expect("the engine encodes");
    assert_eq!(root(whole), root(in_ranges), "the ranges end in the whole log's state");
}

/// Rewrites the daily snapshot at `path` as `written`, its bytes as published, with `alter` applied.
fn edit(path: &Path, written: &[u8], alter: impl FnOnce(&mut serde_json::Value)) {
    let mut snapshot: serde_json::Value = serde_json::from_slice(written).unwrap();
    alter(&mut snapshot);
    std::fs::write(path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
}
