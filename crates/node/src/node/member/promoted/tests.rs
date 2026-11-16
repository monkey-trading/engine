//! What a promoted member's copy must hold before it starts.

use alloy::primitives::B256;
use engine_core::command::Command;

use super::*;
use crate::log::{CheckpointDomain, LogEntry};
use crate::standby::Follower;
use crate::standby_key::{StandbyKeys, StandbyStart};
use crate::tee::EnclaveKey;
use crate::tee::sealing::{KeyStore, MockSealer};
use crate::test_support::log_entries;
use crate::test_support::{CHECKPOINT_EVERY, SEGMENT_COMMANDS, TestSequencer, scratch_dir};

/// The deployment the test sequencer seals and signs for.
const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) };

#[tokio::test]
async fn a_copy_lacking_either_prefix_the_promotion_fixed_names_what_it_lacks() {
    let dir = scratch_dir("promoted-prefixes");
    let node = TestSequencer::start(&dir, 1024);
    for _ in 0..12 {
        node.submit(Command::Tick).await;
    }
    node.stop().await;
    let signer = AccountId(EnclaveKey::test_key().address());
    let follower = Follower::open(&dir, SEGMENT_COMMANDS, DOMAIN, signer, [signer].into(), None, |_| {}).unwrap();
    let replica = follower.replica();
    let last = replica.last_seq().unwrap();
    let checkpoint = replica.last_checkpoint().cloned().expect("the log holds a checkpoint");
    let head = replica.link_at(last - 1).unwrap();
    let promotion = Promotion {
        turn: 20_400,
        position: 1,
        signer_epoch: 2,
        generation: 9,
        head_seq: head.seq,
        head_link: head.link,
        checkpoint_seq: checkpoint.seq,
        checkpoint_root: checkpoint.root.parse().unwrap(),
        at: 0,
        block: 0,
        carried: 20_400,
    };
    assert_eq!(missing(replica, &promotion), None, "both prefixes held");
    let ahead = Promotion { head_seq: last + 3, ..promotion };
    let lacking = missing(replica, &ahead).unwrap();
    assert!(lacking.contains(&format!("seqs {}..={}", last + 1, last + 3)), "{lacking}");
    let forked = Promotion { head_link: B256::repeat_byte(0xee), ..promotion };
    assert!(missing(replica, &forked).unwrap().contains("holds link"), "another history at the head");
    let unaccepted = Promotion { checkpoint_root: B256::repeat_byte(0xee), ..promotion };
    assert!(missing(replica, &unaccepted).unwrap().contains("holds root"));
    let later = Promotion { checkpoint_seq: last + CHECKPOINT_EVERY, ..promotion };
    assert!(missing(replica, &later).unwrap().contains("up to the vault's latest checkpoint"));
    let none = Promotion { head_seq: 0, checkpoint_seq: 0, ..promotion };
    assert_eq!(missing(replica, &none), None, "a first epoch with no head and no checkpoint fixes nothing");
}

/// Setting a removed promotion's history aside records its phase first and moves the record last, so
/// a start after a crash at any step finishes it: here the key was already moved when the process
/// stopped, and the next quarantine moves the log and the record and leaves nothing to reopen.
#[test]
fn an_interrupted_quarantine_finishes_from_its_recorded_phase() {
    let dir = scratch_dir("promoted-quarantine");
    let (sealing, log) = (dir.join("sealing"), dir.join("log"));
    std::fs::create_dir_all(&sealing).unwrap();
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(log.join("segment"), b"history").unwrap();
    let key = sealing.join("key-9.sealed.json");
    std::fs::write(&key, b"sealed").unwrap();
    let record = Record::new(AccountId([0xca; 20]), AccountId([0x5b; 20]), PROMOTION, 121);
    record.write(&sealing).unwrap();
    // The first attempt stops after its phase and the key's move.
    Record { quarantining: true, ..record }.write(&sealing).unwrap();
    std::fs::rename(&key, sealing.join("key-9.sealed.json.quarantined-20400-1-0")).unwrap();
    let loaded = Record::load(&sealing).unwrap().expect("the record stays until the quarantine ends");
    assert!(loaded.quarantining, "a start finds the phase");
    quarantine(&log, &sealing, &key, &loaded).unwrap();
    assert!(!log.exists() && dir.join("log.quarantined-20400-1-0").join("segment").exists(), "the log is aside");
    assert!(Record::load(&sealing).unwrap().is_none(), "the record goes last");
    assert!(sealing.join(format!("{RECORD}.quarantined-20400-1-0")).exists());
    // A quarantine stopped by a failed rename has recorded its phase already: here the log's target is
    // taken, so the key moves and the log does not.
    let stopped = Record { turn: 20_402, ..record };
    std::fs::create_dir_all(&log).unwrap();
    std::fs::create_dir_all(dir.join("log.quarantined-20402-1-0").join("taken")).unwrap();
    std::fs::write(&key, b"sealed").unwrap();
    stopped.write(&sealing).unwrap();
    assert!(quarantine(&log, &sealing, &key, &stopped).is_err());
    assert!(!key.exists() && log.exists());
    assert!(Record::load(&sealing).unwrap().is_some_and(|loaded| loaded.quarantining), "the phase came first");
    std::fs::remove_dir_all(&log).unwrap();
    // A fresh quarantine, of another turn's promotion, moves everything.
    let fresh = Record { turn: 20_401, ..record };
    std::fs::create_dir_all(&log).unwrap();
    std::fs::write(&key, b"sealed").unwrap();
    fresh.write(&sealing).unwrap();
    quarantine(&log, &sealing, &key, &fresh).unwrap();
    assert!(!key.exists() && !log.exists() && Record::load(&sealing).unwrap().is_none());
}

