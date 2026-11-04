//! The time in office dev scenario on a local Anvil, from `V50`: the real registry elects b, whose
//! key takes the office, and a second election, taking effect halfway through a distribution epoch,
//! hands it to a. The epoch's close credits each about half the operator's fifth, by the time each
//! held the office; each part shows in its account's history, the vault pays each claim, and the log
//! replays.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::sol_types::SolCall;
use axum::http::StatusCode;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Millis, Side};
use k256::ecdsa::SigningKey;

use super::chain::{Chain, delegateCall, redelegateCall, setRotationCall};
use super::node::Node;
use super::operator_share::{
    FLOOR_UNITS, TRADER_KEY, allocated_of, claim, closed_epoch, declare, excess_of, next_epoch, round_trip,
};
use super::{
    DIGEST_A, DIGEST_B, DISTRIBUTION_EPOCH_SECS, EFFECTIVE_MS, EFFECTIVE_SECS, ELECTION_CLOSE_SECS,
    FINALITY_MARGIN_SECS, T0_SECS, address, completed_through, monkey,
};
use crate::sequencer::replay_path;
use crate::tee::EnclaveKey;

/// The distribution epoch the second election takes effect halfway through: the sixteenth.
const SPLIT_EPOCH: u64 = 15;

#[tokio::test]
#[ignore = "requires Anvil, Python 3, and compiled contracts; scripts/dev-operator-share.sh runs it"]
async fn dev_scenario_a_handover_halfway_through_an_epoch_splits_the_operators_share() {
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
    let (incoming, outgoing, client) = (AccountId(a.into()), AccountId(b.into()), AccountId(trader.into()));

    // 1. The house holds the floor exactly and a client holds a million, both final.
    chain.fund(None, U256::from(FLOOR_UNITS)).await;
    chain.fund(Some(trader), U256::from(FLOOR_UNITS)).await;
    declare(&node, &trader_key).await;
    node.wait("the deposits to credit and finalise", move |e| {
        e.house_cash() == engine_core::Fixed::from_int(1_000_000)
            && e.accounts().get(&client).is_some_and(|account| {
                account.unfinalised_deposits == engine_core::Fixed::ZERO
                    && account.balance == engine_core::Fixed::from_int(1_000_000)
            })
    })
    .await;

    // 2. Three accounts stake, two register, all delegate, and rotation turns on; the close elects b,
    //    whose key takes the office once its effective time has passed.
    for (account, amount) in [(a, 30_000), (b, 40_000), (c, 20_000)] {
        chain.stake(account, monkey(amount)).await;
    }
    chain.register(a, &key_a, monkey(2_000), DIGEST_A).await;
    chain.register(b, &key_b, monkey(2_000), DIGEST_B).await;
    let approvals = vec![chain.approval(a, DIGEST_A), chain.approval(b, DIGEST_B)];
    chain.govern(approvals).await;
    for (from, candidate, amount) in [(a, a, 30_000), (b, b, 40_000), (c, b, 20_000)] {
        chain.send(from, chain.staking, delegateCall { candidate, amount: monkey(amount) }.abi_encode()).await;
    }
    chain.govern(vec![setRotationCall { enabled: true }.abi_encode()]).await;
    node.wait("rotation to mirror on", |e| e.operator_election().rotation_enabled).await;
    node.advance_to((ELECTION_CLOSE_SECS as Millis + 60) * 1000).await;
    chain.close_epoch_at(ELECTION_CLOSE_SECS).await;
    node.wait("the election to mirror", |e| e.operator_election().last.is_some()).await;
    node.advance_to(EFFECTIVE_MS + 60_000).await;
    let block = chain.mine_at(EFFECTIVE_SECS + 60).await;
    node.wait("the mirror to pass the effective time", move |e| completed_through(e) >= Some(block)).await;
    node.submit(Command::Tick).await;
    assert_eq!(node.query(|e| e.operator_candidate()).await, Some(outgoing));

    // 3. c moves its 20,000 from b to a, which then leads by more than the registry's margin, and a
    //    close elects a to take effect halfway through the epoch `SPLIT_EPOCH`.
    let redelegate = redelegateCall { from: b, to: a, amount: monkey(20_000) }.abi_encode();
    chain.send(c, chain.staking, redelegate).await;
    let start_secs = T0_SECS + SPLIT_EPOCH * DISTRIBUTION_EPOCH_SECS;
    let handover_secs = start_secs + DISTRIBUTION_EPOCH_SECS / 2;
    let close_secs = handover_secs - 7 * 86_400 - FINALITY_MARGIN_SECS;
    node.advance_to((close_secs as Millis + 60) * 1000).await;
    chain.close_epoch_at(close_secs).await;
    node.wait("the second election to mirror", move |e| {
        e.operator_election().last.is_some_and(|last| last.candidate == incoming)
    })
    .await;
    let handover_ms = handover_secs as Millis * 1000;
    assert_eq!(node.query(|e| e.operator_election().last.map(|last| last.effective_at)).await, Some(handover_ms));

    // 4. The close at the epoch's start, with the house at the floor, pays no one and starts the
    //    epoch's count; a client then loses at least $900 to the house.
    let start_ms = start_secs as Millis * 1000;
    node.advance_to(start_ms).await;
    node.submit(Command::Tick).await;
    let since = node.query(|e| e.staking().and_then(|staking| staking.office.as_ref()).map(|o| o.since)).await;
    assert_eq!(since, Some(start_ms));
    assert_eq!(node.query(move |e| e.distribution(outgoing)).await, UsdcUnits(0));
    round_trip(&node, &trader_key, Side::Buy, "3", "100000", "99700").await;

    // 5. A minute past the election's time, the mirror passes it and b hands the office to a.
    node.advance_to(handover_ms + 60_000).await;
    let block = chain.mine_at(handover_secs + 60).await;
    node.wait("the mirror to pass the second election", move |e| completed_through(e) >= Some(block)).await;
    node.submit(Command::Tick).await;
    assert_eq!(node.query(|e| e.operator().map(|op| op.address)).await, Some(AccountId(address(&key_a).into())));
    assert_eq!(node.query(|e| e.operator_candidate()).await, Some(incoming));

    // 6. The close credits b and a about half the fifth each: b held the office twelve hours and a
    //    minute of the day, and a the rest.
    let events = next_epoch(&mut node).await;
    let (fifth, day) = (excess_of(&events).0 / 5, DISTRIBUTION_EPOCH_SECS as u128 * 1000);
    assert!(fifth >= 180_000_000, "{events:?}");
    let served = (handover_ms + 60_000 - start_ms) as u128;
    let parts = [(outgoing, fifth * served / day), (incoming, fifth * (day - served) / day)];
    let epoch = closed_epoch(&events);
    for (account, amount) in parts {
        let allocated = Event::OperatorShareAllocated { account, epoch, amount: UsdcUnits(amount) };
        assert!(events.contains(&allocated), "{account}: {events:?}");
    }
    let shares = events.iter().filter(|e| matches!(e, Event::OperatorShareAllocated { .. })).count();
    assert_eq!(shares, 2, "{events:?}");
    assert!(allocated_of(&events).0 <= excess_of(&events).0, "{events:?}");

    // 7. Each part is in its account's history.
    for (account, amount) in parts {
        assert_eq!(operator_shares_in_history(&node, account).await, [amount]);
    }

    // 8. b and a each claim their whole leaf, part and stakers' share, through the vault.
    for (account, (_, part)) in [b, a].into_iter().zip(parts) {
        let id = AccountId(account.into());
        let leaf = node.query(move |e| e.distribution(id)).await;
        assert!(leaf.0 > part, "{account}: {leaf:?}");
        assert_eq!(claim(&chain, &node, account, leaf).await, U256::from(leaf.0), "{account}");
    }

    // 9. The node stops, and its log replays to the same time in office and distributions.
    let dir = node.dir.clone();
    let office = node.query(|e| e.staking().and_then(|staking| staking.office.clone())).await;
    let leaves = node.query(move |e| [outgoing, incoming].map(|account| e.distribution(account))).await;
    node.stop().await;
    let replayed = replay_path(&dir).expect("the log replays");
    assert_eq!(replayed.engine.staking().and_then(|staking| staking.office.clone()), office);
    assert_eq!([outgoing, incoming].map(|account| replayed.engine.distribution(account)), leaves);
}

/// The amounts of `account`'s `operator_share_allocated` events in its history, once the index has
/// any.
async fn operator_shares_in_history(node: &Node, account: AccountId) -> Vec<u128> {
    let path = format!("/v1/accounts/{account}/history?kind=operator_share_allocated");
    for _ in 0..200 {
        let (status, page) = node.get(&path).await;
        let events = page["events"].as_array().filter(|events| status == StatusCode::OK && !events.is_empty());
        if let Some(events) = events {
            return events.iter().map(|e| e["amount"].as_str().unwrap().parse::<u128>().unwrap()).collect();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the history index never listed {account}'s operator share");
}
