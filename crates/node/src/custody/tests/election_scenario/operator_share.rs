//! The operator's share dev scenario on a local Anvil: the real registry elects a candidate and the
//! custody watcher mirrors it into the real sequencer, a client's losses put the house above the
//! floor, and an epoch's close pays the candidate a fifth and stakers the rest. Each claims through
//! the vault against the posted root, an epoch below the floor pays neither, and the candidate's
//! unclaimed leaf registers in the vault's exit, from the stopped node's public log alone.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol;
use alloy::sol_types::SolCall;
use axum::http::StatusCode;
use engine_core::access::{EligibilityClass, EligibilityRequest};
use engine_core::auth::RequestAuth;
use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::merkle::DistributionProof;
use engine_core::price::SourceQuote;
use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Millis, Side};
use k256::ecdsa::SigningKey;

use super::chain::{Chain, delegateCall, setRotationCall};
use super::node::Node;
use super::{
    DIGEST_A, DIGEST_B, EFFECTIVE_MS, EFFECTIVE_SECS, ELECTION_CLOSE_SECS, address, completed_through, monkey,
};
use crate::clock::Clock;
use crate::exit::{ExitSubject, exit_bundle};
use crate::sequencer::replay_path;
use crate::tee::EnclaveKey;
use crate::test_support::DEV_EXIT_SCHEDULE;

/// The liquidity floor the scenario's genesis records, in USDC base units.
const FLOOR_UNITS: u64 = 1_000_000_000_000;
/// The published private key of the default Anvil account the scenario trades through, index 5.
const TRADER_KEY: &str = "8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba";
/// BTC, as the dev config lists it.
const BTC: u32 = 1;

sol! {
    function balanceOf(address account) external view returns (uint256);
    function latestSeq() external view returns (uint64);
    function distributionsPaid() external view returns (uint256);
    function exitDistributionClaims(address account) external view returns (uint256);
    function claimDistribution(address account, uint256 entitled, uint64 checkpointSeq, bytes32[] siblings) external returns (uint256);
}

