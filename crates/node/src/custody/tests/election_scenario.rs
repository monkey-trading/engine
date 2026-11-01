//! Operator election dev scenario on a local Anvil: stakers delegate to registered candidates in the
//! real staking contract, the real operator registry elects, the custody watcher mirrors the
//! registry's finalised events through the staking pipeline into the real sequencer, and the
//! elected key signs admin commands from the election's effective time. MONKEY is minted by the
//! vault, its only minter, impersonated in place of proven emission claims.

mod chain;
mod node;

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::sol_types::{SolCall, SolError};
use axum::http::StatusCode;
use engine_core::command::AdminCommand;
use engine_core::emissions::TokenUnits;
use engine_core::types::{AccountId, Millis};
use k256::ecdsa::SigningKey;
use serde_json::json;

use crate::sequencer::replay_path;
use crate::tee::EnclaveKey;

use chain::{Chain, Parameters, SelfBondLocked, delegateCall, operatorInEffectCall, setRotationCall, startUnstakeCall};
use node::Node;

/// The chain's genesis time and the node's first sequenced time: Tue 20 Oct 2026 14:00 UTC.
const T0_SECS: u64 = 1_792_504_800;
/// The staking contract's distribution epoch and unstake cooldown, which the genesis records too.
const DISTRIBUTION_EPOCH_SECS: u64 = 86_400;
const COOLDOWN_SECS: u64 = 3 * 86_400;
/// The timelock's delay: a development value.
const GOVERNANCE_DELAY_SECS: u64 = 3_600;
/// The registry's finality margin: a development value, which #360 sets for production.
const FINALITY_MARGIN_SECS: u64 = 600;
/// The deployer's release and the candidates', each a 48-byte launch measurement with one config.
const DEPLOYER_DIGEST: [u8; 48] = [0xd0; 48];
const DIGEST_A: [u8; 48] = [0xa1; 48];
const DIGEST_B: [u8; 48] = [0xb1; 48];
const CONFIG_HASH: [u8; 32] = [0xc0; 32];
/// The block times of the close while rotation is off and of the close that elects.
const OFF_CLOSE_SECS: u64 = T0_SECS + 2 * 3_600;
const ELECTION_CLOSE_SECS: u64 = T0_SECS + 5 * 3_600;
/// When the election takes effect: its close, the seven-day notice, and the finality margin.
const EFFECTIVE_SECS: u64 = ELECTION_CLOSE_SECS + 7 * 86_400 + FINALITY_MARGIN_SECS;
const EFFECTIVE_MS: Millis = EFFECTIVE_SECS as Millis * 1000;

