use super::*;
use crate::daily_snapshots::writer::Job;
use crate::snapshot::sealing::Encoding;
use crate::test_support::scratch_dir;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::AdminCommand;
use engine_core::types::AccountId;

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
    writer.finish();
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

/// A published snapshot carries the SHA-256 of its engine's encoding, which is that engine's state digest, and
/// the writer's signature over its deployment, offset, root and that digest, which recovers to the writer's key
/// there and to no key in another deployment; its manifest line is the format before signatures, which an earlier
/// release reads.
#[test]
fn a_published_snapshot_is_signed_over_its_state() {
    let dir = scratch_dir("daily-signed");
    let job = job(Offset { seq: 5, index: 1 }, 0);
    let (domain, key) = (job.signing.domain, job.signing.key.address());
    let listing = publish_now(&dir, job).unwrap();
    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join(&listing.file)).unwrap()).unwrap();
    assert_eq!(written["schema"], 2);
    let engine: Engine = serde_json::from_value(written["engine"].clone()).unwrap();
    let state = state_digest(&engine).unwrap();
    assert_eq!(written["state"].as_str(), Some(hex::encode(state).as_str()));
    let signature: engine_core::eip712::Signature = written["signature"].as_str().unwrap().parse().unwrap();
    let root: engine_core::eip712::B256 = listing.root.parse().unwrap();
    let offset = Offset { seq: listing.seq, index: listing.index };
    let recovered =
        |domain| engine_core::eip712::recover(&signature::digest(domain, offset, root, state).into(), &signature);
    assert_eq!(recovered(domain), Some(engine_core::types::AccountId(key)));
    let other = crate::log::CheckpointDomain { chain_id: domain.chain_id + 1, ..domain };
    assert_ne!(recovered(other), Some(engine_core::types::AccountId(key)), "another deployment");
    let line: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(dir.join(MANIFEST)).unwrap().trim()).unwrap();
    let fields: Vec<&str> = line.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(fields, ["bytes", "file", "index", "root", "seq", "sha256", "ts"], "the manifest line's fields");
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
        ("of format 3", |snapshot| snapshot["schema"] = 3.into()),
        ("names offset", |snapshot| snapshot["offset"] = "5:2".into()),
        ("stands at seq", |snapshot| snapshot["checkpoint"]["seq"] = 6.into()),
    ];
    for (said, alter) in cases {
        let mut snapshot = written.clone();
        alter(&mut snapshot);
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let error = format!("{:#}", load(&path).err().expect("refused"));
        assert!(error.contains(said), "{said}: {error}");
    }
}

/// A snapshot of format 1, which carries no state digest or signature, loads as the start of a range; one of
/// format 2 loses it by lacking either field.
#[test]
fn load_reads_the_format_before_signatures_and_refuses_the_signed_one_without_them() {
    let dir = scratch_dir("daily-formats");
    let offset = Offset { seq: 5, index: 1 };
    let mut job = job(offset, 0);
    job.engine = Encoding::Encoded(serde_json::to_string(&finality_engine(offset.seq)).unwrap());
    let listing = publish_now(&dir, job).unwrap();
    let path = dir.join(&listing.file);
    let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let write = |snapshot: &serde_json::Value| std::fs::write(&path, serde_json::to_vec(snapshot).unwrap()).unwrap();
    let loaded = load(&path).expect("a signed snapshot of a state with custody finality loads");
    assert_eq!((loaded.schema, loaded.offset), (2, offset));

    let mut unsigned = written.clone();
    unsigned["schema"] = 1.into();
    for field in ["state", "signature"] {
        unsigned.as_object_mut().unwrap().remove(field);
    }
    write(&unsigned);
    let loaded = load(&path).expect("a snapshot of format 1 loads with no state digest or signature");
    assert_eq!((loaded.schema, loaded.offset, loaded.engine.last_seq()), (1, offset, Some(offset.seq)));
    assert_eq!(list(&dir).unwrap(), [(offset, path.clone())], "it starts a range at its offset");

    for field in ["state", "signature"] {
        let mut signed = written.clone();
        signed.as_object_mut().unwrap().remove(field);
        write(&signed);
        let error = format!("{:#}", load(&path).err().expect("refused"));
        assert!(error.contains("carries no state digest or signature"), "without {field}: {error}");
    }
}

