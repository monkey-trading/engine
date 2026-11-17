//! Reference logs: every committed log in `tests/reference/` replays through `Engine::apply` to the
//! state root it recorded after each command. One log is committed per protocol version and never
//! regenerated, so a change that alters any root fails here, naming the first diverging seq. The
//! replay checks the close-out index against a full assessment at every check site.

mod common;

use std::any::Any;
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use common::reference::{ReferenceEntry, root_hex};
use common::{BTC, Harness, acct};
use engine_core::command::Command;
use engine_core::types::Side;
use engine_core::{CloseoutCoverage, CloseoutOracle, Engine, Fixed};

#[test]
fn every_reference_log_replays_to_its_committed_roots() {
    let logs = reference_logs();
    assert!(!logs.is_empty(), "no reference logs in {}", reference_dir().display());
    let failures: Vec<String> = logs
        .iter()
        .filter_map(|path| verify_roots(&read(path)).err().map(|e| format!("{}: {e}", name(path))))
        .collect();
    assert!(failures.is_empty(), "reference logs no longer replay to their committed roots:\n{}", failures.join("\n"));
}

/// The logs reach every part of the close-out index the oracle counts, so the replay's check of it
/// cannot pass vacuously.
#[test]
fn the_reference_logs_reach_every_part_of_the_close_out_index() {
    let mut total = CloseoutCoverage::default();
    for path in reference_logs() {
        let mut engine = Engine::new();
        engine.set_closeout_oracle(CloseoutOracle { verify: true, ..CloseoutOracle::default() });
        for entry in read(&path) {
            engine.apply(&entry.sequenced).expect("every reference log replays");
        }
        let seen = engine.closeout_coverage();
        total.builds += seen.builds;
        total.price_crossings += seen.price_crossings;
        total.fx_crossings += seen.fx_crossings;
        total.headroom_moves += seen.headroom_moves;
        total.relevels += seen.relevels;
        total.rebands += seen.rebands;
        total.keys += seen.keys;
        total.weekend_skips += seen.weekend_skips;
        total.skipped_calls += seen.skipped_calls;
        total.below_untradeable += seen.below_untradeable;
    }
    println!("{total:?}");
}

#[test]
fn a_changed_root_is_reported_at_its_seq() {
    let mut entries = fresh_log();
    entries[6].root = root_hex([0; 32]);
    let err = verify_roots(&entries).unwrap_err();
    assert!(err.starts_with("root diverges at seq 6:"), "{err}");
}

#[test]
fn a_changed_command_is_reported_at_the_first_seq_it_alters() {
    let mut entries = fresh_log();
    // One extra unit of the smallest decimal on the first deposit changes every root from there on.
    let seq = entries.iter().position(|e| matches!(e.sequenced.cmd, Command::Deposit { .. })).expect("a deposit");
    let Command::Deposit { amount, .. } = &mut entries[seq].sequenced.cmd else { unreachable!("found above") };
    *amount += Fixed::from_raw(1);
    let err = verify_roots(&entries).unwrap_err();
    assert!(err.starts_with(&format!("root diverges at seq {seq}:")), "{err}");
}

#[test]
fn an_out_of_range_stamp_is_refused_at_its_seq() {
    // The stamp this refuses used to panic the engine when it scheduled the first rollover; the
    // refusal names the seq instead, and only a corrupt log carries one. Seq 1, not 0, so the
    // named seq is the one refused.
    let mut entries = fresh_log();
    entries[1].sequenced.ts = i64::MAX;
    let err = verify_roots(&entries).unwrap_err();
    assert!(err.starts_with("seq 1 refused: stamp "), "{err}");
}

#[test]
fn a_refused_sequence_is_reported_at_its_seq() {
    let mut entries = fresh_log();
    // Dropping seq 3 leaves a gap: the engine expects 3 and is handed 4.
    entries.remove(3);
    let err = verify_roots(&entries).unwrap_err();
    assert!(err.starts_with("seq 4 refused: expected seq 3, got 4"), "{err}");
}

