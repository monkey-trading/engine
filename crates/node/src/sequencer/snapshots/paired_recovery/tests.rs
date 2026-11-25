//! The signed parent chain a start from a cache checks from the cache's base to the vault's finalized
//! checkpoint over the suffix it replays.

use super::*;

/// Over the suffixes a start from a cache can replay, the chain holds only when each signed checkpoint
/// up to the anchor names the one before it, from the base on, and the last of them is the anchor: one
/// that names another parent, repeats the one before it or follows a dropped one breaks it, and so does
/// a suffix that stops short of the anchor; a checkpoint past the anchor is not the chain's, and an
/// anchor below the base, a base with no parent, or no anchor checks nothing.
#[test]
fn the_chain_holds_only_from_the_base_to_the_anchor() {
    let before = checkpoint(5, 9, None);
    let base = checkpoint(10, 1, Some(&before));
    let c20 = checkpoint(20, 2, Some(&base));
    let c30 = checkpoint(30, 3, Some(&c20));
    let c40 = checkpoint(40, 4, Some(&c30));
    let at = |checkpoint: &Checkpoint| Some(link(checkpoint).unwrap());
    let cases = [
        ("whole", &base, at(&c30), vec![&c20, &c30, &c40], Some(true)),
        ("anchor at the base", &base, at(&base), vec![&c20, &c30], Some(true)),
        ("repeated", &base, at(&c30), vec![&c20, &c20, &c30], Some(false)),
        ("dropped", &base, at(&c30), vec![&c30], Some(false)),
        ("another parent", &base, at(&c30), vec![&c30, &c20], Some(false)),
        ("stops short", &base, at(&c30), vec![&c20, &c40], Some(false)),
        ("ends before the anchor", &base, at(&c30), vec![&c20], Some(false)),
        ("anchor below the base", &base, at(&before), vec![&c40], None),
        ("base with no parent", &before, at(&c30), vec![&c20], None),
        ("no anchor", &base, None, vec![&c20], None),
    ];
    for (name, base, anchor, suffix, holds) in cases {
        let checked = AnchorChain::new(base, anchor).unwrap().map(|mut chain| {
            suffix.iter().try_for_each(|checkpoint| chain.take(&LogEntry::Checkpoint((*checkpoint).clone())))?;
            chain.finish()
        });
        assert_eq!(checked.map(|result| result.is_ok()), holds, "{name}");
    }
}

/// A paired start with no cache to take replays the log after a daily snapshot, the segments before it gone from this
/// host, to the state a whole replay gives, its fingerprint naming the entries it read; a head posted at or under the
/// snapshot's checkpoint is bound to the history the snapshot carries before it replays, and one of another history
/// refuses the start. With the whole log, the history rebuilt from that fingerprint activates.
#[tokio::test]
async fn the_fallback_replays_the_log_after_a_daily_snapshot() {
    let cfg = crate::node::paired_tests::harness::configuration("paired-daily");
    let (dir, daily) = (cfg.node.log_dir.clone(), crate::test_support::scratch_dir("paired-daily-snapshots"));
    crate::sequencer::fixture_tests::write_funded_log(&dir, 4);
    let entries = crate::test_support::log_entries(&dir);
    let signer = AccountId(crate::tee::EnclaveKey::test_key().address());
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let root = crate::sequencer::replay_path_with_trust(&dir, Some(domain), Some(signer), Default::default())
        .unwrap()
        .engine
        .state_root();
    let at = entries.iter().rposition(|entry| matches!(entry, LogEntry::Checkpoint(_))).unwrap() - 1;
    let at = entries[..=at].iter().rposition(|entry| matches!(entry, LogEntry::Checkpoint(_))).unwrap();
    let path = publish_daily(&entries, &daily, at, domain);
    let opened = || crate::daily_snapshots::open_signed(&path, domain, &[signer].into()).unwrap();
    let (base, carried) = (opened().snapshot.offset, opened().snapshot.carried.unwrap());
    let replay = |capture, removed: bool| {
        let (mut log, plan) = Sequencer::replay_paired_log(
            &cfg,
            Some(signer),
            None,
            Default::default(),
            None,
            capture,
            Some(opened()),
            |_| {},
        )?;
        assert_eq!(log.snapshot_seq, Some(base.seq), "the start is the snapshot's");
        assert_eq!(log.replayed.engine.state_root(), root, "removed segments: {removed}");
        let Source::Rebuild { prefix } = &plan.source else { panic!("a fallback rebuilds the history") };
        assert_eq!(prefix.after, Some(base), "the fingerprint names the entries after the snapshot");
        anyhow::Ok((log, plan))
    };
    let (log, plan) = replay(None, false).unwrap();
    let key = std::sync::Arc::new(crate::tee::EnclaveKey::test_key());
    crate::history::HistoryPlan::authenticated(plan).unwrap().finish_with_log(log, key).unwrap();
    remove_segments_before(&dir, base.seq);
    replay(None, true).unwrap();
    let link = |seq: u64| carried.links.iter().find(|link| link.seq == seq).map(|link| link.link).unwrap();
    let held = crate::log_heads::Head { seq: base.seq - 1, link: link(base.seq - 1) };
    let (log, _) = replay(Some(held), true).unwrap();
    assert_eq!(log.replayed.ts_at, Some(carried.links.last().unwrap().ts), "the stamp after the head");
    let foreign = crate::log_heads::Head { seq: base.seq - 1, link: engine_core::eip712::B256::repeat_byte(5) };
    let refused = replay(Some(foreign), true).err().expect("another history refuses the start");
    assert!(refused.is::<crate::log_heads::prefix::Rollback>(), "{refused:#}");
}

