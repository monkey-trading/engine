//! The operator faults dev scenario, from `V70`, which `scripts/dev-operator-faults.sh` runs. On Anvil,
//! A, B and C register with standby keys the vault's signer vouches for and delegate 50, 30 and 20
//! thousand MONKEY, a close adopts them, and a turn is drawn for B. The node, the enclave holding the
//! vault's signing key, takes B's turn; its host stops it, keeps a copy of its log, lets it sequence on,
//! then restarts it from the copy, so it sequences a second history over the same seqs. The enclave's
//! key signs an `engine.log-head.v2` head of each history at one seq, binding B's assignment, and an
//! account with no role submits both to the real `FaultProof`. It checks:
//! 1. `FaultProven` names B, and the custody watcher mirrors it into the real sequencer;
//! 2. the next distribution cuts B's part of the operator's share by the haircut's average over its
//!    time in office, starting at the penalty, the withheld part going to stakers;
//! 3. the next epoch's cut is smaller, as the haircut falls linearly, and once it has decayed a later
//!    turn of B's is paid whole;
//! 4. after a promotion moves the vault's signer to a standby key in a new signer epoch, a head the new
//!    key signs paired with B's proves nothing under either epoch.

use std::path::Path;
use std::sync::Arc;

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolError, SolEvent, SolValue};
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::operator_faults::Penalty;
use engine_core::types::{AccountId, Millis};
use k256::ecdsa::SigningKey;
use serde_json::json;

use super::chain::{Chain, Parameters, delegateCall, setParametersCall, setRotationCall};
use super::node::{Node, POSTED_CHECKPOINT_EVERY};
use super::operator_share::{FLOOR_UNITS, next_epoch};
use super::{CONFIG_HASH, DIGEST_A, DIGEST_B, FINALITY_MARGIN_SECS, T0_SECS, address, monkey};
use crate::checkpoints::scenario::deploy;
use crate::clock::Clock;
use crate::config::NodeConfig;
use crate::liveness::Released;
use crate::liveness::operator::Signing;
use crate::log::CheckpointDomain;
use crate::log_heads::v2::{Assignment, Inputs, next};
use crate::log_heads::{Head, HeadV2, HeadsChain, HeadsRpc, Hold};
use crate::seed_relay::schedule::next_boundary;
use crate::tee::EnclaveKey;
use crate::tee::sealing::Release;

sol! {
    struct Turn { address candidate; uint16 position; uint8 reason; uint32 setId; uint64 generation; uint64 prior; bytes32 seed; address source; }
    struct ProvenHead {
        uint64 seq; bytes32 link; uint64 parentSeq; bytes32 parentLink; uint64 turn; uint16 position;
        bytes32 release; uint64 generation; uint64 stamp; uint64 signedAt; uint8 hold; address confirmer;
        uint64 blockNumber; bytes32 blockHash;
    }
    function turnOf(uint64 turn) external view returns (Turn);
    function setVault(address vault_) external;
    function setRegistry(address registry_) external;
    function setSchedule(address schedule_) external;
    function draw(uint64 turn) external returns (address);
    function memberAt(uint32 setId, bytes32 seed, uint64 turn, uint16 position) external view returns (address);
    function relayAlias() external view returns (address);
    function seedEpoch(uint64 turn) external view returns (uint64);
    function receiveSeed(uint64 turn, uint64 epoch, bytes32 seed) external;
    function signerEpoch() external view returns (uint64);
    function checkpointSigner() external view returns (address);
    function witness() external returns (bool);
    function promote() external returns (uint64, uint16);
    struct Evidence { ProvenHead head; bytes signature; uint256 revision; }
    function prove(uint64 epoch, Evidence a, Evidence b) external returns (address);
    event FaultProven(address indexed member, uint64 indexed turn, uint16 position, uint64 signerEpoch, uint64 seq);
    error InvalidSigner();
}

