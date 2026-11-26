//! A start a promotion made: from the member's copy under its standby key, with the staking contract
//! checked before it connects.

use super::*;

/// A member a promotion made the vault's signer starts from its copy under its standby key, as the
/// promotion's start runs it through the node: it keeps every command the outgoing key sequenced past
/// the vault's latest checkpoint and seals its first command with the standby key (its checkpoints
/// wait for the promotion's finality, which `LiveReplay`'s anchor tests cover). A restart with the
/// promotion's record never cuts the copy back to the vault's anchor.
#[tokio::test]
async fn a_promoted_member_keeps_its_copy_and_extends_the_vaults_latest_under_its_standby_key() {
    let data = scratch_dir("ready-promoted");
    let log = data.join("log");
    let cfg = unreachable(&data, &log);
    write_log(&cfg).await;
    let outgoing = AccountId(EnclaveKey::test_key().address());
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
    let mut history = crate::history::HistoryPlan::open(&cfg.history).unwrap();
    let stats = Stats::new(&cfg).unwrap();
    let follower = Follower::open(&log, 4, domain, outgoing, [outgoing].into(), Some(stats.clone()), |entry| {
        history.observe(entry)
    })
    .unwrap();
    let last = follower.replica().last_seq().unwrap();
    let checkpoints: Vec<_> = log_entries(&log)
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .collect();
    // The vault's latest accepted checkpoint: what the outgoing key sequenced after it, it never saw.
    let anchor = checkpoints.iter().find(|checkpoint| checkpoint.seq < last).cloned().expect("an earlier checkpoint");
    let standby = EnclaveKey::generate();
    let promoted = AccountId(standby.address());
    let promotion = crate::standby_key::chain::Promotion {
        turn: 20_400,
        position: 1,
        signer_epoch: 2,
        generation: 9,
        head_seq: last,
        head_link: follower.replica().head_link(),
        checkpoint_seq: anchor.seq,
        checkpoint_root: anchor.root.parse().unwrap(),
        at: 0,
        block: 0,
        carried: 20_400,
    };
    let sealer = MockSealer { launch_digest: DIGEST };
    let sealing = cfg.tee.sealing.as_ref().unwrap();
    let store =
        KeyStore::create(&sealing.dir, &sealer, cfg.file_sha256, 9, KeySet::for_key(&standby).unwrap()).unwrap();
    let ready = Ready {
        setup: Some(setup(&cfg, &data, &anchor, promoted)),
        anchor: Some(anchor.clone()),
        emission_budget: None,
        staking_terms: None,
        governance_record: None,
        heads: None,
        generations: BTreeMap::new(),
        sample: sample_around(&crate::clock::SystemClock),
        certificates: None,
        promoted: Some(promotion),
        reassigned: None,
    };
    let moved = Moved { after: last, entries: 0, bytes: 0 };
    let replica = Replica { follower, stats, history, marks: Default::default() };
    let start = Start { replica, store, ready, moved, feeds: None };
    let metrics = Arc::new(Metrics::new());
    let (_layer, alerts) = crate::alerts::channel(crate::alerts::Routes::default(), metrics.clone());
    let mut routing = alerts.spawn(cfg.alerts.episode_quiet, &cfg.alerts.outbox).await.unwrap();
    let delivered = Delivered {
        values: Default::default(),
        version: None,
        answered: None,
        chain_read_keyed: false,
        listener: None,
        authority: None,
        store: None,
        archive: None,
    };
    let keys = crate::node::KeyStart { mock_launch_digest: Some(DIGEST), ..Default::default() };
    let mut chain_read = false;
    let running = crate::node::serve_node(
        &cfg,
        metrics.clone(),
        &mut routing,
        None,
        &keys,
        &delivered,
        &mut chain_read,
        Some(Box::new(start)),
        None,
    );
    tokio::pin!(running);
    tokio::select! {
        outcome = &mut running => panic!("the promoted node stopped before its first command: {outcome:?}"),
        () = crate::test_support::wait_for("the first command in the log", async {
            let follows = |log: &std::path::Path| {
                log_entries(log)
                    .into_iter()
                    .any(|entry| matches!(entry, LogEntry::Command(command) if command.sequenced.seq == last + 1))
            };
            while !follows(&log) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }) => {}
    }
    let commands: Vec<_> = log_entries(&log)
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(command) => Some(command),
            _ => None,
        })
        .collect();
    assert!(commands.iter().any(|command| command.sequenced.seq == last), "the outgoing key's tail is kept");
    let first = commands.iter().find(|command| command.sequenced.seq == last + 1).expect("a first command");
    assert_eq!(first.seal.as_ref().map(|seal| seal.signer), Some(promoted), "sealed by the standby key");
    // A restart finds the promotion's record: no start cuts the copy back to the anchor.
    let record = crate::node::member::promoted::Record::new(outgoing, promoted, promotion, last + 1);
    let path = sealing.dir.join(crate::node::member::promoted::RECORD);
    std::fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    let refused = crate::node::keys::cut_to_finalized_anchor(&cfg, &Arc::new(standby)).await.unwrap_err();
    assert!(format!("{refused:#}").contains("never cut back"), "{refused:#}");
    assert!(
        log_entries(&log)
            .iter()
            .any(|entry| matches!(entry, LogEntry::Command(command) if command.sequenced.seq == last))
    );
}

