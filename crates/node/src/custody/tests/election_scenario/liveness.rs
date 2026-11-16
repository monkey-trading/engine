//! The minute-scale liveness dev scenario (#595), which `scripts/dev-liveness.sh` runs. On Anvil, with
//! the chain's blocks as Arbitrum's (`ChainArbSys` etched at 0x64) and the enclaves' trusted time on
//! the chain's clock, A, B and C register, each with a standby key the vault's signer vouches for, a
//! close adopts them, and turn 1 is drawn; its member, the operator, holds the vault's signing key.
//! The operator runs the real head poster, posting `engine.log-head.v2` heads at the seq its rule lets
//! it sign, and the real liveness rule, asking the other members' real member links for their
//! copies; the other members run the real liveness caller from their own accounts. It checks:
//! 1. heartbeats: v2 heads land at the seqs the members confirmed, the first latches the turn, and the
//!    operator's gate opens only once a member answered;
//! 2. relays: with the operator's relayer silenced, the members relay its signed heads from the stream
//!    and nothing is promoted;
//! 3. holds: through a benchmark-sync hold, and then a trusted-time halt in which no enclave gets a
//!    sample past `NO_SAMPLE_GRACE` so the sequencer's own time gate holds it, each past the detection
//!    window, the operator posts held heads on newer blocks and nothing is promoted; once samples
//!    come again the halt ends;
//! 4. a paused chain: the operator stops by its own clock, nothing is promoted, and it resumes on its
//!    first read once blocks come again;
//! 5. blocked reads: with the operator's chain reads blocked, its heads stop, the members witness each
//!    slot, the operator fences by its own clock before the earliest promotion could pass, and the
//!    promotion moves the vault's signer to the next member's standby key in a new signer epoch, which
//!    its seal then opens as that promotion's;
//! 6. a reorg: the promotion reverted away reads as unsettled while no block has passed its time, and
//!    as gone once one has, as a promoted member's restart judges it.

mod fallback;
mod promoted;
mod rig;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use engine_core::types::{AccountId, Millis};
use serde_json::json;
use tokio::task::JoinHandle;

use super::chain::{Chain, Parameters, delegateCall, setParametersCall, setRotationCall};
use super::{CONFIG_HASH, DIGEST_A, DIGEST_B, FINALITY_MARGIN_SECS, T0_SECS, address, monkey};
use crate::checkpoints::scenario::deploy;
use crate::clock::Clock;
use crate::config::LivenessSection;
use crate::liveness::caller::{Caller, Endpoint};
use crate::liveness::chain::LivenessDomain;
use crate::liveness::link::Copy;
use crate::log_heads::Hold;
use crate::node::member::promoted::{Record, Standing};
use crate::standby_key::StandbyKeys;
use crate::standby_key::chain::{StandbyDomain, StandbyState, standby_state};
use crate::tee::EnclaveKey;
use crate::test_support::scratch_dir;
use crate::trusted_time::TrustedClock;
use rig::{Log, Rig, config, etch_arb_sys, sampling, warp};

sol! {
    struct Turn { address candidate; uint16 position; uint8 reason; uint32 setId; uint64 generation; uint64 prior; bytes32 seed; address source; }
    function turnOf(uint64 turn) external view returns (Turn);
    function setVault(address vault_) external;
    function setRegistry(address registry_) external;
    function setSchedule(address schedule_) external;
    function draw(uint64 turn) external returns (address);
    function heads(uint64 epoch) external view returns (uint64 seq, bytes32 link, uint64 signedAt, uint64 boundBlock, uint64 postedAt);
    function latches(uint64 turn, uint16 position) external view returns (uint64 at, uint64 signerEpoch, address member, bytes32 release, uint64 generation);
    function signerEpoch() external view returns (uint64);
    function checkpointSigner() external view returns (address);
    function promotionDueAt() external view returns (uint64);
    function relayAlias() external view returns (address);
    function seedEpoch(uint64 turn) external view returns (uint64);
    function receiveSeed(uint64 turn, uint64 epoch, bytes32 seed) external;
}

