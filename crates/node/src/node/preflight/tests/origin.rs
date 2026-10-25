//! Integrity refusal occurs before absent/corrupt/WAL-backed history is changed.

use super::*;

use engine_core::command::{Command, Sequenced};
use tokio::sync::watch;

use crate::log::{CommandEntry, LogEntry, Manifest, SegmentedLog};
use crate::sequencer::replay;
use crate::test_support::{TestSequencer, log_entries, sequencer_config};

#[tokio::test]
async fn uncovered_ordinary_tail_preserves_missing_corrupt_and_wal_history() {
    for history_kind in 0..3 {
        let root = scratch_dir(&format!("origin-preflight-{history_kind}"));
        let mut cfg = dev_config();
        cfg.node.log_dir = root.join("log");
        cfg.history.dir = root.join("history");
        let node = TestSequencer::start(&cfg.node.log_dir, 1);
        node.stop().await;
        let mut engine = replay(&log_entries(&cfg.node.log_dir)).unwrap().engine;
        let sequenced = Sequenced { seq: engine.last_seq().unwrap() + 1, ts: engine.now(), cmd: Command::Tick };
        let events = engine.apply(&sequenced).unwrap();
        let mut writer = SegmentedLog::open(&cfg.node.log_dir, |_| Ok(()))
            .unwrap()
            .into_writer(10_000, watch::channel(Manifest::default()).0)
            .unwrap();
        writer.append(&LogEntry::Command(CommandEntry { sequenced, events: Some(events), seal: None })).unwrap();
        writer.sync().unwrap();
        drop(writer);
        let mut connection = None;
        if history_kind > 0 {
            std::fs::create_dir_all(&cfg.history.dir).unwrap();
            let path = cfg.history.dir.join(crate::history::INDEX_FILE);
            if history_kind == 1 {
                std::fs::write(path, b"keep diagnostic bytes").unwrap();
            } else {
                let db = rusqlite::Connection::open(path).unwrap();
                db.execute_batch("PRAGMA journal_mode=wal; CREATE TABLE diagnostic(value TEXT); INSERT INTO diagnostic VALUES('keep WAL');").unwrap();
                connection = Some(db);
            }
        }
        let before = files(&root);
        let signer = AccountId(EnclaveKey::test_key().address());
        let (log, plan) = crate::node::read_log_and_history_observe(&cfg, Some(signer), None, |_| {}).unwrap();
        let seq_cfg = sequencer_config(&cfg.node.log_dir, watch::channel(Manifest::default()).0);
        let error = finish_preflight::<Chain>(plan, None, Some((&log, &seq_cfg, signer))).await.err().unwrap();
        assert!(error.to_string().contains("unauthenticated command tail"), "{error:#}");
        assert_eq!(files(&root), before, "integrity refusal precedes writable history: kind {history_kind}");
        assert_eq!(cfg.history.dir.exists(), history_kind > 0);
        drop(connection);
    }
}
