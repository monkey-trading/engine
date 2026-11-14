//! The parameter votes dev scenario, from `V56`, which `scripts/dev-parameter-votes.sh` runs: A, B
//! and C register and delegate 40, 30 and 30 thousand MONKEY, and a close adopts them as the rotation
//! set. Once the set is in effect its members vote on the real `ParameterVote` contract, and the
//! custody watcher mirrors each passed proposal into the real sequencer: a margin raise passes with
//! 70% of the weight and applies seven warped days after it is logged, while a proposal with 60%
//! never passes; a fast-path stress floor at level 2 applies at once, survives the operator's attempt
//! to clear it, and lapses at its lifetime; a fast-path margin cut passes on chain and the engine
//! refuses it; and the operator's signed profit-fee `SetParams` is refused. `/v1/house/state` lists
//! the waiting change and the restriction, `/v1/operators` the open proposal, and the log replays.

use std::sync::Arc;

use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::TransactionReceipt;
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use axum::http::StatusCode;
use engine_core::command::{AdminCommand, Command};
use engine_core::market::{MarketSpec, Tier};
use engine_core::types::{MarketId, Millis};
use k256::ecdsa::SigningKey;
use serde_json::{Value, json};

use super::chain::{Chain, Parameters, delegateCall, setParametersCall, setRotationCall};
use super::node::Node;
use super::{CONFIG_HASH, DIGEST_A, DIGEST_B, FINALITY_MARGIN_SECS, T0_SECS, address, monkey};
use crate::checkpoints::scenario::deploy;
use crate::sequencer::replay_path;
use crate::tee::EnclaveKey;

sol! {
    function setVault(address vault_) external;
    function setRegistry(address registry_) external;
    function setSchedule(address schedule_) external;
    function currentSetId() external view returns (uint32);
    function propose(bytes command, bool fast) external returns (uint256);
    function vote(uint256 id) external;
    function proposalCount() external view returns (uint256);
    event ProposalPassed(
        uint256 indexed id,
        uint32 indexed setId,
        bool fast,
        uint256 yesWeight,
        uint256 totalWeight,
        bytes32 commandHash,
        bytes command
    );
}

const DIGEST_C: [u8; 48] = [0xc1; 48];
/// Mainnet's beacon genesis time, which the seed receiver the schedule needs is built with; the
/// scenario draws no turn.
const BEACON_GENESIS: u64 = 1_606_824_023;
/// Each member's stake, all of it delegated to itself, in whole MONKEY, and its self-bond.
const STAKES: [u64; 3] = [40_000, 30_000, 30_000];
const BOND: u64 = 2_000;
/// The vote contract's windows and the restriction lifetime the node records: development values.
const NORMAL_WINDOW_SECS: u64 = DAY_SECS;
const FAST_WINDOW_SECS: u64 = 3_600;
const TTL_MS: Millis = DAY_SECS as Millis * 1000;
/// The close that adopts the set, at the end of the registry's first one-day epoch, and when the set
/// takes effect.
const SET_CLOSE_SECS: u64 = T0_SECS + DAY_SECS + 3 * 3_600;
const SET_EFFECTIVE_SECS: u64 = SET_CLOSE_SECS + 7 * DAY_SECS + FINALITY_MARGIN_SECS;
const NOTICE_MS: Millis = 7 * DAY_SECS as Millis * 1000;
const DAY_SECS: u64 = 86_400;
const BTC: MarketId = 1;
const ETH: MarketId = 2;