const DIGEST_C: [u8; 48] = [0xc1; 48];
/// The heartbeat rule the scenario deploys, short enough to run in minutes: 30 s slots, 20 s tolerance,
/// 3 misses, 10 blocks, under a 60 s switch budget.
const RULE: (u64, u64, u16, u16) = (30, 20, 3, 10);
const KEYS: [[u8; 32]; 3] = [[0x42; 32], [0x55; 32], [0x56; 32]];
const STAKES: [u64; 3] = [50_000, 30_000, 20_000];
const DAY_SECS: u64 = 86_400;
const SET_CLOSE_SECS: u64 = T0_SECS + DAY_SECS + 3 * 3_600;
const SET_EFFECTIVE_SECS: u64 = SET_CLOSE_SECS + 7 * DAY_SECS + FINALITY_MARGIN_SECS;
/// A turn's boundary, after midnight UTC: 22:30.
const BOUNDARY_OFFSET_SECS: u64 = 81_000;

/// The enclaves' clock: the wall clock moved by the chain's warps, so trusted time and block time
/// share one baseline.
struct Shifted(AtomicI64);

impl Clock for Shifted {
    fn now_ms(&self) -> Millis {
        crate::clock::SystemClock.now_ms() + self.0.load(Ordering::SeqCst)
    }

    fn instant(&self) -> Instant {
        Instant::now()
    }
}

/// A member: its candidate, admin key, release, member link endpoint, and standby keys.
struct Member {
    candidate: Address,
    admin: k256::ecdsa::SigningKey,
    digest: [u8; 48],
    endpoint: String,
    standby: Arc<StandbyKeys>,
    copy: Copy,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Anvil and compiled contracts; scripts/dev-liveness.sh runs it"]
async fn dev_scenario_a_failed_operator_is_promoted_past_within_minutes() {
    // Every worker thread logs: the parts run on the runtime's threads.
    let _ = tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .with_env_filter(tracing_subscriber::EnvFilter::new("info,engine_node::liveness=debug"))
            .finish(),
    );
    let signing = Arc::new(EnclaveKey::generate());
    let deployer = k256::ecdsa::SigningKey::from_slice(&[0x41; 32]).unwrap();
    let mut chain = Chain::start(Address::from(signing.address()), address(&deployer)).await;
    etch_arb_sys(&chain).await;
    let clock = Arc::new(Shifted(AtomicI64::new(0)));
    let receiver = deploy(
        &chain.provider,
        chain.accounts[0],
        "SeedReceiver.sol/SeedReceiver.json",
        &(chain.accounts[9], 1_606_824_023u64, crate::seed_relay::schedule::TURN_SECS).abi_encode_params(),
    )
    .await;
    let (schedule, log_heads) = chain.deploy_schedule_with(receiver, RULE, 60).await;
    chain.schedule = Some(schedule);
    let members: Vec<Member> = [(chain.accounts[2], 0), (chain.accounts[3], 1), (chain.accounts[4], 2)]
        .into_iter()
        .map(|(candidate, i)| {
            let dir = scratch_dir(&format!("liveness-member-{i}"));
            let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
            Member {
                candidate,
                admin: k256::ecdsa::SigningKey::from_slice(&KEYS[i]).unwrap(),
                digest: [DIGEST_A, DIGEST_B, DIGEST_C][i],
                endpoint: free.to_string(),
                standby: Arc::new(StandbyKeys::for_test(&dir)),
                copy: Copy::default(),
            }
        })
        .collect();