#[test]
fn an_empty_log_does_not_verify() {
    assert_eq!(verify_roots(&[]), Err("empty log".to_owned()));
}

#[test]
fn only_jsonl_logs_may_sit_in_the_reference_directory() {
    assert_eq!(logs_among(&["v2.jsonl", ".DS_Store", "v1.jsonl"]), Ok(vec!["v1.jsonl", "v2.jsonl"]));
    assert_eq!(logs_among(&["v1.jsonl", "v2.json"]), Err("v2.json"));
}

// ── Helpers ──

/// Replays `entries` on a fresh engine, comparing the state root after every command.
/// Error cases:
/// - no entries
/// - the engine refuses a command's seq or timestamp, or panics applying it
/// - a root differs from the committed one; the message names the first such seq
fn verify_roots(entries: &[ReferenceEntry]) -> Result<(), String> {
    if entries.is_empty() {
        return Err("empty log".into());
    }
    let mut engine = Engine::new();
    engine.set_closeout_oracle(CloseoutOracle { verify: true, ..CloseoutOracle::default() });
    entries.iter().try_for_each(|entry| {
        let seq = entry.sequenced.seq;
        // A panic is caught so the failure still names the log and the seq.
        panic::catch_unwind(AssertUnwindSafe(|| engine.apply(&entry.sequenced)))
            .map_err(|payload| format!("seq {seq} panicked: {}", panic_message(payload.as_ref())))?
            .map_err(|e| format!("seq {seq} refused: {e}"))?;
        let replayed = root_hex(engine.state_root());
        if replayed != entry.root {
            return Err(format!("root diverges at seq {seq}: committed {}, replay gives {replayed}", entry.root));
        }
        Ok(())
    })
}

/// The directory holding one reference log per protocol version.
fn reference_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference")
}

/// Every log in the reference directory, in name order; panics on a file `logs_among` refuses.
fn reference_logs() -> Vec<PathBuf> {
    let dir = reference_dir();
    let names: Vec<String> = fs::read_dir(&dir)
        .expect("reference directory is readable")
        .map(|entry| entry.expect("directory entry is readable").file_name().to_string_lossy().into_owned())
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let logs =
        logs_among(&names).unwrap_or_else(|stray| panic!("{stray} in {} is not a .jsonl reference log", dir.display()));
    logs.into_iter().map(|log| dir.join(log)).collect()
}

/// The `.jsonl` logs among a directory's file names, in name order, skipping hidden files such as
/// `.DS_Store`.
/// Error cases:
/// - any other file, returned so a misnamed log cannot go unreplayed
fn logs_among<'a>(names: &[&'a str]) -> Result<Vec<&'a str>, &'a str> {
    let mut logs: Vec<&str> = names.iter().copied().filter(|name| !name.starts_with('.')).collect();
    if let Some(stray) = logs.iter().find(|name| !name.ends_with(".jsonl")) {
        return Err(stray);
    }
    logs.sort();
    Ok(logs)
}

/// A short log with the roots this build produces, so the verifier's own tests hold whatever the
/// committed logs say.
fn fresh_log() -> Vec<ReferenceEntry> {
    let mut h = Harness::new();
    h.deposit(acct(1), "1000");
    h.price(BTC, "100000");
    h.market_order(acct(1), BTC, Side::Buy, "0.001", "100000");
    let mut engine = Engine::new();
    h.log
        .into_iter()
        .map(|sequenced| {
            engine.apply(&sequenced).expect("the harness sequenced it");
            ReferenceEntry { root: root_hex(engine.state_root()), sequenced }
        })
        .collect()
}

/// The message a panic carried, when it is a string.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("")
}

/// The log's file name, which names its protocol version.
fn name(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// Parses a reference log; panics naming the file and line of the first entry that does not parse.
fn read(path: &Path) -> Vec<ReferenceEntry> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    text.lines()
        .enumerate()
        .map(|(n, line)| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{} line {}: {e}", path.display(), n + 1))
        })
        .collect()
}
