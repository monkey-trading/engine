//! Cache authentication, deployment binding, publication, retention, and corruption.

use super::*;
use std::num::NonZeroUsize;

use engine_core::command::{Command, Sequenced};

use crate::test_support::scratch_dir;

#[test]
fn snapshot_configuration_refuses_zero_limits_and_unknown_keys() {
    let text = "dir = 'cache'\nretain = 3\nmax_bytes = 1024\ntrusted_signers = []\n";
    let cfg: SnapshotSection = toml::from_str(text).unwrap();
    assert_eq!((cfg.retain.get(), cfg.max_bytes.get()), (3, 1024));
    for bad in [
        text.replace("retain = 3", "retain = 0"),
        text.replace("max_bytes = 1024", "max_bytes = 0"),
        text.replace("retain = 3", "retian = 3"),
        format!("{text}retian = 3\n"),
        text.replace("trusted_signers = []", "trusted_signers = ['not an address']"),
        text.replace("trusted_signers = []", ""),
    ] {
        assert!(toml::from_str::<SnapshotSection>(&bad).is_err(), "{bad}");
    }
}

#[test]
fn authenticates_payload_and_deployment_before_loading_state() {
    let (cfg, key, mut snapshot) = fixture("snapshot-trust");
    write(&cfg, &snapshot, &key).unwrap();
    let path = candidates(&cfg).unwrap().remove(0);
    let (_, mut restored) = read(&cfg, &path, 31337, snapshot.vault).unwrap();
    assert_eq!(hex::encode(restored.state_root()), snapshot.checkpoint.root[2..]);
    let untrusted = SnapshotSection { trusted_signers: vec![], ..cfg.clone() };
    assert!(
        read(&untrusted, &path, 31337, snapshot.vault).err().unwrap().to_string().contains("not independently trusted")
    );
    assert!(read(&cfg, &path, 1, snapshot.vault).is_err());
    assert!(read(&cfg, &path, 31337, AccountId([1; 20])).is_err());
    let mut envelope: Envelope = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    envelope.payload.push(' ');
    fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
    assert!(read(&cfg, &path, 31337, snapshot.vault).is_err(), "self-reported root cannot authorize altered payload");
    snapshot.checkpoint.root = format!("0x{}", "00".repeat(32));
    write(&cfg, &snapshot, &key).unwrap();
    assert!(read(&cfg, &path, 31337, snapshot.vault).err().unwrap().to_string().contains("state root mismatch"));
}

#[test]
fn retains_newest_caches_and_reclaims_staging_files() {
    let (cfg, key, mut snapshot) = fixture("snapshot-retain");
    for index in 1..5 {
        snapshot.offset.index = index;
        write(&cfg, &snapshot, &key).unwrap();
    }
    fs::write(cfg.dir.join("99999999999999999999-0000000000.json.tmp"), b"torn").unwrap();
    let paths = candidates(&cfg).unwrap();
    assert!(!cfg.dir.join("99999999999999999999-0000000000.json.tmp").exists());
    assert_eq!(paths.len(), 2);
    assert!(paths[0].ends_with("00000000000000000000-0000000004.json"));
    fs::write(&paths[0], b"{").unwrap();
    assert!(read(&cfg, &paths[0], 31337, snapshot.vault).is_err());
    let small = SnapshotSection { max_bytes: NonZeroUsize::new(1).unwrap(), ..cfg };
    assert!(write(&small, &snapshot, &key).is_err());
    assert!(read(&small, &paths[1], 31337, snapshot.vault).err().unwrap().to_string().contains("max_bytes"));
}

#[test]
fn failed_publication_removes_staging_and_restart_reclaims_partial_files() {
    let (cfg, key, snapshot) = fixture("snapshot-failed-publication");
    fs::create_dir_all(&cfg.dir).unwrap();
    let target = cfg.dir.join(name(snapshot.offset));
    fs::create_dir(&target).unwrap();
    assert!(write(&cfg, &snapshot, &key).err().unwrap().to_string().contains("publishing snapshot"));
    assert!(!cfg.dir.join(format!("{}.tmp", name(snapshot.offset))).exists());
    fs::remove_dir(target).unwrap();
    for seq in 0..8 {
        fs::write(cfg.dir.join(format!("{seq:020}-0000000001.json.tmp")), b"partial allocation").unwrap();
    }
    let unrelated = cfg.dir.join("operator-notes.json.tmp");
    fs::write(&unrelated, b"preserve").unwrap();
    assert!(candidates(&cfg).unwrap().is_empty());
    assert_eq!(fs::read_dir(&cfg.dir).unwrap().count(), 1);
    assert_eq!(fs::read(unrelated).unwrap(), b"preserve");
    write(&cfg, &snapshot, &key).unwrap();
    assert_eq!(candidates(&cfg).unwrap().len(), 1);
}