/// An engine at `V27` with custody finality that applied commands up to `seq`, the state a range can start from.
fn finality_engine(seq: u64) -> Engine {
    let mut engine = Engine::new();
    let admin = |cmd| engine_core::command::Command::Admin(cmd);
    let mut commands = vec![
        admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)),
        admin(AdminCommand::SetOperator(Operator {
            address: AccountId([8; 20]),
            chain_id: 31337,
            vault: AccountId([9; 20]),
        })),
    ];
    commands.extend(
        [ProtocolVersion::V21, ProtocolVersion::V22, ProtocolVersion::V27]
            .map(|version| admin(AdminCommand::ActivateProtocol { version })),
    );
    commands.resize(seq as usize + 1, engine_core::command::Command::Tick);
    for (seq, cmd) in commands.into_iter().enumerate() {
        engine.apply(&engine_core::command::Sequenced { seq: seq as u64, ts: 0, cmd }).unwrap();
    }
    engine
}

/// A writer busy with a snapshot is not ready, a day it cannot take is skipped, and its drop waits for its
/// thread a bounded time, whatever the thread waits for.
#[test]
fn a_busy_writer_skips_the_day_and_its_drop_waits_a_bounded_time() {
    let dir = scratch_dir("daily-busy");
    let mut writer = Writer::start(&dir, None).unwrap();
    assert!(writer.ready() && writer.due(0));
    // An encoding that never comes holds the thread.
    let (_encoder, pending) = std::sync::mpsc::channel::<crate::stats::background::Encoded>();
    writer.take(Job { engine: Encoding::Pending(pending), ..job(Offset { seq: 5, index: 1 }, 0) });
    // The thread clears `ready` as it takes the job; yielding to it, with no sleep, waits for that and no time.
    while writer.ready() {
        std::thread::yield_now();
    }
    assert!(!writer.due(1), "the day is taken");
    assert!(writer.due(DAY_MS) && !writer.ready());
    writer.skip(6, DAY_MS, "the writer is still writing the last one");
    assert!(!writer.due(DAY_MS + 1), "a skipped day is taken");
    let dropping = std::time::Instant::now();
    drop(writer);
    assert!(dropping.elapsed() < std::time::Duration::from_secs(10), "the drop waited {:?}", dropping.elapsed());
}

/// A day the sequencer skipped, or could not encode, raises `daily_snapshot_failed`, and the next snapshot
/// handed over resolves it, though the thread failed no publish itself; a snapshot published with none
/// raised resolves nothing.
#[test]
fn a_published_snapshot_resolves_the_alert_its_senders_end_raised() {
    let (_logging, _alerts, mut resolved) = crate::test_support::capture_alerts_and_resolutions();
    let mut resolutions = |name: &str, raise: fn(&mut Writer)| {
        let mut writer = Writer::start(&scratch_dir(name), None).unwrap();
        raise(&mut writer);
        writer.take(job(Offset { seq: 5, index: 1 }, DAY_MS));
        writer.finish();
        std::iter::from_fn(|| resolved.try_recv().ok()).count()
    };
    assert_eq!(resolutions("daily-resolve-none", |_| {}), 0, "nothing was raised");
    assert_eq!(resolutions("daily-resolve-skip", |w| w.skip(1, 0, "the writer is still writing")), 1, "a skipped day");
    assert_eq!(resolutions("daily-resolve-encoding", Writer::encoding_failed), 1, "an encoding that failed");
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
    let engine = Encoding::Encoded(serde_json::to_string(&engine).unwrap());
    Job { offset, checkpoint, ts, engine, signing: Signing::for_tests() }
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