const DIGEST_C: [u8; 48] = [0xc1; 48];
/// The heartbeat rule the schedule is deployed with: 30 s slots, 20 s tolerance, 3 misses, 10 blocks.
const RULE: (u64, u64, u16, u16) = (30, 20, 3, 10);
const KEYS: [[u8; 32]; 3] = [[0x55; 32], [0x56; 32], [0x57; 32]];
const STAKES: [u64; 3] = [50_000, 30_000, 20_000];
const DAY_SECS: u64 = 86_400;
const DAY_MS: Millis = DAY_SECS as Millis * 1000;
const SET_CLOSE_SECS: u64 = T0_SECS + DAY_SECS + 3 * 3_600;
const SET_EFFECTIVE_SECS: u64 = SET_CLOSE_SECS + 7 * DAY_SECS + FINALITY_MARGIN_SECS;
/// A turn's boundary, after midnight UTC: 22:30.
const BOUNDARY_OFFSET_SECS: u64 = 81_000;
/// The scenario's penalty: half the share at the fault, falling to nothing over four days.
const PENALTY: Penalty = Penalty { initial_bps: 5_000, decay_ms: 4 * DAY_MS };
/// The house's capital above the floor each paying close distributes: 10,000 USDC.
const EXCESS_UNITS: u64 = 10_000_000_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Anvil and compiled contracts; scripts/dev-operator-faults.sh runs it"]
async fn dev_scenario_a_member_that_signs_two_histories_has_its_share_cut() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
    );
    let signing = Arc::new(EnclaveKey::generate());
    let deployer = SigningKey::from_slice(&[0x42; 32]).unwrap();
    let mut chain = Chain::start(Address::from(signing.address()), address(&deployer)).await;
    // `witness` reads Arbitrum's block number, which an `ArbSys` stand-in at 0x64 answers from Anvil's.
    let stand_in = deploy(&chain.provider, chain.accounts[0], "ChainArbSys.sol/ChainArbSys.json", &[]).await;
    let code: Bytes = chain.provider.request("eth_getCode", (stand_in, "latest")).await.unwrap();
    chain.rpc("anvil_setCode", json!(["0x0000000000000000000000000000000000000064", code])).await;
    let receiver = deploy(
        &chain.provider,
        chain.accounts[0],
        "SeedReceiver.sol/SeedReceiver.json",
        &(chain.accounts[9], 1_606_824_023u64).abi_encode_params(),
    )
    .await;
    let (schedule, log_heads) = chain.deploy_schedule_with(receiver, RULE, 60).await;
    chain.schedule = Some(schedule);
    let fault_proof = deploy(
        &chain.provider,
        chain.accounts[0],
        "FaultProof.sol/FaultProof.json",
        &(chain.vault,).abi_encode_params(),
    )
    .await;
    chain.fault_proof = Some((fault_proof, PENALTY));
    let mut node = Node::start_posting_checkpoints(&chain, signing.clone()).await;
    let recorded = node.query(|e| e.operator_faults().config).await.expect("the node records the contract");
    assert_eq!((recorded.contract.0, recorded.penalty), (fault_proof.0.0, PENALTY));
    println!("PASS: the node recorded the fault proof contract and its penalty at V70's activation");

    // A, B and C stake, register and delegate to themselves; the vault's signer vouches for a standby key
    // of each; the house holds the floor; a close adopts the three as the rotation set.
    let members = [chain.accounts[2], chain.accounts[3], chain.accounts[4]];
    let keys = KEYS.map(|key| SigningKey::from_slice(&key).unwrap());
    let digests = [DIGEST_A, DIGEST_B, DIGEST_C];
    let releases = digests.map(|digest| B256::from(Release { launch_digest: digest, config_sha256: CONFIG_HASH }.id()));
    for i in 0..3 {
        chain.stake(members[i], monkey(STAKES[i])).await;
        chain.register(members[i], &keys[i], monkey(2_000), digests[i]).await;
    }
    let daily = Parameters { epochLength: DAY_SECS, ..super::dev_parameters().0 };
    let mut calls = vec![
        (chain.registry, setParametersCall { parameters: daily }.abi_encode()),
        (chain.registry, setVaultCall { vault_: chain.vault }.abi_encode()),
        (chain.vault, setRegistryCall { registry_: chain.registry }.abi_encode()),
        (chain.registry, setScheduleCall { schedule_: schedule }.abi_encode()),
    ];
    calls.extend((0..3).map(|i| (chain.registry, chain.approval(members[i], digests[i]))));
    calls.push((chain.registry, setRotationCall { enabled: true }.abi_encode()));
    chain.govern_each(calls).await;
    let standbys: Vec<EnclaveKey> = (0..3).map(|_| EnclaveKey::generate()).collect();
    for i in 0..3 {
        let delegate = delegateCall { candidate: members[i], amount: monkey(STAKES[i]) };
        chain.send(members[i], chain.staking, delegate.abi_encode()).await;
        chain.register_standby(members[i], releases[i], Address::from(standbys[i].address()), &signing).await;
    }
    chain.fund(None, U256::from(FLOOR_UNITS)).await;
    node.wait("the house's capital to credit", |e| e.house_cash() >= engine_core::Fixed::from_int(1_000_000)).await;
    node.advance_to((SET_CLOSE_SECS as Millis + 60) * 1000).await;
    chain.close_epoch_at(SET_CLOSE_SECS).await;
    node.wait("the rotation set to mirror", |e| e.daily_turns().sets.contains_key(&1)).await;
    let b = AccountId(members[1].into());
    println!("PASS: a close adopted A, B and C at 50:30:20");

    // 1. A turn is drawn for B; the node, as B's enclave, takes it at its boundary.
    let turn = (SET_EFFECTIVE_SECS + DAY_SECS - BOUNDARY_OFFSET_SECS) / DAY_SECS + 1;
    let begins = draw_for(&chain, &mut node, (receiver, schedule), turn, members[1]).await;
    let taken = node.submit(Command::TurnTaken { turn, release: releases[1] }).await;
    assert!(matches!(taken.last(), Some(Event::TurnTaken { candidate, .. }) if *candidate == b), "{taken:?}");
    let taken_at = node.query(|e| e.now()).await;
    let assignment = chain.view(schedule, turnOfCall { turn }).await;
    println!("PASS: turn {turn} names B from {begins}, and B's enclave took it");

    // 2. The host stops the enclave and keeps a copy of its log; the enclave sequences on; the host
    //    restarts it from the copy, and it sequences a second history over the same seqs. The copy is
    //    taken just after a checkpoint, and neither history reaches the next before the swap, so the
    //    vault's latest checkpoint is in both.
    while node.query(|e| e.last_seq().expect("a log") + 1).await % POSTED_CHECKPOINT_EVERY != 0 {
        tick(&mut node, 1, 100).await;
    }
    let copy = node.dir.with_extension("copy");
    let first = node.dir.with_extension("first");
    let second = node.dir.with_extension("second");
    let mut node = node.restart_with(&chain, |dir| crate::test_support::copy_dir(dir, &copy)).await;
    tick(&mut node, 3, 1_000).await;
    let mut node = node
        .restart_with(&chain, |dir| {
            std::fs::rename(dir, &first).unwrap();
            std::fs::rename(&copy, dir).unwrap();
        })
        .await;
    tick(&mut node, 5, 700).await;
    let mut node = node.restart_with(&chain, |dir| crate::test_support::copy_dir(dir, &second)).await;
    let (history_a, history_b) =
        (crate::sequencer::replay_path(&first).unwrap(), crate::sequencer::replay_path(&second).unwrap());
    let seq = history_a.engine.last_seq().min(history_b.engine.last_seq()).expect("both logs hold commands");
    let ((link_a, stamp_a), (link_b, stamp_b)) = (head_at(&first, &history_a, seq), head_at(&second, &history_b, seq));
    assert_ne!(link_a, link_b, "the two histories differ at seq {seq}");
    println!("PASS: restarted from an older copy, the enclave sequenced two histories through seq {seq}");

    // 3. The enclave signs a head of each at that seq as its log-head poster does: the turn read from
    //    the chain through the custody quorum binds B's assignment, since its keys are sealed in that
    //    assignment's release and generation. Anyone proves the pair.
    let epoch = chain.view(chain.vault, signerEpochCall {}).await;
    let heads = heads_rpc(&chain, log_heads, &node.dir.with_extension("heads"));
    let keys = (releases[1], assignment.generation);
    let reached = node.clock.now_ms();
    let enclave = |link, stamp| Released { head: Head { seq, link }, stamp };
    let a = enclave_head(&heads, keys, enclave(link_a, stamp_a), reached, (&signing, chain.vault, epoch)).await;
    let other = enclave_head(&heads, keys, enclave(link_b, stamp_b), reached, (&signing, chain.vault, epoch)).await;
    assert_eq!((a.0.turn, a.0.position, a.0.release), (turn, assignment.position, releases[1]));
    let call = proveCall { epoch, a: evidence(&a, 0), b: evidence(&other, 0) };
    let receipt = chain.send(chain.accounts[7], fault_proof, call.abi_encode()).await;
    let proven = receipt.inner.logs().iter().find_map(|log| FaultProven::decode_log(&log.inner).ok());
    assert_eq!(proven.map(|log| (log.member, log.turn, log.seq)), Some((members[1], turn, seq)));
    let at = chain.latest_timestamp().await;
    node.advance_to((at as Millis + 60) * 1000).await;
    chain.mine_at(at + 60).await;
    node.wait("the proven fault to mirror", move |e| e.operator_faults().haircuts.contains_key(&b)).await;
    let since = node.query(move |e| e.operator_faults().haircuts[&b]).await;
    assert_eq!(node.query(move |e| e.haircut_bps(b, e.now())).await, PENALTY.initial_bps);
    println!("PASS: anyone proved the two heads; FaultProven names B and B's haircut starts at 50% at {since}");

    // 4. Each close cuts B's part by the haircut's average over its time in office then: about 46% at
    //    the first, about 40% at the next, the rest of the excess going to stakers.
    let turn_end = (begins + DAY_SECS) as Millis * 1000;
    let mut cuts = Vec::new();
    for _ in 0..2 {
        restock(&chain, &node).await;
        let from = node.query(|e| e.staking().and_then(|s| s.office.as_ref()).map(|o| o.since)).await.unwrap();
        let events = next_epoch(&mut node).await;
        let to = node.query(|e| e.now()).await.min(turn_end);
        let (kept, withheld) = part(&events, b);
        let cut = withheld * 10_000 / (kept + withheld);
        let expected = average_haircut(since, from.max(taken_at), to);
        assert!(cut.abs_diff(expected) <= 2, "cut {cut} bps against an average haircut of {expected} bps: {events:?}");
        // The stakers share the rest of the excess: only the pro-rata dust of three stakers is left.
        assert!(excess(&events) - allocated(&events) <= 3, "the withheld part goes to stakers: {events:?}");
        cuts.push(cut);
        println!(
            "PASS: the close cut B's part by {cut} bps ({withheld} of {}), the haircut's average",
            kept + withheld
        );
    }
    assert!(cuts[1] < cuts[0], "the cut falls with the haircut: {cuts:?}");

    // 5. Once the haircut has decayed, B's next turn is paid whole.
    let later = turn + 5;
    draw_for(&chain, &mut node, (receiver, schedule), later, members[1]).await;
    node.submit(Command::TurnTaken { turn: later, release: releases[1] }).await;
    let assignment = chain.view(schedule, turnOfCall { turn: later }).await;
    restock(&chain, &node).await;
    let events = next_epoch(&mut node).await;
    assert!(node.query(|e| e.operator_faults().haircuts.is_empty()).await, "the spent haircut is dropped");
    let (kept, withheld) = part(&events, b);
    assert!(kept > 0 && withheld == 0, "{events:?}");
    println!("PASS: four days on the haircut has decayed, and B's turn {later} is paid whole ({kept})");

    // 6. A promotion moves the vault's signer to a standby key in a new epoch; a head the new key signs,
    //    naming B's assignment of the later turn at a seq the old key signed, pairs with it under neither
    //    epoch.
    // B's enclave's head of the later turn, read while the turn names it.
    let released = Released { head: Head { seq, link: link_a }, stamp: stamp_a };
    let later_keys = (releases[1], assignment.generation);
    let a = enclave_head(&heads, later_keys, released, node.clock.now_ms(), (&signing, chain.vault, epoch)).await;
    let start = chain.latest_timestamp().await;
    let slot = RULE.0;
    for k in 1..=u64::from(RULE.2) {
        chain.mine_at((start / slot + k) * slot + 5).await;
        chain.send(chain.accounts[6], schedule, witnessCall {}.abi_encode()).await;
    }
    chain.mine_at((start / slot + u64::from(RULE.2) + 1) * slot + 5).await;
    chain.send(chain.accounts[6], schedule, promoteCall {}.abi_encode()).await;
    let promoted = chain.view(chain.vault, signerEpochCall {}).await;
    assert_eq!(promoted, epoch + 1);
    let installed = chain.view(chain.vault, checkpointSignerCall {}).await;
    let key = standbys.iter().find(|key| Address::from(key.address()) == installed).expect("a vouched standby key");
    // The same head over another history, signed by the promoted standby key under the new epoch.
    let mut discarded = a.0;
    discarded.head.link = keccak256(b"the promoted member's history");
    let discarded = (discarded, key.sign_digest(discarded.digest(31337, chain.vault, promoted)).parse().unwrap());
    for under in [epoch, promoted] {
        let call = proveCall { epoch: under, a: evidence(&a, 0), b: evidence(&discarded, 0) };
        let reverted = chain.reverted(chain.accounts[7], fault_proof, call.abi_encode()).await;
        assert_eq!(reverted.as_ref(), InvalidSigner {}.abi_encode().as_slice(), "under epoch {under}");
    }
    println!("PASS: across the promotion's signer change, epochs {epoch} and {promoted}, the pair proves nothing");
}