#[test]
fn startup_reclaims_catalog_head_staging_files_of_every_generation() {
    let (cfg, _, _) = fixture("snapshot-catalog-staging");
    fs::create_dir_all(&cfg.dir).unwrap();
    let head = |id: u8, nonce: u64| format!("catalog-head-{}-{nonce:020}.json", hex::encode([id; 32]));
    let staged = [cfg.dir.join(format!("{}.tmp", head(1, 7))), cfg.dir.join(format!("{}.tmp", head(2, 3)))];
    let kept = [cfg.dir.join(head(1, 6)), cfg.dir.join("operator-notes.json.tmp")];
    for path in staged.iter().chain(&kept) {
        fs::write(path, b"bytes").unwrap();
    }
    reclaim(&cfg, &mut PlainPrune).unwrap();
    assert!(staged.iter().all(|path| !path.exists()), "an abandoned generation's staging file is reclaimed");
    assert!(kept.iter().all(|path| path.exists()), "published heads and unrecognized files stay");
}

/// A rewound catalog removes its generation's heads past the boundary oldest first, so the newest
/// one goes last; another generation's heads and staging files stay.
#[test]
fn catalog_heads_past_a_version_list_oldest_first_for_their_generation_only() {
    let (cfg, _, _) = fixture("snapshot-catalog-heads-after");
    fs::create_dir_all(&cfg.dir).unwrap();
    let head = |id: u8, nonce: u64| format!("catalog-head-{}-{nonce:020}.json", hex::encode([id; 32]));
    for name in [head(1, 3), head(1, 12), head(1, 5), head(1, 7), head(2, 9), format!("{}.tmp", head(1, 13))] {
        fs::write(cfg.dir.join(name), b"bytes").unwrap();
    }
    let after = |version| catalog_heads::after(&cfg, head::StoreId([1; 32]), version).unwrap();
    assert_eq!(after(Some(5)), [cfg.dir.join(head(1, 7)), cfg.dir.join(head(1, 12))]);
    assert_eq!(after(None).len(), 4, "a boundary without a version keeps none of its generation's heads");
    assert!(after(Some(12)).is_empty());
}

#[test]
fn partial_write_and_sync_failures_release_staging_allocation() {
    let (cfg, _, snapshot) = fixture("snapshot-failed-io");
    fs::create_dir_all(&cfg.dir).unwrap();
    let staged = cfg.dir.join(format!("{}.tmp", name(snapshot.offset)));
    let target = cfg.dir.join(name(snapshot.offset));
    for written in [0, 16, 4096] {
        let result = publish(&staged, &target, |file| {
            file.write_all(&vec![1; written])?;
            Err(std::io::Error::other("injected write/sync failure").into())
        });
        assert!(result.unwrap_err().to_string().contains("injected write/sync failure"));
        assert!(!staged.exists(), "partial allocation of {written} bytes must be released");
        assert!(!target.exists(), "failed staging must never publish");
    }
}

#[test]
fn post_rename_failures_bound_retries_and_restart_preserves_durable_caches() {
    for failure in ["opening snapshot directory", "syncing snapshot directory"] {
        let (cfg, key, mut snapshot) = fixture("snapshot-post-rename");
        let mut durable = Vec::new();
        for index in 1..=cfg.retain.get() as u32 {
            snapshot.offset.index = index;
            write(&cfg, &snapshot, &key).unwrap();
            durable.push(cfg.dir.join(name(snapshot.offset)));
        }
        let before: Vec<_> = durable.iter().map(|path| fs::read(path).unwrap()).collect();
        let failed_index = cfg.retain.get() as u32 + 1;
        for index in failed_index..failed_index + 8 {
            snapshot.offset.index = index;
            let err = write_with_sync(&cfg, &snapshot, &key, &|_| anyhow::bail!(failure)).unwrap_err();
            assert_eq!(err.to_string(), failure);
            assert_eq!(fs::read_dir(&cfg.dir).unwrap().count(), cfg.retain.get() + 1);
            for (path, bytes) in durable.iter().zip(&before) {
                assert_eq!(&fs::read(path).unwrap(), bytes, "earlier durable caches must survive failed syncs");
            }
            assert!(cfg.dir.join(format!("{:020}-{failed_index:010}.json", snapshot.offset.seq)).exists());
            if index > failed_index {
                assert!(!cfg.dir.join(name(snapshot.offset)).exists(), "cleanup failure must block allocation");
            }
        }
        let unrelated = cfg.dir.join("operator-notes.json");
        fs::write(&unrelated, b"preserve").unwrap();
        // Discovery is the restart path: it syncs the replacement before removing an older cache.
        let recovered = candidates(&cfg).unwrap();
        assert_eq!(recovered.len(), cfg.retain.get());
        assert!(recovered[0].ends_with(format!("{:020}-{failed_index:010}.json", snapshot.offset.seq)));
        assert_eq!(fs::read(&unrelated).unwrap(), b"preserve");
        for path in &recovered {
            read(&cfg, path, 31337, snapshot.vault).unwrap();
        }
        snapshot.offset.index += 1;
        write(&cfg, &snapshot, &key).unwrap();
        assert_eq!(candidates(&cfg).unwrap().len(), cfg.retain.get());
    }
}