#[tokio::test]
#[ignore = "requires Anvil, Python 3, and compiled contracts; scripts/dev-parameter-votes.sh runs it"]
async fn dev_scenario_members_vote_parameter_changes_by_stake() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
    );
    // The dev operator key `config/dev.toml` names, the operator in effect throughout.
    let operator = SigningKey::from_slice(&[0x42; 32]).unwrap();
    let keys = [0x55, 0x56, 0x57].map(|byte| SigningKey::from_slice(&[byte; 32]).unwrap());
    let enclave = Arc::new(EnclaveKey::generate());
    let mut chain = Chain::start(Address::from(enclave.address()), address(&operator)).await;
    let members = [chain.accounts[2], chain.accounts[3], chain.accounts[4]];
    let [a, b, c] = members;
    let receiver = deploy(
        &chain.provider,
        chain.accounts[0],
        "SeedReceiver.sol/SeedReceiver.json",
        &(chain.accounts[9], BEACON_GENESIS).abi_encode_params(),
    )
    .await;
    let (schedule, _) = chain.deploy_schedule(receiver).await;
    chain.schedule = Some(schedule);
    let vote_args = (schedule, NORMAL_WINDOW_SECS, FAST_WINDOW_SECS).abi_encode_params();
    let vote = deploy(&chain.provider, chain.accounts[0], "ParameterVote.sol/ParameterVote.json", &vote_args).await;
    chain.vote = Some((vote, TTL_MS));
    chain.deploy_lens().await;
    let vouching = enclave.clone();
    let mut node = Node::start(&chain, enclave).await;
    let recorded = node.query(|e| e.governance().config).await.expect("the node records the vote contract");
    assert_eq!((recorded.vote.0, recorded.restriction_ttl_ms), (vote.0.0, TTL_MS));
    println!("PASS: the node recorded the vote contract and a one-day restriction lifetime at V56's activation");

    // 1. A, B and C stake and register; the timelock lengthens the election epoch to a day, names the
    //    vault, the registry and the schedule to each other, approves the releases, and turns rotation
    //    on; each member delegates its whole stake to itself, and a close adopts them as the set.
    for ((member, key), (stake, digest)) in
        members.iter().zip(&keys).zip(STAKES.into_iter().zip([DIGEST_A, DIGEST_B, DIGEST_C]))
    {
        chain.stake(*member, monkey(stake)).await;
        chain.register(*member, key, monkey(BOND), digest).await;
    }
    let daily = Parameters { epochLength: DAY_SECS, ..super::dev_parameters().0 };
    let mut calls = vec![
        (chain.registry, setParametersCall { parameters: daily }.abi_encode()),
        (chain.registry, setVaultCall { vault_: chain.vault }.abi_encode()),
        (chain.vault, setRegistryCall { registry_: chain.registry }.abi_encode()),
        (chain.registry, setScheduleCall { schedule_: schedule }.abi_encode()),
    ];
    for (member, digest) in members.iter().zip([DIGEST_A, DIGEST_B, DIGEST_C]) {
        calls.push((chain.registry, chain.approval(*member, digest)));
    }
    calls.push((chain.registry, setRotationCall { enabled: true }.abi_encode()));
    chain.govern_each(calls).await;
    for (member, stake) in members.iter().zip(STAKES) {
        chain
            .send(*member, chain.staking, delegateCall { candidate: *member, amount: monkey(stake) }.abi_encode())
            .await;
    }
    // A close adopts only members with a promotable standby key: the vault's signer vouches for each.
    let releases = [DIGEST_A, DIGEST_B, DIGEST_C].map(|digest| {
        alloy::primitives::B256::from(
            crate::tee::sealing::Release { launch_digest: digest, config_sha256: CONFIG_HASH }.id(),
        )
    });
    let standbys: Vec<_> = members.into_iter().zip(releases).collect();
    chain.register_standbys(&standbys, &vouching).await;
    node.advance_to((SET_CLOSE_SECS as Millis + 60) * 1000).await;
    chain.close_epoch_at(SET_CLOSE_SECS).await;
    node.wait("the rotation set to mirror", |e| e.daily_turns().sets.contains_key(&1)).await;
    node.advance_to((SET_EFFECTIVE_SECS as Millis + 60) * 1000).await;
    chain.mine_at(SET_EFFECTIVE_SECS + 30).await;
    assert_eq!(chain.view(schedule, currentSetIdCall {}).await, 1);
    println!("PASS: a close adopted A, B and C at 40:30:30, and the set is in effect");

    // 2. A margin raise in BTC passes with A's and B's 70%; ETH's raise, with B's and C's 60%, does not.
    let raise = margin(&node, BTC, 200).await;
    let (_, raise_logged) = propose(&mut chain, &node, a, &raise, false, &[b]).await;
    let pending = house_state(&node).await["governance"]["pending"].clone();
    assert_eq!(pending[0]["proposal"], json!(1), "{pending}");
    assert_eq!(pending[0]["applies_at"], json!(raise_logged + NOTICE_MS), "{pending}");
    let eth_raise = margin(&node, ETH, 200).await;
    let (open, _) = submit(&chain, b, &eth_raise, false).await;
    chain.send(c, vote, voteCall { id: U256::from(open) }.abi_encode()).await;
    chain.mine_at(chain.latest_timestamp().await + 1).await;
    // The list is read at a final block, which can trail C's vote: wait for the vote to show.
    let weighed = json!(monkey(60_000).to_string());
    let proposals = operators(&node, |proposals| proposals["open"][0]["yes_weight"] == weighed).await;
    assert_eq!(proposals["open"][0]["id"], json!(open), "{proposals}");
    assert_eq!(proposals["open"][0]["yes_weight"], weighed, "{proposals}");
    println!(
        "PASS: the BTC raise passed with 70% and waits seven days; the ETH raise is open at 60% and has not passed"
    );

    // 3. A fast-path stress floor at level 2 in BTC applies at once; the operator's key setting its own
    //    floor to 0 leaves it in force.
    let floor = AdminCommand::SetStressFloor { market: Some(BTC), level: 2 };
    let (id, floor_logged) = propose(&mut chain, &node, a, &floor, true, &[b]).await;
    assert_eq!(node.query(|e| e.markets()[&BTC].stress.level).await, 2);
    let restriction = house_state(&node).await["governance"]["restrictions"][id.to_string()].clone();
    assert_eq!(restriction["expires_at"], json!(floor_logged + TTL_MS), "{restriction}");
    let lever = AdminCommand::SetStressFloor { market: Some(BTC), level: 0 };
    assert_eq!(node.admin(&operator, lever).await, (StatusCode::OK, json!({ "type": "admin_applied" })));
    assert_eq!(node.query(|e| e.markets()[&BTC].stress.level).await, 2);
    println!("PASS: the fast-path floor put BTC at stress level 2 at once, and the operator's own floor left it there");

    // 4. A fast-path margin cut passes on chain, and the engine refuses it.
    let cut = margin(&node, BTC, 50).await;
    let (id, _) = propose(&mut chain, &node, a, &cut, true, &[b]).await;
    assert!(!node.query(move |e| e.governance().restrictions.contains_key(&id)).await, "the cut restricts nothing");
    assert_eq!(band(&node, BTC).await, 100);
    println!("PASS: the fast-path margin cut passed on chain and the engine refused it");

    // 5. The operator's key signs a profit-fee SetParams, which is refused while rotation is on.
    let mut params = node.query(|e| e.baseline_params()).await.expect("genesis sets parameters");
    params.profit_fee_bps += 100;
    let refused =
        json!({ "type": "admin_rejected", "reason": "while rotation is on, only a passed vote changes this" });
    assert_eq!(node.admin(&operator, AdminCommand::SetParams(params)).await, (StatusCode::OK, refused));
    println!("PASS: the operator's signed profit-fee SetParams was refused");

    // 6. At the restriction's lifetime the floor lapses.
    node.advance_to(floor_logged + TTL_MS).await;
    node.submit(Command::Tick).await;
    assert_eq!(node.query(|e| e.markets()[&BTC].stress.level).await, 0);
    println!("PASS: the level-2 floor lapsed at its one-day lifetime");

    // 7. Seven warped days after it was logged the raise applies; the 60% proposal never does.
    node.advance_to(raise_logged + NOTICE_MS - 60_000).await;
    node.submit(Command::Tick).await;
    assert_eq!(band(&node, BTC).await, 100, "nothing applies before its time");
    node.advance_to(raise_logged + NOTICE_MS).await;
    node.submit(Command::Tick).await;
    assert_eq!(band(&node, BTC).await, 200);
    assert_eq!((band(&node, ETH).await, node.query(|e| e.governance().pending.len()).await), (100, 0));
    let governance = house_state(&node).await["governance"].clone();
    assert_eq!((&governance["pending"], &governance["restrictions"]), (&Value::Null, &Value::Null), "{governance}");
    println!("PASS: the BTC raise applied seven warped days after it was logged, and the 60% ETH raise never applied");

    // 8. The log replays to its roots, with the same votes and markets.
    let live = node.query(|e| (e.governance().clone(), e.markets()[&BTC].spec.clone())).await;
    let dir = node.dir.clone();
    node.stop().await;
    let replayed = replay_path(&dir).expect("the log replays");
    assert_eq!((replayed.engine.governance().clone(), replayed.engine.markets()[&BTC].spec.clone()), live);
    println!("PASS: the log replays to its roots");
}