/// Relays a seed for `turn` whose first position picks `member`, draws it at its deadline, and moves
/// the node to a minute past its boundary, which it returns in seconds, once the turn applies.
async fn draw_for(
    chain: &Chain,
    node: &mut Node,
    (receiver, schedule): (Address, Address),
    turn: u64,
    member: Address,
) -> u64 {
    let begins = turn * DAY_SECS + BOUNDARY_OFFSET_SECS;
    let mut seed = B256::ZERO;
    for i in 0u64.. {
        seed = keccak256((turn, i).abi_encode());
        if chain.view(schedule, memberAtCall { setId: 1, seed, turn, position: 0 }).await == member {
            break;
        }
    }
    let alias = chain.view(receiver, relayAliasCall {}).await;
    let epoch = chain.view(receiver, seedEpochCall { turn }).await;
    chain.impersonate(alias).await;
    chain.send(alias, receiver, receiveSeedCall { turn, epoch, seed }.abi_encode()).await;
    node.advance_to(((begins - DAY_SECS + 60) * 1000) as Millis).await;
    chain.mine_at(begins - DAY_SECS + 60).await;
    chain.send(chain.accounts[1], schedule, drawCall { turn }.abi_encode()).await;
    node.wait("the drawn turn to mirror", move |e| e.daily_turns().assigned.contains_key(&turn)).await;
    node.advance_to(((begins + 60) * 1000) as Millis).await;
    chain.mine_at(begins + 60).await;
    node.submit(Command::Tick).await;
    node.wait("the turn to apply", move |e| e.daily_turns().in_effect.is_some_and(|t| t.turn == turn)).await;
    begins
}