    // A, B and C stake, register with their member endpoints and register standby keys the vault's
    // signer vouches for; rotation turns on; a close adopts them and the set takes effect.
    for (member, stake) in members.iter().zip(STAKES) {
        chain.stake(member.candidate, monkey(stake)).await;
        chain
            .register_serving_at(
                member.candidate,
                &member.admin,
                monkey(2_000),
                member.digest,
                &member.endpoint,
                "127.0.0.1:1",
            )
            .await;
    }
    let daily = Parameters { epochLength: DAY_SECS, ..super::dev_parameters().0 };
    let mut calls = vec![
        (chain.registry, setParametersCall { parameters: daily }.abi_encode()),
        (chain.registry, setVaultCall { vault_: chain.vault }.abi_encode()),
        (chain.vault, setRegistryCall { registry_: chain.registry }.abi_encode()),
        (chain.registry, setScheduleCall { schedule_: schedule }.abi_encode()),
    ];
    calls.extend(members.iter().map(|member| (chain.registry, chain.approval(member.candidate, member.digest))));
    calls.push((chain.registry, setRotationCall { enabled: true }.abi_encode()));
    chain.govern_each(calls).await;
    for (member, stake) in members.iter().zip(STAKES) {
        chain
            .send(
                member.candidate,
                chain.staking,
                delegateCall { candidate: member.candidate, amount: monkey(stake) }.abi_encode(),
            )
            .await;
        let release =
            B256::from(crate::tee::sealing::Release { launch_digest: member.digest, config_sha256: CONFIG_HASH }.id());
        chain.register_standby(member.candidate, release, Address::from(member.standby.address().0), &signing).await;
    }
    chain.close_epoch_at(SET_CLOSE_SECS).await;
    // The first turn whose draw deadline finds the set in effect, drawn at its deadline.
    let turn = (SET_EFFECTIVE_SECS + DAY_SECS - BOUNDARY_OFFSET_SECS) / DAY_SECS + 1;
    let boundary = turn * DAY_SECS + BOUNDARY_OFFSET_SECS;
    // The turn's seed arrives from the relay's alias before its draw deadline, as a relay would send it.
    let alias = chain.view(receiver, relayAliasCall {}).await;
    let seed_epoch = chain.view(receiver, seedEpochCall { turn }).await;
    chain.impersonate(alias).await;
    chain
        .send(alias, receiver, receiveSeedCall { turn, epoch: seed_epoch, seed: B256::repeat_byte(0x5e) }.abi_encode())
        .await;
    chain.mine_at(boundary - DAY_SECS + 60).await;
    chain.send(chain.accounts[1], schedule, drawCall { turn }.abi_encode()).await;
    warp(&chain, &clock, boundary + 1).await;
    let drawn = chain.view(schedule, turnOfCall { turn }).await;
    let named = drawn.candidate;
    let operator = members.iter().position(|member| member.candidate == named).expect("a member is drawn");
    let release = crate::tee::sealing::Release { launch_digest: members[operator].digest, config_sha256: CONFIG_HASH };
    let keys = (B256::from(release.id()), drawn.generation);
    println!("PASS: A, B and C hold vouched standby keys; turn {turn} names member {operator}");