#[tokio::test]
#[ignore = "requires Anvil, Python 3, and compiled contracts; scripts/dev-operator-election.sh runs it"]
async fn dev_scenario_stakers_elect_the_key_that_signs_from_the_effective_time() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
    );
    // The dev operator key that `config/dev.toml` names: the deployer, in effect at first.
    let deployer = SigningKey::from_slice(&[0x42; 32]).unwrap();
    let (key_a, key_b) = (SigningKey::from_slice(&[0x55; 32]).unwrap(), SigningKey::from_slice(&[0x56; 32]).unwrap());
    let enclave = Arc::new(EnclaveKey::generate());
    let mut chain = Chain::start(Address::from(enclave.address()), address(&deployer)).await;
    let mut node = Node::start(&chain, enclave).await;
    let [a, b, c] = [chain.accounts[2], chain.accounts[3], chain.accounts[4]];

    // 1–2. Three accounts stake, two register as candidates with self-bonds, the timelock approves
    //      their releases, and all delegate: b leads with its own stake and c's.
    for (staker, amount) in [(a, 30_000), (b, 40_000), (c, 20_000)] {
        chain.stake(staker, monkey(amount)).await;
    }
    chain.register(a, &key_a, monkey(2_000), DIGEST_A).await;
    chain.register(b, &key_b, monkey(2_000), DIGEST_B).await;
    let approvals = vec![chain.approval(a, DIGEST_A), chain.approval(b, DIGEST_B)];
    chain.govern(approvals).await;
    for (staker, candidate) in [(a, a), (b, b), (c, b)] {
        chain.send(staker, chain.staking, delegateCall { candidate }.abi_encode()).await;
    }
    let staked = TokenUnits(monkey(20_000).to());
    node.wait("the stakes to mirror", move |e| e.staked(AccountId(c.into())) == staked).await;

    // 3. An epoch closes while rotation is off: it elects nothing, and the engine mirrors nothing.
    let block = chain.close_epoch_at(OFF_CLOSE_SECS).await.block_number.unwrap();
    node.wait("the staking mirror to pass the close", move |e| completed_through(e) >= Some(block)).await;
    assert!(node.query(|e| e.operator_election().is_default()).await);

    // 4. The timelock turns rotation on, and the close of a later epoch elects b; the engine logs it
    //    to take effect seven days and the finality margin after the close.
    chain.govern(vec![setRotationCall { enabled: true }.abi_encode()]).await;
    node.wait("rotation to mirror on", |e| e.operator_election().rotation_enabled).await;
    node.advance_to((ELECTION_CLOSE_SECS as Millis + 60) * 1000, || (ELECTION_CLOSE_SECS as Millis + 60) * 1000).await;
    chain.close_epoch_at(ELECTION_CLOSE_SECS).await;
    node.wait("the election to mirror", |e| e.operator_election().last.is_some()).await;
    let election = node.query(|e| e.operator_election().last).await.unwrap();
    assert_eq!(
        (election.candidate, election.admin_key, election.effective_at),
        (AccountId(b.into()), AccountId(address(&key_b).into()), EFFECTIVE_MS)
    );
    let (deployer_id, b_key) = (format!("{:#x}", address(&deployer)), format!("{:#x}", address(&key_b)));
    let (status, view) = node.get("/v1/operator").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["operator"], json!(deployer_id));
    assert_eq!(view["pending_rotation"], json!({ "address": b_key, "effective_at": EFFECTIVE_MS }));
    assert_eq!(
        (view["rotation_enabled"].clone(), view["last_election"]["candidate"].clone()),
        (json!(true), json!(format!("{b:#x}")))
    );

    // 5. Before the effective time the deployer's commands apply. From it, once the custody
    //    watcher has mirrored a finalised block stamped at or after it, they are refused and the
    //    elected key's apply. No block follows that one before the deployer's last request, so the
    //    request reaches the log rather than being refused at the route.
    node.advance_to(EFFECTIVE_MS - 60_000, || EFFECTIVE_MS - 60_000).await;
    let block = chain.mine_at(EFFECTIVE_SECS - 60).await;
    node.wait("the mirror to settle before the effective time", move |e| completed_through(e) >= Some(block)).await;
    let reduce_only = |reduce_only| AdminCommand::SetReduceOnly { market: None, reduce_only };
    assert_eq!(node.admin(&deployer, reduce_only(true)).await, (StatusCode::OK, json!({ "type": "admin_applied" })));
    node.advance_to(EFFECTIVE_MS + 60_000, || EFFECTIVE_MS + 60_000).await;
    let block = chain.mine_at(EFFECTIVE_SECS + 60).await;
    node.wait("the mirror to pass the effective time", move |e| completed_through(e) >= Some(block)).await;
    let refused = json!({ "type": "admin_rejected", "reason": "invalid signature" });
    assert_eq!(node.admin(&deployer, reduce_only(false)).await, (StatusCode::OK, refused));
    assert_eq!(node.admin(&key_b, reduce_only(false)).await, (StatusCode::OK, json!({ "type": "admin_applied" })));
    let (_, view) = node.get("/v1/operator").await;
    assert_eq!((view["operator"].clone(), view["pending_rotation"].clone()), (json!(b_key), json!(null)));
    let on_chain = chain.view(chain.registry, operatorInEffectCall {}).await;
    assert_eq!((on_chain.candidate, on_chain.adminKey), (b, address(&key_b)));

    // b's self-bond cannot leave its stake while it is registered.
    let revert = chain.reverted(b, chain.staking, startUnstakeCall { amount: monkey(40_000) }.abi_encode()).await;
    assert_eq!(revert[..4], SelfBondLocked::SELECTOR);

    // The log replays to every root, with the elected key in effect.
    let dir = node.dir.clone();
    let operator = node.query(|e| e.operator().copied()).await;
    node.stop().await;
    let replayed = replay_path(&dir).expect("the log replays");
    assert_eq!(replayed.engine.operator().copied(), operator);
    assert_eq!(replayed.engine.operator_election().last, Some(election));
}

/// The scenario's development registry parameters, which #360 replaces for production, and the
/// bounds the timelock may move them within.
fn dev_parameters() -> (Parameters, Parameters, Parameters) {
    let parameters = Parameters {
        epochLength: 3_600,
        minTerm: 3_600,
        finalityMargin: FINALITY_MARGIN_SECS,
        marginBps: 1_000,
        eligibilityBps: 500,
        quorumBps: 2_000,
        maxCandidates: 10,
        minSelfBond: monkey(1_000),
        livenessTimeout: 7 * 24 * 3_600,
    };
    let minimums = Parameters {
        epochLength: 60,
        minTerm: 0,
        finalityMargin: 0,
        marginBps: 0,
        eligibilityBps: 0,
        quorumBps: 0,
        maxCandidates: 1,
        minSelfBond: U256::ZERO,
        livenessTimeout: 60,
    };
    let maximums = Parameters {
        epochLength: 30 * 86_400,
        minTerm: 365 * 86_400,
        finalityMargin: 86_400,
        marginBps: 10_000,
        eligibilityBps: 10_000,
        quorumBps: 10_000,
        maxCandidates: 100,
        minSelfBond: monkey(1_000_000),
        livenessTimeout: 30 * 24 * 3_600,
    };
    (parameters, minimums, maximums)
}

/// `amount` whole MONKEY in base units.
fn monkey(amount: u64) -> U256 {
    U256::from(amount) * U256::from(10).pow(U256::from(18))
}

fn address(key: &SigningKey) -> Address {
    Address::from(engine_core::eip712::address(key.verifying_key()).0)
}

/// The last finalised staking block the engine has recorded as fully mirrored.
fn completed_through(engine: &engine_core::Engine) -> Option<u64> {
    engine.staking().and_then(|staking| staking.completed_through).map(|checkpoint| checkpoint.block)
}