/// A promoted member's start checks the replica's and the genesis's staking config against the staking
/// contract before its other chain reads, as a handover's member start does, since the start after it
/// checks neither: here the contract's providers are unreachable, and the refusal names the contract.
#[tokio::test]
async fn a_promoted_start_checks_its_staking_config_against_the_contract() {
    let data = scratch_dir("ready-promoted-staking");
    let log = data.join("log");
    let cfg = unreachable(&data, &log);
    write_log(&cfg).await;
    let signer = AccountId(EnclaveKey::test_key().address());
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
    let follower = Follower::open(&log, 4, domain, signer, [signer].into(), None, |_| {}).unwrap();
    let promotion = crate::standby_key::chain::Promotion {
        turn: 1,
        position: 0,
        signer_epoch: 2,
        generation: 0,
        head_seq: 0,
        head_link: Default::default(),
        checkpoint_seq: 0,
        checkpoint_root: Default::default(),
        at: 0,
        block: 0,
        carried: 1,
    };
    let sample = std::future::ready(Err(anyhow::anyhow!("the start sampled time after its staking check failed")));
    let start = crate::node::KeyStart::default();
    let replica = follower.replica();
    let read = Ready::read_promoted(&cfg, &start, &Metrics::new(), signer, signer, replica, promotion, sample).await;
    let err = read.err().expect("a promoted start whose staking contract cannot be read read on");
    assert!(format!("{err:#}").contains("cannot read the staking contract"), "{err:#}");
}

/// A promoted start publishes the generation of the turn in effect its promotion is carried into, as well
/// as the promotion's own, so the first command in that turn is its `TurnTaken` before any read of the
/// schedule's generations.
#[tokio::test]
async fn a_promoted_start_publishes_the_generation_of_the_turn_it_carries_into() {
    let data = scratch_dir("ready-promoted-generations");
    let log = data.join("log");
    let mut cfg = unreachable(&data, &log);
    cfg.checkpoints = None;
    cfg.genesis.staking = None;
    cfg.custody.evm.as_mut().unwrap().staking = None;
    write_log(&cfg).await;
    let signer = AccountId(EnclaveKey::test_key().address());
    let domain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };
    let follower = Follower::open(&log, 4, domain, signer, [signer].into(), None, |_| {}).unwrap();
    let promotion = crate::standby_key::chain::Promotion {
        turn: 20_400,
        position: 1,
        signer_epoch: 2,
        generation: 9,
        head_seq: 0,
        head_link: Default::default(),
        checkpoint_seq: 0,
        checkpoint_root: Default::default(),
        at: 0,
        block: 0,
        carried: 20_401,
    };
    let sample = std::future::ready(Ok(sample_around(&crate::clock::SystemClock)));
    let start = crate::node::KeyStart::default();
    let metrics = Metrics::new();
    let read = Ready::read_promoted(&cfg, &start, &metrics, signer, signer, follower.replica(), promotion, sample);
    let ready = read.await.expect("a promoted start with no chain to read");
    assert_eq!(ready.generations, BTreeMap::from([(20_400, 9), (20_401, 9)]));
}
