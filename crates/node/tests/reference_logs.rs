//! Node reference logs: every committed log in `tests/reference/`, in the node's format with each
//! command's events and signed withdrawals, still parses and replays with its events checked. One
//! log is committed per protocol version and never regenerated, so a change to the events a command
//! emits, or to how they serialize, fails here.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;

use engine_core::{Engine, event::Event};
use engine_node::log::{LogEntry, read_file};

#[test]
fn every_node_reference_log_replays_with_its_events() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/reference");
    let mut logs: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    logs.sort();
    assert!(!logs.is_empty(), "no reference logs in {}", dir.display());
    for path in logs {
        let (mut without_events, mut withdrawals) = (0, 0);
        read_file(&path, |entry| {
            match entry {
                LogEntry::Command(c) if c.events.is_none() => without_events += 1,
                LogEntry::Withdrawal(_) => withdrawals += 1,
                _ => {}
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(without_events, 0, "{}: every command carries its events", path.display());
        assert!(withdrawals > 0, "{}: no withdrawal", path.display());
        // These immutable protocol fixtures intentionally carry placeholder enclave signatures.
        // Audit their roots/events here; the CLI's signature verifier refuses placeholders.
        let mut engine = Engine::new();
        let mut approved = VecDeque::new();
        let mut checkpoints = 0;
        read_file(&path, |entry| {
            match entry {
                LogEntry::Command(command) => {
                    assert!(approved.is_empty());
                    let events = engine.apply(&command.sequenced)?;
                    assert_eq!(command.events.as_ref(), Some(&events), "{}", path.display());
                    // A V22 payout is signed later, against an accepted checkpoint.
                    approved = events
                        .into_iter()
                        .filter_map(|event| match event {
                            Event::WithdrawalApproved { account, amount, nonce }
                                if engine.withdrawal_approval(account, nonce).is_none() =>
                            {
                                Some((account, amount, nonce))
                            }
                            _ => None,
                        })
                        .collect();
                }
                LogEntry::Withdrawal(withdrawal) => {
                    assert_eq!(approved.pop_front(), Some((withdrawal.account, withdrawal.amount, withdrawal.nonce)))
                }
                LogEntry::Checkpoint(checkpoint) => {
                    assert!(approved.is_empty());
                    assert_eq!(engine.last_seq(), Some(checkpoint.seq));
                    assert_eq!(
                        format!("0x{}", hex::encode(engine.state_root())),
                        checkpoint.root,
                        "{}",
                        path.display()
                    );
                    assert_eq!((checkpoint.signature.as_str(), checkpoint.signer.as_str()), ("0x", "0x"));
                    checkpoints += 1;
                }
            }
            Ok(())
        })
        .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
        assert!(checkpoints > 0 && approved.is_empty(), "{}", path.display());
    }
}
