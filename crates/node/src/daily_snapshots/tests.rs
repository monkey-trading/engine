use super::*;
use crate::daily_snapshots::writer::Job;
use crate::snapshot::sealing::Encoding;
use crate::test_support::scratch_dir;

/// A snapshot's name is its checkpoint's offset, as recovery caches are named, and only such a name gives
/// one.
#[test]
fn a_snapshot_is_named_for_its_checkpoints_offset() {
    let offset = Offset { seq: 1_234_567, index: 3 };
    let name = crate::snapshot::name(offset);
    assert_eq!(offset_named(&name), Some(offset));
    for other in ["manifest.jsonl", "00000000000001234567-0000000003.json.tmp", "1234567-3.json", "x.json"] {
        assert_eq!(offset_named(other), None, "{other}");
    }
}

/// A snapshot is due at a checkpoint on a later UTC day than the last one taken, and at the first
/// checkpoint of all; one handed to the writer takes its day at once.
#[test]
fn a_snapshot_is_due_on_each_new_utc_day() {
    let dir = scratch_dir("daily-due");
    let mut writer = Writer::start(&dir, None).unwrap();
    let noon = 20_000 * DAY_MS + DAY_MS / 2;
    assert!(writer.due(noon), "the first checkpoint takes one");
    writer.take(job(Offset { seq: 5, index: 1 }, noon));
    assert!(!writer.due(noon + DAY_MS / 2 - 1), "the last millisecond of the day");
    assert!(writer.due(noon + DAY_MS / 2), "midnight");
    drop(writer);
    let listed = read_manifest(&dir).unwrap();
    assert_eq!((listed.len(), listed[0].ts), (1, noon));
    let restarted = Writer::start(&dir, Some(5)).unwrap();
    assert!(!restarted.due(noon + 1) && restarted.due(noon + DAY_MS / 2), "a restart reads the last day");
}

/// A published snapshot is listed with its file's length and SHA-256, and loads as the engine it holds; a
/// restart removes a staging file and an unlisted snapshot, and drops a listing past the log's last
/// checkpoint with its file.
#[test]
fn a_restart_keeps_only_listed_snapshots_of_checkpoints_the_log_holds() {
    let dir = scratch_dir("daily-restart");
    let listing = publish_now(&dir, job(Offset { seq: 5, index: 1 }, 0)).unwrap();
    let bytes = std::fs::read(dir.join(&listing.file)).unwrap();
    assert_eq!(listing.bytes, bytes.len() as u64);
    assert_eq!(listing.sha256, hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&bytes)));
    let later = publish_now(&dir, job(Offset { seq: 9, index: 1 }, DAY_MS)).unwrap();
    std::fs::write(dir.join("00000000000000000007-0000000001.json"), b"{}").unwrap();
    std::fs::write(dir.join(format!("{}.tmp", later.file)), b"{").unwrap();
    drop(Writer::start(&dir, Some(8)).unwrap());
    let mut held: Vec<String> =
        std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
    held.sort();
    assert_eq!(held, [listing.file.clone(), MANIFEST.to_owned()]);
    assert_eq!(read_manifest(&dir).unwrap(), [listing]);
}

/// `load` refuses a snapshot of another format, one whose name, offset, checkpoint and engine disagree,
/// and one of a state before `V27` or custody finality.
#[test]
fn load_refuses_a_snapshot_it_cannot_start_a_range_from() {
    let dir = scratch_dir("daily-load");
    let listing = publish_now(&dir, job(Offset { seq: 5, index: 1 }, 0)).unwrap();
    let path = dir.join(&listing.file);
    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let error = format!("{:#}", load(&path).err().expect("an engine at V0 is refused"));
    assert!(error.contains("predates V27"), "{error}");
    let cases: [(&str, Alter); 3] = [
        ("of format 2", |snapshot| snapshot["schema"] = 2.into()),
        ("names offset", |snapshot| snapshot["offset"] = "5:2".into()),
        ("stand at seqs", |snapshot| snapshot["checkpoint"]["seq"] = 6.into()),
    ];
    for (said, alter) in cases {
        let mut snapshot = written.clone();
        alter(&mut snapshot);
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let error = format!("{:#}", load(&path).err().expect("refused"));
        assert!(error.contains(said), "{said}: {error}");
    }
}

// ── Helpers ──

/// A change to a daily snapshot's JSON.
type Alter = fn(&mut serde_json::Value);

/// A snapshot of an engine that applied ticks up to `offset`'s seq, the last stamped `ts`, at a checkpoint.
fn job(offset: Offset, ts: Millis) -> Job {
    let mut engine = Engine::default();
    for seq in 0..=offset.seq {
        let tick = engine_core::command::Sequenced { seq, ts, cmd: engine_core::command::Command::Tick };
        engine.apply(&tick).unwrap();
    }
    let checkpoint = Checkpoint {
        parent: None,
        seq: offset.seq,
        root: format!("0x{}", hex::encode([offset.seq as u8; 32])),
        signature: String::new(),
        signer: format!("0x{}", hex::encode([1; 20])),
        domain: None,
        deferred_withdrawals: false,
    };
    Job { offset, checkpoint, ts, engine: Encoding::Encoded(serde_json::to_string(&engine).unwrap()) }
}

/// A staged file whose rename fails is removed, so the next write can stage it again.
#[test]
fn a_staged_file_that_cannot_be_published_is_removed() {
    let dir = scratch_dir("daily-snapshots-staged");
    std::fs::create_dir_all(dir.join(MANIFEST).join("held")).unwrap();
    let staged = dir.join(format!("{MANIFEST}.tmp"));
    std::fs::write(&staged, b"{}\n").unwrap();
    assert!(writer::publish_staged(&staged, &dir.join(MANIFEST)).is_err(), "a rename onto a full directory");
    assert!(!staged.exists(), "the staged file is removed");
}