#[tokio::test]
#[ignore = "requires Anvil, Python 3, and compiled contracts; scripts/dev-operator-share.sh runs it"]
async fn dev_scenario_the_elected_operator_and_a_staker_each_claim_their_share() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
    );
    let deployer = SigningKey::from_slice(&[0x42; 32]).unwrap();
    let (key_a, key_b) = (SigningKey::from_slice(&[0x55; 32]).unwrap(), SigningKey::from_slice(&[0x56; 32]).unwrap());
    let trader_key = SigningKey::from_slice(&hex::decode(TRADER_KEY).unwrap()).unwrap();
    let enclave = Arc::new(EnclaveKey::generate());
    let mut chain = Chain::start(Address::from(enclave.address()), address(&deployer)).await;
    let mut node = Node::start_posting_checkpoints(&chain, enclave).await;
    let [a, b, c, trader] = [chain.accounts[2], chain.accounts[3], chain.accounts[4], chain.accounts[5]];
    let (operator, staker, client) = (AccountId(b.into()), AccountId(c.into()), AccountId(trader.into()));
    assert_eq!(address(&trader_key), trader, "the trader's key is the published Anvil key");

    // 1. The house holds the floor exactly and a client holds a million, both final: the client's
    //    waits for its declaration, as the screening source requires.
    chain.fund(None, U256::from(FLOOR_UNITS)).await;
    chain.fund(Some(trader), U256::from(FLOOR_UNITS)).await;
    declare(&node, &trader_key).await;
    node.wait("the deposits to credit and finalise", move |e| {
        e.house_cash() == engine_core::Fixed::from_int(1_000_000)
            && e.accounts().get(&client).is_some_and(|account| {
                account.balance == engine_core::Fixed::from_int(1_000_000)
                    && account.unfinalised_deposits == engine_core::Fixed::ZERO
            })
    })
    .await;

    // 2. Three accounts stake, two register, all delegate, and the timelock turns rotation on and
    //    approves their releases. The close elects b, whose stake and c's lead.
    for (account, amount) in [(a, 30_000), (b, 40_000), (c, 20_000)] {
        chain.stake(account, monkey(amount)).await;
    }
    chain.register(a, &key_a, monkey(2_000), DIGEST_A).await;
    chain.register(b, &key_b, monkey(2_000), DIGEST_B).await;
    let approvals = vec![chain.approval(a, DIGEST_A), chain.approval(b, DIGEST_B)];
    chain.govern(approvals).await;
    for (from, candidate) in [(a, a), (b, b), (c, b)] {
        chain.send(from, chain.staking, delegateCall { candidate }.abi_encode()).await;
    }
    chain.govern(vec![setRotationCall { enabled: true }.abi_encode()]).await;
    node.wait("rotation to mirror on", |e| e.operator_election().rotation_enabled).await;
    node.advance_to((ELECTION_CLOSE_SECS as Millis + 60) * 1000).await;
    chain.close_epoch_at(ELECTION_CLOSE_SECS).await;
    node.wait("the election to mirror", |e| e.operator_election().last.is_some()).await;

    // 3. Before the election takes effect, the candidate earns nothing: the house is at the floor,
    //    so the epochs the wait crosses pay no one.
    node.advance_to(EFFECTIVE_MS + 60_000).await;
    let block = chain.mine_at(EFFECTIVE_SECS + 60).await;
    node.wait("the mirror to pass the effective time", move |e| completed_through(e) >= Some(block)).await;
    node.submit(Command::Tick).await;
    assert_eq!(node.query(|e| e.operator_candidate()).await, Some(operator));
    assert_eq!(node.query(|e| e.operator().map(|op| op.address)).await, Some(AccountId(address(&key_b).into())));
    let balances = [a, b, c].map(|account| AccountId(account.into()));
    for account in balances {
        assert_eq!(node.query(move |e| e.distribution(account)).await, UsdcUnits(0));
    }

    // 4. A client loses at least $900 to the house, with the spreads it pays, so the close finds that
    //    much USDC above the floor: a fifth to the operator, and the rest to stakers 30:40:20. The
    //    operator also stakes, so its leaf holds both, and only the stakers' rounding stays behind.
    round_trip(&node, &trader_key, Side::Buy, "3", "100000", "99700").await;
    let events = next_epoch(&mut node).await;
    let excess = excess_of(&events).0;
    assert!(excess >= 900_000_000, "{events:?}");
    let share = excess / 5;
    let part = |stake: u128| (excess - share) * stake / 90;
    assert!(
        events.contains(&Event::OperatorShareAllocated {
            account: operator,
            epoch: closed_epoch(&events),
            amount: UsdcUnits(share)
        }),
        "{events:?}"
    );
    let allocated = share + part(30) + part(40) + part(20);
    assert_eq!(allocated_of(&events), UsdcUnits(allocated), "{events:?}");
    assert!(excess - allocated < 3, "{events:?}");
    let (operator_leaf, staker_leaf) = (UsdcUnits(share + part(40)), UsdcUnits(part(20)));
    assert_eq!(node.query(move |e| e.distribution(operator)).await, operator_leaf);
    assert_eq!(node.query(move |e| e.distribution(staker)).await, staker_leaf);

    // 5. The operator and a staker each claim through the vault against a posted root, relayed by
    //    another account; the USDC goes to them.
    assert_eq!(claim(&chain, &node, b, operator_leaf).await, U256::from(operator_leaf.0));
    assert_eq!(claim(&chain, &node, c, staker_leaf).await, U256::from(staker_leaf.0));
    let paid = U256::from(operator_leaf.0 + staker_leaf.0);
    assert_eq!(chain.view(chain.vault, distributionsPaidCall {}).await, paid);

    // 6. The client wins back more than the house has above the floor: the next close finds NAV
    //    below it, and pays neither the operator nor a staker.
    round_trip(&node, &trader_key, Side::Sell, "1", "100900", "100000").await;
    let events = next_epoch(&mut node).await;
    assert!(!events.iter().any(|e| matches!(e, Event::OperatorShareAllocated { .. })), "{events:?}");
    assert_eq!(allocated_of(&events), UsdcUnits(0), "{events:?}");
    assert_eq!(node.query(move |e| e.distribution(operator)).await, operator_leaf);
    assert_eq!(node.query(move |e| e.distribution(staker)).await, staker_leaf);

    // 7. The house earns above the floor again, and the operator leaves the new share unclaimed.
    round_trip(&node, &trader_key, Side::Buy, "3", "100000", "99500").await;
    let events = next_epoch(&mut node).await;
    let later = share_of(&events);
    assert!(later.0 > 0 && later.0 == excess_of(&events).0 / 5, "{events:?}");
    let entitled = node.query(move |e| e.distribution(operator)).await;
    assert!(entitled.0 > operator_leaf.0 + later.0, "{events:?}");
    let posted = posted_proof(&node, operator, entitled).await;
    assert_eq!(posted.entitled, entitled);

    // 8. The node stops; its log replays to the same distributions, with the same candidate.
    let dir = node.dir.clone();
    let election = node.query(|e| *e.operator_election()).await;
    node.stop().await;
    let replayed = replay_path(&dir).expect("the log replays");
    assert_eq!(*replayed.engine.operator_election(), election);
    assert_eq!(replayed.engine.distribution(operator), entitled);

    // 9. No checkpoint posts for the exit period, so the exit starts against the last posted root.
    //    The same leaf registers from the public log alone, for what the vault has not paid, and
    //    once the claim window has passed the operator is paid it.
    let seq = chain.view(chain.vault, latestSeqCall {}).await;
    let bundle = exit_bundle(&dir, seq, ExitSubject::Account(operator)).expect("the log proves the exit");
    let proof = bundle.account.as_ref().and_then(|exit| exit.distribution.as_ref()).expect("the leaf is in the root");
    assert_eq!(proof.entitled, entitled);
    let latest = chain.latest_timestamp().await;
    chain.next_block_at(latest + DEV_EXIT_SCHEDULE.0).await;
    let relayer = chain.accounts[1];
    let (register, claim_exit) = bundle.calls.split_at(bundle.calls.len() - 1);
    for call in register {
        let receipt = transact(&chain, relayer, chain.vault, call.calldata.to_vec()).await;
        assert!(receipt.status(), "{} reverted", call.what);
    }
    let owed = U256::from(entitled.0 - operator_leaf.0);
    assert_eq!(chain.view(chain.vault, exitDistributionClaimsCall { account: b }).await, owed);
    chain.next_block_at(chain.latest_timestamp().await + DEV_EXIT_SCHEDULE.1).await;
    let before = chain.view(chain.usdc, balanceOfCall { account: b }).await;
    let receipt = transact(&chain, relayer, chain.vault, claim_exit[0].calldata.to_vec()).await;
    assert!(receipt.status(), "{} reverted", claim_exit[0].what);
    assert_eq!(chain.view(chain.usdc, balanceOfCall { account: b }).await - before, owed);
}