/// Sequences `count` ticks `apart` milliseconds apart.
async fn tick(node: &mut Node, count: usize, apart: Millis) {
    for _ in 0..count {
        let next = node.clock.now_ms() + apart;
        node.advance_to(next).await;
        node.submit(Command::Tick).await;
    }
}

/// Deposits `EXCESS_UNITS` to the house and waits until the engine has it to distribute.
async fn restock(chain: &Chain, node: &Node) {
    let before = node.query(|e| e.distributable_excess().unwrap_or_default()).await;
    chain.fund(None, U256::from(EXCESS_UNITS)).await;
    let enough = before + engine_core::Fixed::from_int(10_000);
    node.wait("the restock to credit and finalise", move |e| e.distributable_excess().is_some_and(|x| x >= enough))
        .await;
}

/// The cumulative command-prefix link `replayed`, the log in `dir`, holds after `seq`, and that
/// command's stamp.
fn head_at(dir: &Path, replayed: &crate::sequencer::Replayed, seq: u64) -> (B256, u64) {
    let link = replayed.prefix_ring.prefix_at(seq).expect("the log holds the seq");
    let stamp = crate::test_support::log_entries(dir)
        .into_iter()
        .find_map(|entry| match entry {
            crate::log::LogEntry::Command(command) if command.sequenced.seq == seq => Some(command.sequenced.ts),
            _ => None,
        })
        .expect("the log holds the seq");
    (link, u64::try_from(stamp).unwrap())
}

