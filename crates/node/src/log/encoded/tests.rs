use std::fs;
use std::path::Path;

use engine_core::command::Command;
use engine_core::types::AccountId;
use tokio::sync::watch;

use super::*;
use crate::log::command_seal::command_link;
use crate::log::{CheckpointDomain, CommandEntry, LogEntry, Manifest, Offset, SegmentedLog, command_digest};
use crate::tee::EnclaveKey;
use crate::test_support::events::every_event;
use crate::test_support::scratch_dir;

#[test]
fn every_reference_log_command_encodes_to_its_own_line_link_and_digest() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/reference");
    let mut logs = 0;
    for file in fs::read_dir(&dir).unwrap().map(|file| file.unwrap().path()) {
        let text = fs::read_to_string(&file).unwrap();
        let mut previous = B256::ZERO;
        for line in text.lines() {
            let LogEntry::Command(entry) = serde_json::from_str(line).unwrap() else { continue };
            let Some(events) = &entry.events else { continue };
            let raw = encode_events(events).unwrap();
            let encoded = EncodedCommand::of(&entry, &raw);
            assert_eq!(encoded.line().unwrap(), format!("{line}\n").into_bytes(), "{}", file.display());
            assert_same(&entry, &encoded, previous);
            previous = encoded.link(previous).unwrap();
        }
        logs += 1;
    }
    assert!(logs > 70, "every reference log is read: {logs}");
}

#[test]
fn a_command_of_every_event_encodes_as_its_typed_entry_sealed_and_unsealed() {
    let key = EnclaveKey::test_key();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
    let previous = B256::repeat_byte(3);
    let commands = [
        Command::Tick,
        Command::Deposit { account: AccountId([1; 20]), amount: engine_core::Fixed::ONE, chain_ref: "0x\"\\é".into() },
    ];
    for cmd in commands {
        for events in [every_event(), Vec::new()] {
            let mut entry = CommandEntry {
                sequenced: Sequenced { seq: 41, ts: 1_792_000_000_000, cmd: cmd.clone() },
                events: Some(events),
                seal: None,
            };
            let raw = encode_events(entry.events.as_deref().unwrap()).unwrap();
            assert_same(&entry, &EncodedCommand::of(&entry, &raw), previous);
            entry.seal = Some(key.seal_command(domain, command_link(previous, &entry).unwrap()));
            assert_same(&entry, &EncodedCommand::of(&entry, &raw), previous);
        }
    }
}

#[test]
fn a_writer_appends_an_encoded_command_as_it_appends_the_typed_entry() {
    // Segments of two commands, so the third command completes the first segment and starts another.
    let entries: Vec<CommandEntry> = (0..5)
        .map(|seq| CommandEntry {
            sequenced: Sequenced { seq, ts: 1_792_000_000_000, cmd: Command::Tick },
            events: Some(if seq % 2 == 0 { every_event() } else { Vec::new() }),
            seal: None,
        })
        .collect();
    let (typed, encoded) = (scratch_dir("encoded-typed"), scratch_dir("encoded-raw"));
    let manifests = [&typed, &encoded].map(|dir| {
        let (sender, manifest) = watch::channel(Manifest::default());
        let mut writer = SegmentedLog::open(dir, |_| Ok(())).unwrap().into_writer(2, sender).unwrap();
        for entry in &entries {
            let offset = if *dir == typed {
                writer.append(&LogEntry::Command(entry.clone())).unwrap()
            } else {
                let raw = encode_events(entry.events.as_deref().unwrap()).unwrap();
                writer.append_command(&EncodedCommand::of(entry, &raw)).unwrap()
            };
            assert_eq!(offset, Offset::start_of(entry.sequenced.seq));
        }
        writer.flush().unwrap();
        manifest.borrow().clone()
    });
    assert_eq!(manifests[0].flushed, manifests[1].flushed);
    assert_eq!(manifests[0].active.as_ref().map(|a| &a.info), manifests[1].active.as_ref().map(|a| &a.info));
    let segments = |dir: &Path| -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|file| file.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .map(|path| (path.file_name().unwrap().to_string_lossy().into_owned(), fs::read(&path).unwrap()))
            .collect();
        files.sort();
        files
    };
    let written = segments(&typed);
    assert!(
        written.len() > 2,
        "five commands, two a segment: {:?}",
        written.iter().map(|(name, _)| name).collect::<Vec<_>>()
    );
    assert_eq!(written, segments(&encoded), "the same segment files, byte for byte");
}

// ── Helpers ──

/// Checks that `encoded`, `entry`'s events encoded, gives the line, the link after `previous` and
/// the digest the typed entry gives.
fn assert_same(entry: &CommandEntry, encoded: &EncodedCommand<'_>, previous: B256) {
    let mut line = serde_json::to_vec(&LogEntry::Command(entry.clone())).unwrap();
    line.push(b'\n');
    let seq = entry.sequenced.seq;
    assert_eq!(encoded.line().unwrap(), line, "line at seq {seq}");
    assert_eq!(encoded.link(previous).unwrap(), command_link(previous, entry).unwrap(), "link at seq {seq}");
    assert_eq!(encoded.digest().unwrap(), command_digest(entry).unwrap(), "digest at seq {seq}");
}