/// Declares the key's account eligible through the API, as a client does before its deposit credits.
async fn declare(node: &Node, key: &SigningKey) {
    let account = eip712::address(key.verifying_key());
    let nonce = node.query(move |engine| engine.next_request_nonce(account)).await;
    let expires_at = node.clock.now_ms() + 60_000;
    let class = EligibilityClass::Professional;
    let domain = eip712::domain(node.state.operator.chain_id, node.state.operator.vault);
    let hash = eip712::eligibility_hash(&domain, account, class, nonce, expires_at);
    let declaration = EligibilityRequest {
        account,
        class,
        auth: RequestAuth { nonce, expires_at, signature: eip712::sign(key, &hash) },
    };
    let (status, events) = node.post("/v1/eligibility", serde_json::to_value(declaration).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{events}");
    assert!(events.as_array().unwrap().iter().any(|event| event["type"] == "eligibility_attested"), "{events}");
}

/// The trader opens with `open` at `from` and closes the other way at `to`, through the API.
async fn round_trip(node: &Node, key: &SigningKey, open: Side, qty: &str, from: &str, to: &str) {
    let close = if open == Side::Buy { Side::Sell } else { Side::Buy };
    for (mid, side) in [(from, open), (to, close)] {
        let ts = node.clock.now_ms();
        let quotes = ["a", "b"].map(|source| SourceQuote {
            source: source.into(),
            mid: mid.parse().unwrap(),
            ts,
            contract: None,
        });
        node.submit(Command::Prices { market: BTC, quotes: quotes.to_vec() }).await;
        let account = eip712::address(key.verifying_key());
        let mut order = OrderRequest {
            account,
            client_id: 1,
            market: BTC,
            side,
            qty: qty.parse().unwrap(),
            kind: OrderKind::Market { quoted: mid.parse().unwrap(), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        };
        let nonce = node.query(move |engine| engine.next_request_nonce(account)).await;
        let expires_at = ts + 60_000;
        let hash = eip712::order_hash(
            &eip712::domain(node.state.operator.chain_id, node.state.operator.vault),
            &order,
            nonce,
            expires_at,
        );
        order.auth = Some(RequestAuth { nonce, expires_at, signature: eip712::sign(key, &hash) });
        let (status, events) = node.post("/v1/orders", serde_json::to_value(order).unwrap()).await;
        assert!(status == StatusCode::OK && events.to_string().contains("\"fill\""), "{events}");
    }
}

/// Ticks at the end of the open epoch, closing it, and returns the tick's events.
async fn next_epoch(node: &mut Node) -> Vec<Event> {
    let end = node
        .query(|e| {
            let staking = e.staking().expect("the genesis stakes");
            staking.config.epoch_end(staking.config.epochs_elapsed(e.now()))
        })
        .await;
    node.advance_to(end).await;
    node.submit(Command::Tick).await
}

/// The epoch a close's events closed.
fn closed_epoch(events: &[Event]) -> u64 {
    events
        .iter()
        .find_map(|e| match e {
            Event::DistributionEpochClosed { epoch, .. } => Some(*epoch),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no epoch closed: {events:?}"))
}

/// The operator's share among a close's events.
fn share_of(events: &[Event]) -> UsdcUnits {
    events
        .iter()
        .find_map(|e| match e {
            Event::OperatorShareAllocated { amount, .. } => Some(*amount),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no operator share: {events:?}"))
}

/// What a close's events allocated.
fn allocated_of(events: &[Event]) -> UsdcUnits {
    events
        .iter()
        .find_map(|e| match e {
            Event::DistributionEpochClosed { allocated, .. } => Some(*allocated),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no epoch closed: {events:?}"))
}

/// The excess a close's events measured.
fn excess_of(events: &[Event]) -> UsdcUnits {
    events
        .iter()
        .find_map(|e| match e {
            Event::DistributionEpochClosed { excess, .. } => Some(*excess),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no epoch closed: {events:?}"))
}

/// `account`'s distribution proof from the API once the vault has recorded a root naming `entitled`,
/// ticking until a checkpoint carries it there.
async fn posted_proof(node: &Node, account: AccountId, entitled: UsdcUnits) -> DistributionProof {
    crate::test_support::wait_for("the checkpoint worker to post the distribution's root", async {
        loop {
            node.submit(Command::Tick).await;
            let (status, body) = node.get(&format!("/v1/accounts/{account}/distributions")).await;
            if status == StatusCode::OK {
                let answer: crate::sequencer::ClaimAnswer<DistributionProof> = serde_json::from_value(body).unwrap();
                if answer.claimable && answer.proof.entitled == entitled {
                    return answer.proof;
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
}

/// Has a relayer claim `account`'s distribution, returning the USDC `account` received.
async fn claim(chain: &Chain, node: &Node, account: Address, entitled: UsdcUnits) -> U256 {
    let relayer = chain.accounts[1];
    let before = chain.view(chain.usdc, balanceOfCall { account }).await;
    let proof = posted_proof(node, AccountId(account.into()), entitled).await;
    let call = claimDistributionCall {
        account,
        entitled: U256::from(proof.entitled.0),
        checkpointSeq: proof.seq,
        siblings: proof.siblings.clone(),
    };
    assert!(transact(chain, relayer, chain.vault, call.abi_encode()).await.status(), "the claim reverted");
    assert_eq!(chain.view(chain.usdc, balanceOfCall { account: relayer }).await, U256::ZERO);
    chain.view(chain.usdc, balanceOfCall { account }).await - before
}

/// Sends a transaction from an unlocked account and returns its receipt, reverted or not.
async fn transact(chain: &Chain, from: Address, to: Address, input: Vec<u8>) -> TransactionReceipt {
    crate::checkpoints::scenario::transact(
        &chain.provider,
        TransactionRequest::default().from(from).to(to).input(input.into()),
    )
    .await
}