/// A start after a promotion routes by what the chain shows of it, over a copy holding a trade the
/// promoted member sequenced and the keys resealed for it: standing, it resumes; unsettled or behind,
/// it waits with everything in place; passed or ended, it follows; removed by a reorg, it sets that
/// exact history aside with the resealed keys, so no start reopens the promoted key, which a new
/// standby key replaces, nor resumes under its epoch.
#[tokio::test]
async fn a_start_after_a_promotion_routes_by_its_standing_and_sets_a_removed_ones_trade_aside() {
    let standings =
        [Standing::Stands, Standing::Unsettled, Standing::Behind, Standing::Passed, Standing::Ended, Standing::Gone];
    for standing in standings {
        let dir = scratch_dir(&format!("promoted-settle-{standing:?}"));
        let (sealing, log) = (dir.join("sealing"), dir.join("log"));
        let traded = sequence_a_trade(&log).await;
        let keys = StandbyKeys::sealed_for_test(&sealing, LAUNCH, CONFIG);
        let promoted = keys.address();
        let store = keys.reseal(PROMOTION.generation).unwrap();
        let key = crate::tee::sealing::file_for(&sealing, &store.release(), PROMOTION.generation);
        let record = Record::new(AccountId([0xca; 20]), promoted, PROMOTION, traded);
        record.write(&sealing).unwrap();
        let resumed = settle(standing, &record, (&log, &sealing, &key)).unwrap();
        let held = (log.exists(), key.exists(), Record::load(&sealing).unwrap().is_some());
        match standing {
            Standing::Stands => assert!(resumed.is_none() && held == (true, true, true), "resumes from the copy"),
            Standing::Unsettled | Standing::Behind => {
                assert!(matches!(resumed, Some(Resumed::Wait)) && held == (true, true, true), "{standing:?}: waits");
            }
            Standing::Passed => assert!(matches!(resumed, Some(Resumed::None)) && held == (true, true, true)),
            Standing::Ended => assert!(matches!(resumed, Some(Resumed::None)) && held == (true, true, false)),
            Standing::Gone => {
                assert!(matches!(resumed, Some(Resumed::None)) && held == (false, false, false), "set aside");
                let aside = dir.join("log.quarantined-20400-1-0");
                let fills = log_entries(&aside).into_iter().filter(|entry| match entry {
                    LogEntry::Command(command) => command.sequenced.seq == traded,
                    _ => false,
                });
                assert_eq!(fills.count(), 1, "the trade the promotion's history holds is aside");
                let sealer = MockSealer { launch_digest: LAUNCH };
                let reopened = KeyStore::open(&sealing, &sealer, CONFIG, PROMOTION.generation).unwrap();
                assert!(reopened.is_none(), "the promoted key opens as no generation's keys");
                let chain = crate::liveness::fake::FakeChain::new(Default::default());
                let domain = crate::liveness::fake::DOMAIN.standby;
                let opened = StandbyKeys::open_or_generate(
                    &sealing,
                    Box::new(sealer),
                    CONFIG,
                    &chain,
                    &domain,
                    record.candidate,
                );
                let StandbyStart::Standby { keys, generated } = opened.await.unwrap() else { panic!("promoted") };
                assert!(generated && keys.address() != promoted, "a new standby key replaces the promoted one");
            }
        }
    }
}

/// The measurement and config hash the routing test's keys are sealed for.
const LAUNCH: [u8; 48] = [0x5d; 48];
const CONFIG: [u8; 32] = [0xc5; 32];

/// Sequences a deposit and a filled order into a log at `dir`, returning the seq of the fill.
async fn sequence_a_trade(dir: &std::path::Path) -> u64 {
    let clock = std::sync::Arc::new(crate::clock::test_clock::TestClock::default());
    let genesis = crate::sequencer::genesis_commands(&crate::test_support::dev_config().genesis);
    let node = TestSequencer::start_legacy_at(dir, 64, clock.clone(), genesis);
    let ts = crate::clock::Clock::now_ms(clock.as_ref());
    let quotes = ["a", "b", "c"].map(|source| engine_core::price::SourceQuote {
        source: source.into(),
        mid: engine_core::Fixed::from_int(100_000),
        ts,
        contract: None,
    });
    node.submit(Command::Prices { market: 1, quotes: quotes.to_vec() }).await;
    node.submit(crate::test_support::deposit(1_000, "promoted-trade")).await;
    let order = engine_core::command::OrderRequest {
        account: crate::test_support::TRADER,
        client_id: 1,
        market: 1,
        side: engine_core::types::Side::Buy,
        qty: "0.01".parse().unwrap(),
        kind: engine_core::command::OrderKind::Market {
            quoted: engine_core::Fixed::from_int(100_000),
            max_slippage_bps: None,
        },
        reduce_only: false,
        auth: None,
    };
    let filled = node.submit(Command::PlaceOrder(order)).await;
    assert!(filled.iter().any(|event| matches!(event, engine_core::event::Event::Fill(_))), "{filled:?}");
    let seq = node.next_seq().await - 1;
    node.stop().await;
    seq
}

/// The promotion the quarantine test records.
const PROMOTION: Promotion = Promotion {
    turn: 20_400,
    position: 1,
    signer_epoch: 2,
    generation: 9,
    head_seq: 120,
    head_link: B256::repeat_byte(0x4c),
    checkpoint_seq: 100,
    checkpoint_root: B256::repeat_byte(0x7e),
    at: 0,
    block: 0,
    carried: 20_400,
};