/// The custody quorum's reads of the turn naming the operator, as the log-head poster makes them.
fn heads_rpc(chain: &Chain, log_heads: Address, dir: &Path) -> HeadsRpc {
    std::fs::create_dir_all(dir).unwrap();
    let anvil = std::fs::read_to_string(crate::test_support::ANVIL_CONFIG).unwrap();
    let mut cfg: NodeConfig = toml::from_str(&format!(
        "{anvil}\n[checkpoints]\nrpc_url = \"{}\"\nrelayer = \"{:#x}\"\nretry_interval = \"1s\"\nrpc_timeout = \"5s\"\n\n\
         [log_heads]\ncontract = \"{log_heads:#x}\"\nrelayer = \"{:#x}\"\ninterval = \"1h\"\n",
        chain.url, chain.accounts[8], chain.accounts[5]
    ))
    .unwrap();
    cfg.custody = super::node::custody_section(chain, dir, cfg.custody);
    cfg.custody.vault = format!("{:#x}", chain.vault);
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: AccountId(chain.vault.into()) };
    let quorum = Arc::new(crate::custody::heads_reader(&cfg.custody).unwrap());
    HeadsRpc::new(cfg.checkpoints.as_ref().unwrap(), cfg.log_heads.as_ref().unwrap(), domain, dir, quorum).unwrap()
}