// ── Helpers ──

/// Publishes in `daily` the daily snapshot of the checkpoint at `entries[at]`, signed by the test enclave in `domain`
/// and carrying what a whole replay holds there, and gives its path.
fn publish_daily(entries: &[LogEntry], daily: &Path, at: usize, domain: CheckpointDomain) -> std::path::PathBuf {
    use crate::snapshot::sealing::Encoding;
    let LogEntry::Checkpoint(checkpoint) = entries[at].clone() else { panic!("no checkpoint at entry {at}") };
    let offset = entries[..=at].iter().fold(None, |last, entry| crate::log::offset_of(last, entry).ok()).unwrap();
    let signer = AccountId(crate::tee::EnclaveKey::test_key().address());
    let mut replay = Replay::with_trust(Some(domain), Some(signer));
    entries[..=at].iter().for_each(|entry| replay.apply(entry).unwrap());
    let carried = serde_json::to_string(&replay.carried().unwrap()).unwrap();
    let engine = replay.finish().engine;
    let signing =
        crate::daily_snapshots::Signing { key: std::sync::Arc::new(crate::tee::EnclaveKey::test_key()), domain };
    let job = crate::daily_snapshots::Job {
        offset,
        checkpoint,
        ts: engine.now(),
        engine: Encoding::Encoded(serde_json::to_string(&engine).unwrap()),
        carried: Encoding::Encoded(carried),
        history: crate::daily_snapshots::Pending::none(),
        signing,
    };
    daily.join(crate::daily_snapshots::publish_now(daily, job).unwrap().file)
}

/// Removes the log's segment files in `dir` before the one holding the command at `seq`.
fn remove_segments_before(dir: &Path, seq: u64) {
    let mut segments: Vec<(u64, std::path::PathBuf)> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.unwrap().path();
            Some((crate::log::segment_seq(path.file_name()?.to_str()?)?, path))
        })
        .collect();
    segments.sort();
    let holding = segments.iter().rposition(|(first, _)| *first <= seq).expect("a segment holds the seq");
    assert!(holding > 0, "a segment before the one holding seq {seq}");
    segments[..holding].iter().for_each(|(_, path)| std::fs::remove_file(path).unwrap());
}

/// A checkpoint at `seq` whose root is all `byte`, naming `parent` as its parent, or none.
fn checkpoint(seq: u64, byte: u8, parent: Option<&Checkpoint>) -> Checkpoint {
    Checkpoint {
        parent: parent.map(|parent| link(parent).unwrap()),
        seq,
        root: format!("0x{}", hex::encode([byte; 32])),
        signature: format!("0x{}", "00".repeat(65)),
        signer: format!("0x{}", "11".repeat(20)),
        domain: None,
        deferred_withdrawals: false,
    }
}
