//! History head authority, prefix matching, and publication failures.

use super::*;

use std::num::NonZeroUsize;

use engine_core::command::{Command, Sequenced};

use crate::log::{CommandEntry, LogEntry};
use crate::test_support::scratch_dir;

#[test]
fn complete_head_requires_independent_authority_deployment_and_exact_replayed_prefix() {
    let (cfg, domain, id, store, key) = fixture("history-head-authority");
    let mut index = Index::empty(store.clone(), id.history_namespace(), 1000).unwrap();
    assert!(write(&cfg, domain, id, &store, index.head(), &key).is_err());
    let commands = (0..3).map(tick).collect::<Vec<_>>();
    for command in &commands {
        index.ingest(command.clone()).unwrap();
    }
    let statement = write(&cfg, domain, id, &store, index.head(), &key).unwrap();
    let path = candidates_readonly(&cfg).unwrap().remove(0);
    let restored = read(&cfg, &path, domain).unwrap();
    assert_eq!(restored.head, statement.head);
    let replay = crate::sequencer::replay(&commands.into_iter().map(LogEntry::Command).collect::<Vec<_>>()).unwrap();
    restored.verify_prefix(2, 2, replay.command_prefix, 1000).unwrap();
    assert!(restored.verify_prefix(2, 2, [0; 32], 1000).is_err());
    assert!(restored.verify_prefix(1, 2, replay.command_prefix, 1000).is_err());
    assert!(restored.verify_prefix(2, 3, replay.command_prefix, 1000).is_err());
    assert!(restored.verify_prefix(2, 2, replay.command_prefix, 999).is_err());
    let untrusted = SnapshotSection { trusted_signers: Vec::new(), ..cfg.clone() };
    assert!(read(&untrusted, &path, domain).is_err());
    assert!(read(&cfg, &path, CheckpointDomain { chain_id: 1, ..domain }).is_err());
    let mut envelope: Envelope = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    envelope.payload.push(' ');
    fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
    assert!(read(&cfg, &path, domain).is_err());
}

#[test]
fn head_publication_preserves_durable_pointers_and_bounds_repeated_postrename_failures() {
    let (cfg, domain, id, store, key) = fixture("history-head-postrename");
    let mut index = Index::empty(store.clone(), id.history_namespace(), 1000).unwrap();
    index.ingest(tick(0)).unwrap();
    let first = write(&cfg, domain, id, &store, index.head(), &key).unwrap();
    let earlier = cfg.dir.join(name(first.publication));
    let durable = fs::read(&earlier).unwrap();
    index.ingest(tick(1)).unwrap();
    let head = index.head();
    let failed_sync = |_: &Path| anyhow::bail!("injected directory fsync failure");
    for _ in 0..8 {
        assert!(write_with_sync(&cfg, domain, id, &store, head.clone(), &key, &failed_sync).is_err());
        assert_eq!(fs::read(&earlier).unwrap(), durable);
        assert!(candidates_readonly(&cfg).unwrap().len() <= RETAINED_HEADS + 1);
    }
    drop(index);
    drop(store);
    let reopened = Store::open_existing(id.path(&cfg.dir)).unwrap();
    let final_head = write(&cfg, domain, id, &reopened, head.clone(), &key).unwrap();
    let candidates = candidates_readonly(&cfg).unwrap();
    assert_eq!(candidates.len(), RETAINED_HEADS);
    assert!(!earlier.exists(), "earlier pointer is removed only after durable replacement reconciliation");
    assert_eq!(read(&cfg, &candidates[0], domain).unwrap().head, head);
    assert_eq!(final_head.head.last_seq(), Some(1));
}

#[test]
fn head_preflight_is_readonly_and_other_publication_groups_are_preserved() {
    let (cfg, domain, id, store, key) = fixture("history-head-preflight");
    let mut index = Index::empty(store.clone(), id.history_namespace(), 1000).unwrap();
    for seq in 0..5 {
        index.ingest(tick(seq)).unwrap();
        write(&cfg, domain, id, &store, index.head(), &key).unwrap();
    }
    let staging = cfg.dir.join("history-head-99999999999999999999.json.tmp");
    let foreign = cfg.dir.join("00000000000000000000-0000000001.json");
    fs::write(&staging, b"interrupted head write").unwrap();
    fs::write(&foreign, b"preserve the independent engine-cache group").unwrap();
    let before = files(&cfg.dir);
    for path in candidates_readonly(&cfg).unwrap() {
        read(&cfg, &path, domain).unwrap();
    }
    assert_eq!(files(&cfg.dir), before, "preflight must not reconcile interrupted writes");
    write(&cfg, domain, id, &store, index.head(), &key).unwrap();
    assert!(!staging.exists());
    assert_eq!(fs::read(&foreign).unwrap(), before[&foreign]);
}

/// The source carries no account events; the real engine and index still consume every command.
fn tick(seq: u64) -> CommandEntry {
    CommandEntry {
        seal: None,
        sequenced: Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick },
        events: Some(Vec::new()),
    }
}

fn fixture(label: &str) -> (SnapshotSection, CheckpointDomain, StoreId, Store, EnclaveKey) {
    let key = EnclaveKey::test_key();
    let cfg = SnapshotSection {
        dir: scratch_dir(label),
        retain: NonZeroUsize::new(3).unwrap(),
        max_bytes: NonZeroUsize::new(16 * 1024 * 1024).unwrap(),
        trusted_signers: vec![AccountId(key.address())],
    };
    fs::create_dir_all(&cfg.dir).unwrap();
    let id = StoreId([0x7a; 32]);
    let store = Store::create(id.path(&cfg.dir)).unwrap();
    (cfg, CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) }, id, store, key)
}

fn files(dir: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (path.clone(), fs::read(path).unwrap())
        })
        .collect()
}