/// `market`'s baseline spec with its first margin band at `bps`.
async fn margin(node: &Node, market: MarketId, bps: i64) -> AdminCommand {
    let spec = node.query(move |e| e.baseline_spec(market)).await.expect("listed");
    let mut tiers = spec.margin_tiers.clone();
    tiers[0] = Tier { bps, ..tiers[0] };
    AdminCommand::UpsertMarket(MarketSpec { margin_tiers: tiers, ..spec })
}

/// `market`'s first margin band in force.
async fn band(node: &Node, market: MarketId) -> i64 {
    node.query(move |e| e.markets()[&market].spec.margin_tiers[0].bps).await
}

/// `proposer` proposes `command`'s canonical JSON on `fast`: its id and the proposal's receipt.
async fn submit(chain: &Chain, proposer: Address, command: &AdminCommand, fast: bool) -> (u64, TransactionReceipt) {
    let vote = chain.vote.expect("the scenario deploys the vote contract").0;
    let bytes = Bytes::from(serde_json::to_vec(command).unwrap());
    let receipt = chain.send(proposer, vote, proposeCall { command: bytes, fast }.abi_encode()).await;
    (chain.view(vote, proposalCountCall {}).await.to(), receipt)
}

/// `proposer` proposes `command` on `fast` and each of `voters` votes for it, passing it, and the
/// node mirrors it: its id and the time it was logged.
async fn propose(
    chain: &mut Chain,
    node: &Node,
    proposer: Address,
    command: &AdminCommand,
    fast: bool,
    voters: &[Address],
) -> (u64, Millis) {
    let (id, mut receipt) = submit(chain, proposer, command, fast).await;
    let vote = chain.vote.expect("the scenario deploys the vote contract").0;
    for voter in voters {
        receipt = chain.send(*voter, vote, voteCall { id: U256::from(id) }.abi_encode()).await;
    }
    let passed = receipt
        .logs()
        .iter()
        .find(|log| log.topic0() == Some(&ProposalPassed::SIGNATURE_HASH))
        .map(|log| format!("{}:{}", log.transaction_hash.unwrap(), log.log_index.unwrap()))
        .expect("the last vote passes the proposal");
    node.wait(&format!("proposal {id} to mirror"), move |e| e.is_staking_credited(&passed)).await;
    (id, node.query(|e| e.now()).await)
}

/// `GET /v1/house/state`.
async fn house_state(node: &Node) -> Value {
    let (status, body) = node.get("/v1/house/state").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// The proposals `GET /v1/operators` lists, once `ready` holds for them.
async fn operators(node: &Node, ready: impl Fn(&Value) -> bool) -> Value {
    let mut last = Value::Null;
    for _ in 0..200 {
        let (status, body) = node.get("/v1/operators").await;
        if status == StatusCode::OK && ready(&body["proposals"]) {
            return body["proposals"].clone();
        }
        last = body;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("/v1/operators never listed the proposals as expected; the last answer: {last}");
}