    // The operator's node and the members' callers.
    let dir = scratch_dir("liveness-operator");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = config(&chain, &dir, log_heads, chain.accounts[5]);
    let domain = LivenessDomain {
        standby: StandbyDomain {
            chain_id: cfg.custody.chain_id,
            vault: chain.vault,
            registry: chain.registry,
            schedule,
        },
        log_heads,
    };
    let (samples, halting) = sampling(clock.clone());
    let trusted = || TrustedClock::new(clock.clone(), samples.subscribe(), Duration::from_secs(10));
    let log = Log::default();
    // The operator reads through proxies of its own, which leg 5 stops.
    let (operator_proxies, operator_urls) = super::chain::proxies(&chain.url).await;
    let mut operator_cfg = config(&chain, &dir, log_heads, chain.accounts[5]);
    operator_cfg.custody.evm.as_mut().unwrap().providers = operator_urls
        .iter()
        .enumerate()
        .map(|(index, url)| toml::from_str(&format!("name = 'operator-{index}'\nrpc_url = '{url}'")).unwrap())
        .collect();
    let time = (samples.subscribe(), clock.clone() as Arc<dyn Clock>);
    let rig = Rig::start(&operator_cfg, &signing, (domain, keys), trusted(), time, &log, &members, operator).await;
    let callers: Vec<JoinHandle<anyhow::Result<()>>> = members
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != operator)
        .map(|(i, member)| {
            let caller = Caller {
                chain: crate::custody::StandbyReader::connect_named(&cfg.custody, log_heads, "caller").unwrap(),
                sending: Endpoint::new(
                    cfg.checkpoints.as_ref().unwrap(),
                    &LivenessSection { caller: chain.accounts[6 + i], min_balance: U256::from(1) },
                    cfg.custody.chain_id,
                )
                .unwrap(),
                domain,
                copy: member.copy.clone(),
                min_balance: U256::from(1),
                poll: crate::liveness::caller::POLL,
                clock: clock.clone(),
            };
            tokio::spawn(caller.run())
        })
        .collect();

    // 1. Heartbeats at the confirmed seq, the turn latched, the gate open once a member answered.
    for seq in 1..=6 {
        log.extend(&rig, &members, operator, seq);
        tokio::time::sleep(Duration::from_secs(6)).await;
    }
    let epoch = chain.view(chain.vault, signerEpochCall {}).await;
    let posted = chain.view(log_heads, headsCall { epoch }).await;
    assert!(posted.seq >= 4 && posted.signedAt > 0, "v2 heads land: seq {}, signed at {}", posted.seq, posted.signedAt);
    let latch = chain.view(log_heads, latchesCall { turn, position: drawn.position }).await;
    assert_eq!(latch.member, named, "the first head latched the turn's assignment");
    assert!(rig.gate.sequences(), "a member answered and confirms the log");
    println!("PASS: v2 heads land at the confirmed seqs every interval, and the first latched turn {turn}");

    // 2. The operator's relayer is silenced: the members relay its signed heads, nothing is promoted.
    rig.silence(&chain).await;
    let due_before = chain.view(schedule, promotionDueAtCall {}).await;
    for seq in 7..=22 {
        log.extend(&rig, &members, operator, seq);
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    assert_eq!(chain.view(chain.vault, signerEpochCall {}).await, epoch, "no promotion while members relay");
    assert!(chain.view(schedule, promotionDueAtCall {}).await > due_before, "relayed heads moved detection on");
    rig.unsilence(&chain).await;
    println!("PASS: with the operator's relayer silenced, members relay its heads and nothing is promoted");

    // 3. A benchmark-sync hold, then a trusted-time halt, each past the detection window: held heads on
    //    newer blocks, no promotion. The halt is real: no enclave gets a sample, and the sequencer's
    //    time gate holds once the last is older than the grace.
    let window = Duration::from_secs(RULE.0 * (u64::from(RULE.2) + 2));
    for hold in [Hold::BenchmarkSync, Hold::TimeHalt] {
        let (halt, grace) = (hold == Hold::TimeHalt, crate::trusted_time::NO_SAMPLE_GRACE);
        match halt {
            true => halting.store(true, Ordering::SeqCst),
            false => drop(rig.benchmark.send_replace(true)),
        }
        let bound_before = chain.view(log_heads, headsCall { epoch }).await.boundBlock;
        let held_until = Instant::now() + window + if halt { grace } else { Duration::ZERO };
        let (mut bound, mut held) = (0, false);
        while Instant::now() < held_until {
            chain.rpc("evm_mine", json!([])).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
            held |= *rig.hold.borrow() == hold;
            bound = bound.max(chain.view(log_heads, headsCall { epoch }).await.boundBlock);
        }
        assert!(held, "{hold:?}: the sequencer held");
        assert!(bound > bound_before, "{hold:?}: held heads landed on newer blocks");
        assert_eq!(chain.view(chain.vault, signerEpochCall {}).await, epoch, "{hold:?}: no promotion through a hold");
        match halt {
            true => halting.store(false, Ordering::SeqCst),
            false => drop(rig.benchmark.send_replace(false)),
        }
        crate::test_support::wait_for("the hold to end", async {
            while *rig.hold.borrow() != Hold::Live {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        println!("PASS: through a {hold:?} hold past the detection window, held heads land and nothing is promoted");
    }

    // 4. The chain pauses past the window: the operator stops by its own clock; nothing is promoted;
    //    the first read after the pause reopens it.
    chain.rpc("evm_setAutomine", json!([false])).await;
    let paused_until = Instant::now() + window;
    let mut stopped = false;
    while Instant::now() < paused_until {
        stopped |= !rig.gate.signs();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(stopped, "the operator stopped while its reads could not rule out a promotion");
    chain.rpc("evm_setAutomine", json!([true])).await;
    chain.rpc("evm_mine", json!([])).await;
    crate::test_support::wait_for("the operator to resume after the pause", async {
        while !rig.gate.signs() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await;
    assert_eq!(chain.view(chain.vault, signerEpochCall {}).await, epoch, "no promotion across a pause");
    println!(
        "PASS: through a paused chain the operator stopped by its own clock, and resumed on its first read after it"
    );

    // 5. The operator's chain reads are blocked: its heads stop; members witness; it fences by its own
    //    clock before the earliest promotion; a promotion moves the vault's signer to the next member's
    //    standby key. A snapshot first, for leg 6's reorg.
    let before_promotion = chain.rpc_result("evm_snapshot", json!([])).await;
    drop(operator_proxies);
    let mut fenced_at = None;
    let promoted = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            // The epoch first: a fence first seen once the promotion is in counts as no fence before it.
            let now = chain.view(chain.vault, signerEpochCall {}).await;
            if now > epoch {
                break now;
            }
            if fenced_at.is_none() && !rig.gate.signs() {
                fenced_at = Some(chain.latest_timestamp().await);
            }
            chain.rpc("evm_mine", json!([])).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
    .await
    .expect("a promotion within five minutes of the operator's failure");
    let signer = chain.view(chain.vault, checkpointSignerCall {}).await;
    let next =
        members.iter().position(|member| Address::from(member.standby.address().0) == signer).expect("a standby key");
    assert_ne!(next, operator);
    let fenced_at = fenced_at.expect("the operator fenced before the promotion");
    println!(
        "PASS: the failed operator fenced at block time {fenced_at}; the promotion moved the vault's signer to member {next}'s standby key in epoch {promoted}"
    );
    let reader = crate::custody::StandbyReader::connect(&cfg.custody, log_heads).unwrap();
    let state = standby_state(
        &reader,
        &domain.standby,
        AccountId(members[next].candidate.0.0),
        members[next].standby.address(),
    )
    .await
    .unwrap();
    let StandbyState::Promoted(promotion) = state else { panic!("the promoted key opens as {state:?}") };
    assert_eq!(promotion.signer_epoch, promoted);
    assert!(
        fenced_at < promotion.at,
        "the operator fenced at block time {fenced_at}, not before the promotion's block at {}",
        promotion.at
    );
    println!("PASS: the promoted member's standby seal opens as that promotion's");

    // 6. A reorg removes the promotion: a latest block below the one it was read at says nothing of it,
    // and once the replacing chain passes that block, a final block there lacking it settles it gone.
    for caller in callers {
        caller.abort();
    }
    rig.stop();
    let candidate = AccountId(members[next].candidate.0.0);
    let record = Record::new(candidate, members[next].standby.address(), promotion, 1);
    let own = crate::tee::sealing::Release { launch_digest: members[next].digest, config_sha256: CONFIG_HASH };
    let own = B256::from(own.id());
    assert_eq!(record.standing(&reader, &domain.standby, own).await.unwrap(), Standing::Stands);
    chain.rpc("evm_revert", json!([before_promotion])).await;
    assert_eq!(chain.view(chain.vault, signerEpochCall {}).await, epoch, "the reorg took the promotion");
    assert_eq!(record.standing(&reader, &domain.standby, own).await.unwrap(), Standing::Behind);
    let mut standing = Standing::Behind;
    for _ in 0..256 {
        chain.rpc("evm_mine", json!([])).await;
        standing = record.standing(&reader, &domain.standby, own).await.unwrap();
        if standing == Standing::Gone {
            break;
        }
        assert!(matches!(standing, Standing::Behind | Standing::Unsettled), "{standing:?} before it settled");
    }
    assert_eq!(standing, Standing::Gone, "the replacing chain passed the promotion's block without it");
    println!("PASS: a promotion a reorg removed says nothing below its block, then reads gone");
}