/// The v2 head the enclave signs for `released`, built and signed as its log-head poster builds and
/// signs one: the turn naming the operator read from the chain, the assignment bound when `keys`, the
/// release and generation the enclave's keys are sealed in, are that turn's, the head from both, and
/// its digest under `epoch` of `vault` signed by `key`.
async fn enclave_head(
    heads: &HeadsRpc,
    keys: (B256, u64),
    released: Released,
    reached: Millis,
    (key, vault, epoch): (&EnclaveKey, Address, u64),
) -> (HeadV2, Bytes) {
    // As the poster does: the signer epoch's posted head, the parent a head extends (none here),
    // then the turn read at the block it was agreed at.
    let (posted_epoch, parent) = heads.posted().await.unwrap();
    assert_eq!(posted_epoch, epoch);
    let turn = heads.turn().await.unwrap().expect("a turn names the operator");
    let bound = Assignment::held(&turn, Some(keys));
    // The permission is good until the next turn boundary after the read's time, as the liveness rule
    // gives it, `reached` standing for the read's time.
    let until = next_boundary(u64::try_from(reached).unwrap() / 1000).saturating_mul(1000);
    let inputs = Inputs {
        signing: Signing::Tip(Some((epoch, turn.set)), until),
        tip: released,
        hold: Hold::Live,
        parent,
        reached: Some(reached),
        last_signed: 0,
        epoch,
    };
    let head = next(&turn, bound, Some(keys), inputs).expect("a head to sign");
    let signature = key.sign_digest(head.digest(31337, vault, epoch)).parse().unwrap();
    (head, signature)
}

/// A signed head as `prove` takes it, naming the assignment's revision at `revision`.
fn evidence((head, signature): &(HeadV2, Bytes), revision: u64) -> Evidence {
    Evidence { head: abi(head), signature: signature.clone(), revision: U256::from(revision) }
}

/// `head` as `prove` takes it.
fn abi(head: &HeadV2) -> ProvenHead {
    let (parent_seq, parent_link) = head.parent.map_or((0, B256::ZERO), |parent| (parent.seq, parent.link));
    ProvenHead {
        seq: head.head.seq,
        link: head.head.link,
        parentSeq: parent_seq,
        parentLink: parent_link,
        turn: head.turn,
        position: head.position,
        release: head.release,
        generation: head.generation,
        stamp: head.stamp,
        signedAt: head.signed_at,
        hold: head.hold.code(),
        confirmer: head.confirmer,
        blockNumber: head.block_number,
        blockHash: head.block_hash,
    }
}

/// `member`'s part of the operator's share among a close's events, and what its haircut withheld.
fn part(events: &[Event], member: AccountId) -> (u128, u128) {
    let find = |withheld: bool| {
        events
            .iter()
            .find_map(|event| match event {
                Event::OperatorShareAllocated { account, amount, .. } if !withheld && *account == member => {
                    Some(amount.0)
                }
                Event::OperatorShareWithheld { account, amount, .. } if withheld && *account == member => {
                    Some(amount.0)
                }
                _ => None,
            })
            .unwrap_or(0)
    };
    (find(false), find(true))
}

/// The haircut's average over `[from, to]`, in bps, for a fault mirrored at `since`.
fn average_haircut(since: Millis, from: Millis, to: Millis) -> u128 {
    let end = since + PENALTY.decay_ms;
    let (a, b) = (from.max(since).min(end), to.min(end));
    let square = |left: Millis| (left as i128) * (left as i128);
    let integral = PENALTY.initial_bps as i128 * (square(end - a) - square(end - b)) / (2 * PENALTY.decay_ms as i128);
    (integral / (to - from) as i128) as u128
}

/// What a close allocated, and the excess it measured.
fn allocated(events: &[Event]) -> u128 {
    events
        .iter()
        .find_map(|event| match event {
            Event::DistributionEpochClosed { allocated, .. } => Some(allocated.0),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no epoch closed: {events:?}"))
}

fn excess(events: &[Event]) -> u128 {
    events
        .iter()
        .find_map(|event| match event {
            Event::DistributionEpochClosed { excess, .. } => Some(excess.0),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no epoch closed: {events:?}"))
}
