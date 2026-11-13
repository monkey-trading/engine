//! A log the node wrote before protocol versions existed still replays, through `engine replay`'s
//! log format and checkpoints, to the roots it was signed with. Each version's engine roots are
//! pinned by `crates/core/tests/reference/`. A log `engine replay` cannot check fails it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use engine_node::log::CheckpointDomain;
use engine_node::{replay_path, replay_path_in_domain};

/// Written by the node before protocol versions existed (commit 452ec1b), on `config/dev.toml`
/// with two mock-priced markets and `checkpoint_every = 40`: genesis, prices and ticks, a deposit,
/// a market and a limit order, a cancel, a withdrawal, and three checkpoints.
const V1_DEV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/v1-dev.jsonl");

#[test]
fn a_log_written_before_protocol_versions_replays_to_its_roots() {
    let mut replayed =
        replay_path_in_domain(Path::new(V1_DEV), Some(CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) }))
            .unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (63, 3));
    assert_eq!(replayed.engine.protocol_version(), ProtocolVersion::V2);
    assert!(replayed.activations.is_empty());
    // The root that build printed for this log.
    let root = format!("0x{}", hex::encode(replayed.engine.state_root()));
    assert_eq!(root, "0xeb7239777dc18152f4e4a75cd902800dbb918b1e6da35da6171d391d649df535");
}

/// A log segment altered to carry a stamp past the end of 9999, the latest the engine accepts, is
/// refused with an error naming the entry's seq, not a panic: recovery and `engine replay` treat
/// every logged field as untrusted. The altered entry is seq 6, not 0, so the named seq is the one
/// refused. One stamp, `i64::MAX`, is one chrono cannot represent at all; the other,
/// `253_402_300_800_000`, is the first millisecond past the range's end.
#[test]
fn replay_refuses_a_stamp_outside_the_range_the_engine_accepts() {
    let fixture = fs::read_to_string(V1_DEV).unwrap();
    let lines: Vec<serde_json::Value> = fixture.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let dir = scratch("stamp-out-of-range");
    for (i, ts) in [i64::MAX, 253_402_300_800_000].into_iter().enumerate() {
        let mut entries = lines[..=6].to_vec();
        entries[6]["ts"] = serde_json::json!(ts);
        let stamped: String = entries.iter().map(|e| format!("{}\n", serde_json::to_string(e).unwrap())).collect();
        fs::write(dir.join(format!("stamp-{i}.jsonl")), &stamped).unwrap();
        fs::create_dir_all(dir.join(format!("stamp-dir-{i}"))).unwrap();
        fs::write(dir.join(format!("stamp-dir-{i}/00000000000000000000.jsonl")), &stamped).unwrap();
        for case in [format!("stamp-{i}.jsonl"), format!("stamp-dir-{i}")] {
            let err = replay_path(&dir.join(&case)).err().unwrap_or_else(|| panic!("{case} verified"));
            let msg = format!("{err:#}");
            assert!(msg.contains("applying seq 6"), "{case}: {msg}");
            assert!(msg.contains("is past the end of 9999"), "{case}: {msg}");
        }
    }
}

/// `engine replay` verifies only a log it finds and checks against a signed root: a missing path,
/// an empty log, and a log of commands with no checkpoint all fail, as a file and as a directory.
#[test]
fn replay_fails_on_a_log_it_cannot_verify() {
    let dir = scratch("unverifiable");
    let tick = concat!(r#"{"entry":"command","seq":0,"ts":0,"cmd":{"type":"tick"}}"#, "\n");
    fs::create_dir_all(dir.join("empty-dir")).unwrap();
    fs::write(dir.join("empty.jsonl"), "").unwrap();
    fs::write(dir.join("no-checkpoint.jsonl"), tick).unwrap();
    fs::create_dir_all(dir.join("no-checkpoint-dir")).unwrap();
    fs::write(dir.join("no-checkpoint-dir/00000000000000000000.jsonl"), tick).unwrap();
    // (case, path, the cause named)
    let cases = [
        ("a missing file", "typo.jsonl", "no readable log at"),
        ("a missing directory", "typo", "no readable log at"),
        ("an empty file", "empty.jsonl", "holds no command"),
        ("an empty directory", "empty-dir", "holds no command"),
        ("a file with no checkpoint", "no-checkpoint.jsonl", "holds no checkpoint"),
        ("a directory with no checkpoint", "no-checkpoint-dir", "holds no checkpoint"),
    ];
    for (case, path, cause) in cases {
        let err = replay_path(&dir.join(path)).err().unwrap_or_else(|| panic!("{case} verified"));
        assert!(format!("{err:#}").contains(cause), "{case}: {err:#}");
    }
}

/// `engine replay` on a path that does not exist exits non-zero without a verdict; on a log it
/// verifies, it exits zero and says so.
#[test]
fn engine_replay_exits_non_zero_on_a_missing_log() {
    let missing = scratch("missing").join("typo.jsonl");
    let run = |log: &Path| Command::new(env!("CARGO_BIN_EXE_engine")).arg("replay").arg("--log").arg(log).output();
    let refused = run(&missing).unwrap();
    assert!(!refused.status.success(), "{refused:?}");
    assert!(!String::from_utf8_lossy(&refused.stdout).contains("verified"), "{refused:?}");
    assert!(String::from_utf8_lossy(&refused.stderr).contains("no readable log at"), "{refused:?}");

    let verified = Command::new(env!("CARGO_BIN_EXE_engine"))
        .args([
            "replay",
            "--log",
            V1_DEV,
            "--chain-id",
            "31337",
            "--vault",
            "0x0000000000000000000000000000000000000000",
        ])
        .output()
        .unwrap();
    assert!(verified.status.success(), "{verified:?}");
    assert!(
        String::from_utf8_lossy(&verified.stdout).starts_with("verified: 63 commands, 3 checkpoints"),
        "{verified:?}"
    );
}

/// An empty directory for one test, named after it, removed when the test ends. A failed test's stays.
fn scratch(name: &str) -> Scratch {
    let dir = std::env::temp_dir().join(format!("engine-node-replay-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    Scratch(dir)
}

struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