#[test]
fn pruning_failures_block_allocation_until_restart_cleanup_succeeds() {
    let (cfg, key, mut snapshot) = fixture("snapshot-prune-failure");
    let mut durable = Vec::new();
    for index in 2..=3 {
        snapshot.offset.index = index;
        write(&cfg, &snapshot, &key).unwrap();
        durable.push(cfg.dir.join(name(snapshot.offset)));
    }
    let before: Vec<_> = durable.iter().map(|path| fs::read(path).unwrap()).collect();
    let blocked = cfg.dir.join(name(Offset { index: 1, ..snapshot.offset }));
    fs::create_dir(&blocked).unwrap();
    assert_eq!(candidates(&cfg).unwrap_err().to_string(), "pruning snapshot");
    for index in 4..12 {
        snapshot.offset.index = index;
        assert_eq!(write(&cfg, &snapshot, &key).unwrap_err().to_string(), "pruning snapshot");
        assert!(!cfg.dir.join(name(snapshot.offset)).exists(), "failed cleanup must precede allocation");
        assert_eq!(fs::read_dir(&cfg.dir).unwrap().count(), cfg.retain.get() + 1);
        for (path, bytes) in durable.iter().zip(&before) {
            assert_eq!(&fs::read(path).unwrap(), bytes);
            read(&cfg, path, 31337, snapshot.vault).unwrap();
        }
    }
    fs::remove_dir(&blocked).unwrap();
    assert_eq!(candidates(&cfg).unwrap(), durable.into_iter().rev().collect::<Vec<_>>());
    write(&cfg, &snapshot, &key).unwrap();
    let paths = candidates(&cfg).unwrap();
    assert_eq!(paths.len(), cfg.retain.get());
    assert_eq!(paths[0], cfg.dir.join(name(snapshot.offset)));
}

#[test]
fn failed_retention_sync_preserves_durable_replacements_and_bounded_retries() {
    use std::cell::Cell;

    let (cfg, key, mut snapshot) = fixture("snapshot-retention-sync-failure");
    for index in 1..=2 {
        snapshot.offset.index = index;
        write(&cfg, &snapshot, &key).unwrap();
    }
    for index in 3..11 {
        snapshot.offset.index = index;
        let calls = Cell::new(0);
        let err = write_with_sync(&cfg, &snapshot, &key, &|path| {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                anyhow::bail!("injected retention sync failure");
            }
            sync_directory(path)
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "injected retention sync failure");
        assert_eq!(calls.get(), 3, "publication and replacement sync precede retention sync");
        let paths = candidates(&cfg).unwrap();
        assert_eq!(paths.len(), cfg.retain.get());
        for (path, retained_index) in paths.iter().zip([index, index - 1]) {
            assert_eq!(*path, cfg.dir.join(name(Offset { index: retained_index, ..snapshot.offset })));
            read(&cfg, path, 31337, snapshot.vault).unwrap();
        }
        assert!(!cfg.dir.join(name(Offset { index: index - 2, ..snapshot.offset })).exists());
    }
    snapshot.offset.index += 1;
    write(&cfg, &snapshot, &key).unwrap();
    let paths = candidates(&cfg).unwrap();
    assert_eq!(paths.len(), cfg.retain.get());
    assert_eq!(paths[0], cfg.dir.join(name(snapshot.offset)));
}

pub(super) fn fixture(label: &str) -> (SnapshotSection, EnclaveKey, Snapshot) {
    let key = EnclaveKey::generate();
    let cfg = SnapshotSection {
        dir: scratch_dir(label),
        retain: NonZeroUsize::new(2).unwrap(),
        max_bytes: NonZeroUsize::new(16 * 1024 * 1024).unwrap(),
        trusted_signers: vec![AccountId(key.address())],
    };
    let mut engine = Engine::new();
    let sequenced = Sequenced { seq: 0, ts: 0, cmd: Command::Tick };
    let events = engine.apply(&sequenced).unwrap();
    let command = crate::log::CommandEntry { seal: None, sequenced, events: Some(events) };
    let command_prefix = crate::log::extend_command_prefix([0; 32], crate::log::command_digest(&command).unwrap());
    let root = engine.state_root();
    let vault = AccountId([0; 20]);
    let checkpoint = Checkpoint {
        seq: 0,
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint(31337, &vault.0, 0, &root),
        domain: Some(crate::log::CheckpointDomain { chain_id: 31337, vault }),
        parent: None,
        deferred_withdrawals: false,
    };
    let snapshot = Snapshot {
        schema: 2,
        pair: None,
        chain_id: 31337,
        vault,
        checkpoint,
        offset: Offset { seq: 0, index: 1 },
        engine: serde_json::to_string(&engine).unwrap(),
        progress: Progress::default(),
        context: RecoveryContext {
            commands: 1,
            command_prefix,
            accounting: crate::checkpoints::accounting::Accounting::default(),
            logs_events: true,
            unsigned: Vec::new(),
            deferred: Vec::new(),
            checkpoint_signers: [AccountId(key.address())].into(),
            base: RecoveryBase::Ungated,
            work: Vec::new(),
        },
    };
    (cfg, key, snapshot)
}
