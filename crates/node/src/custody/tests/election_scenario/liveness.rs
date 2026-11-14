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
//! 3. holds: through a trusted-time halt the operator posts held heads on newer blocks, and nothing is
//!    promoted past the detection window;
//! 4. a paused chain: the operator stops by its own clock, nothing is promoted, and it resumes on its
//!    first read once blocks come again;
//! 5. a failed operator: with its poster stopped the members witness each slot, the operator fences
//!    before the earliest promotion could pass, and the promotion moves the vault's signer to the next
//!    member's standby key in a new signer epoch, which its seal then opens as that promotion's.

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
use super::schedule::LIVENESS;
use super::{CONFIG_HASH, DIGEST_A, DIGEST_B, FINALITY_MARGIN_SECS, T0_SECS, address, monkey};
use crate::checkpoints::scenario::deploy;
use crate::clock::Clock;
use crate::config::LivenessSection;
use crate::liveness::caller::{Caller, Endpoint};
use crate::liveness::chain::LivenessDomain;
use crate::liveness::link::Copy;
use crate::log_heads::Hold;
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
}

const DIGEST_C: [u8; 48] = [0xc1; 48];
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

#[tokio::test]
#[ignore = "requires Anvil and compiled contracts; scripts/dev-liveness.sh runs it"]
async fn dev_scenario_a_failed_operator_is_promoted_past_within_minutes() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
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
        &(chain.accounts[9], 1_606_824_023u64).abi_encode_params(),
    )
    .await;
    let (schedule, log_heads) = chain.deploy_schedule(receiver).await;
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
    let samples = sampling(clock.clone());
    let trusted = || TrustedClock::new(clock.clone(), samples.subscribe(), Duration::from_secs(10));
    let log = Log::default();
    let rig = Rig::start(&cfg, &signing, (domain, keys), trusted(), &log, &members, operator).await;
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
    for seq in 7..=20 {
        log.extend(&rig, &members, operator, seq);
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    assert_eq!(chain.view(chain.vault, signerEpochCall {}).await, epoch, "no promotion while members relay");
    assert!(chain.view(schedule, promotionDueAtCall {}).await > due_before, "relayed heads moved detection on");
    rig.unsilence(&chain).await;
    println!("PASS: with the operator's relayer silenced, members relay its heads and nothing is promoted");

    // 3. A trusted-time halt past the detection window: held heads on newer blocks, no promotion.
    rig.hold.send_replace(Hold::TimeHalt);
    let window = Duration::from_secs(LIVENESS.0 * (u64::from(LIVENESS.2) + 2));
    let held_until = Instant::now() + window;
    while Instant::now() < held_until {
        chain.rpc("evm_mine", json!([])).await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let held = chain.view(log_heads, headsCall { epoch }).await;
    assert!(held.boundBlock > 0, "the latest head is a held one: bound block {}", held.boundBlock);
    assert_eq!(chain.view(chain.vault, signerEpochCall {}).await, epoch, "no promotion through a hold");
    rig.hold.send_replace(Hold::Live);
    println!("PASS: through a trusted-time halt past the detection window, held heads land and nothing is promoted");

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

    // 5. The operator fails: its poster stops; members witness; it fences before the earliest
    //    promotion; a promotion moves the vault's signer to the next member's standby key.
    rig.poster.abort();
    let mut fenced_at = None;
    let promoted = crate::test_support::wait_for("a promotion", async {
        loop {
            if fenced_at.is_none() && !rig.gate.signs() {
                fenced_at = Some(chain.latest_timestamp().await);
            }
            let now = chain.view(chain.vault, signerEpochCall {}).await;
            if now > epoch {
                break now;
            }
            chain.rpc("evm_mine", json!([])).await;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
    .await;
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
    assert!(matches!(state, StandbyState::Promoted(promotion) if promotion.signer_epoch == promoted), "{state:?}");
    println!("PASS: the promoted member's standby seal opens as that promotion's");
    for caller in callers {
        caller.abort();
    }
    rig.stop();
}
