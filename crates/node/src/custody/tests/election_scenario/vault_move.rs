//! The move to a new vault dev scenario, from `V90`, which `scripts/dev-vault-move.sh` runs on the parameter votes
//! scenario's stack (`parameter_votes::adopted_set`). The alpha's node posts checkpoints to its vault while the
//! rotation set governs: the operator initialises emissions, a trader loses twice and claims the first loss's
//! MONKEY from the minter, and a High award voted in is paid in part by a close. The timelock turns rotation off,
//! the alpha settles at a checkpoint its vault accepts, and `engine carry` builds the carry from its log, which it
//! refuses at an earlier checkpoint, while rotation was on. The guardian retires the alpha's signer; the launch
//! vault and its schedule deploy, and one timelock batch moves the registry's custody and MONKEY's minter to them.
//! The launch node holds until the chain shows the move done, then sequences a genesis carrying every entitlement
//! and award, each award unpaid, and mirrors the staking contract's events again until each stake is the alpha's.
//! The trader claims the rest of its MONKEY against the launch vault, a member's new stake mirrors once past the
//! carried frontier, and a close pays the carried award from its whole amount. The launch log replays and checks
//! against the alpha's, and a carry file with an entitlement raised by one unit is refused.

mod helpers;

use std::sync::Arc;
use std::time::Instant;

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use axum::http::StatusCode;
use engine_core::Fixed;
use engine_core::awards::{Award, AwardApproval, AwardPaid, Severity};
use engine_core::carry::CarryPart;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::Event;
use engine_core::operator_faults::Penalty;
use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Millis, Side};
use k256::ecdsa::SigningKey;
use serde_json::json;

use super::chain::{operatorInEffectCall, setRotationCall};
use super::node::{Node, carry_config, emission_budget};
use super::operator_share::{FLOOR_UNITS, TRADER_KEY, declare, next_epoch, round_trip};
use super::parameter_votes::{
    FAST_WINDOW_SECS, NORMAL_WINDOW_SECS, NOTICE_MS, SET_EFFECTIVE_SECS, TTL_MS, adopted_set, propose, setCustodyCall,
    setRegistryCall,
};
use super::{address, completed_through, monkey};
use crate::checkpoints::scenario::deploy;
use crate::clock::Clock;
use crate::sequencer::replay_path;
use crate::tee::EnclaveKey;
use crate::test_support::{DEV_EXIT_SCHEDULE, scratch_dir};
use helpers::{claim_monkey, close_period, settled_checkpoint};

sol! {
    function setVault(address next) external;
    function retireSigner() external;
    function checkpointSigner() external view returns (address);
    function latestSeq() external view returns (uint64);
    function emissionsClaimed(address account) external view returns (uint256);
}

/// One USDC in USDC base units.
const USDC: u128 = 1_000_000;
/// The seed receiver the launch schedule takes, deployed as `adopted_set` deploys the alpha's.
const BEACON_GENESIS: u64 = 1_606_824_023;
const DAY_SECS: u64 = 86_400;

#[tokio::test]
#[ignore = "requires Anvil, Python 3, and compiled contracts; scripts/dev-vault-move.sh runs it"]
async fn dev_scenario_the_launch_log_carries_monkey_and_awards_and_mirrors_the_stakes_again() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
    );
    let (mut chain, mut node, [a, b, c], operator) = adopted_set(true).await;
    let (researcher, trader) = (chain.accounts[6], chain.accounts[5]);
    let trader_key = SigningKey::from_slice(&hex::decode(TRADER_KEY).unwrap()).unwrap();
    let client = AccountId(trader.into());

    // 1. The timelock names the alpha's vault to the minter, and the operator initialises emissions at its
    //    budget; the house holds the floor and the trader a million through the vault. The trader loses a round
    //    trip, claims its period's MONKEY from the minter against the alpha's accepted root, and loses again, so
    //    more waits unclaimed.
    let (minter, alpha_vault) = (chain.minter, chain.vault);
    chain.govern_each(vec![(minter, setVaultCall { next: alpha_vault }.abi_encode())]).await;
    let schedule = EmissionSchedule { start_ms: node.clock.now_ms(), period_budget: emission_budget(&chain).await };
    let applied = (StatusCode::OK, json!({ "type": "admin_applied" }));
    assert_eq!(node.admin(&operator, AdminCommand::InitEmissions(schedule)).await, applied);
    chain.fund(None, U256::from(FLOOR_UNITS)).await;
    chain.fund(Some(trader), U256::from(FLOOR_UNITS)).await;
    declare(&node, &trader_key).await;
    node.wait("the deposits to credit and finalise", move |e| {
        e.house_cash() == Fixed::from_int(1_000_000)
            && e.accounts().get(&client).is_some_and(|account| {
                account.balance == Fixed::from_int(1_000_000) && account.unfinalised_deposits == Fixed::ZERO
            })
    })
    .await;
    round_trip(&node, &trader_key, Side::Buy, "1", "100000", "99900").await;
    close_period(&mut node).await;
    let first = node.query(move |e| e.entitlement(client)).await;
    assert!(first.0 > 0, "the loss earned MONKEY");
    assert_eq!(claim_monkey(&chain, &node, trader, first).await, U256::from(first.0));
    round_trip(&node, &trader_key, Side::Buy, "1", "100000", "99900").await;
    close_period(&mut node).await;
    let entitled = node.query(move |e| e.entitlement(client)).await;
    assert!(entitled > first, "the second loss earned more");
    println!("PASS: the trader claimed {first} of its MONKEY from the minter, and {entitled} is entitled in all");

    // 2. A proposes a High award to the researcher and the set passes it; seven days on it applies, and a close
    //    above the floor pays it in part.
    node.advance_to((SET_EFFECTIVE_SECS as Millis + 60) * 1000).await;
    chain.mine_at(SET_EFFECTIVE_SECS + 30).await;
    let high = AdminCommand::ApproveAward(AwardApproval {
        recipient: AccountId(researcher.into()),
        severity: Severity::High,
        advisory: "GHSA-m0v3-v4u1-7c4r".into(),
        amount: Severity::High.amount(),
    });
    let (_, logged) = propose(&mut chain, &node, a, &high, false, &[b]).await;
    node.advance_to(logged + NOTICE_MS + 60_000).await;
    node.submit(Command::Tick).await;
    node.wait("the award to apply", |e| e.awards().len() == 1).await;
    chain.fund(None, U256::from(600_000 * USDC)).await;
    node.wait("the house to stand final above the floor", |e| {
        e.distributable_excess().is_some_and(|excess| excess > Fixed::from_int(500_000))
    })
    .await;
    let in_part: u128 = paid(&next_epoch(&mut node, &chain).await).iter().map(|award| award.amount.0).sum();
    assert!(in_part > 0 && in_part < Severity::High.amount().0, "the close paid the award in part: {in_part}");
    println!("PASS: the set passed a High award, which applied seven days on, and a close paid {in_part} of it");

    // 3. The timelock turns rotation off and the open period closes with no fills after: the alpha's last
    //    accepted checkpoint carries, and one from while rotation was on does not.
    let rotating = settled_checkpoint(&node, &chain).await;
    chain.govern(vec![setRotationCall { enabled: false }.abi_encode()]).await;
    node.wait("rotation to mirror off", |e| !e.operator_election().rotation_enabled).await;
    close_period(&mut node).await;
    settled_checkpoint(&node, &chain).await;
    let (dir, at) = (node.dir.clone(), node.clock.now_ms() + 60_000);
    node.stop().await;
    // The alpha's log moves aside: a fresh node's directory is the one it ran in, which the launch node clears.
    let alpha_dir = scratch_dir("vault-move-alpha");
    std::fs::rename(&dir, &alpha_dir).unwrap();
    let seq = chain.view(alpha_vault, latestSeqCall {}).await;
    let refused = crate::carry::build(&alpha_dir, Some(rotating)).unwrap_err();
    assert!(format!("{refused:#}").contains("rotation is on"), "{refused:#}");
    let file = crate::carry::build(&alpha_dir, Some(seq)).expect("the alpha's last accepted checkpoint carries");
    let dir = scratch_dir("vault-move-carry");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("carry.json");
    let sha256 = crate::carry::write(&file, &path).unwrap();
    let bytes = std::fs::metadata(&path).unwrap().len();
    let totals = file.totals;
    println!(
        "PASS: the carry from seq {seq} ({} parts, {bytes} bytes, sha256 {}) carries {} accounts, {} entitled, {} \
         staked and {} award, and seq {rotating}, with rotation on, carries nothing",
        file.parts.len(),
        hex::encode(sha256),
        totals.accounts,
        totals.entitled,
        totals.staked,
        totals.awards
    );

    // 4. The guardian retires the alpha's signer. The launch vault and its schedule deploy, and one timelock batch
    //    names them to the registry and moves MONKEY's minter to the launch vault.
    let guardian = chain.accounts[0];
    chain.send(guardian, alpha_vault, retireSignerCall {}.abi_encode()).await;
    assert_eq!(chain.view(alpha_vault, checkpointSignerCall {}).await, Address::ZERO);
    let launch = Arc::new(EnclaveKey::generate());
    let (exit_period, claim_window, stale_after) = DEV_EXIT_SCHEDULE;
    let signer = Address::from(launch.address());
    let args = (chain.usdc, signer, chain.governance, guardian, exit_period, claim_window, stale_after).abi_encode();
    let vault = deploy(&chain.provider, guardian, "Vault.sol/Vault.json", &args).await;
    chain.vault = vault;
    let receiver_args = (chain.accounts[9], BEACON_GENESIS, DAY_SECS).abi_encode_params();
    let receiver = deploy(&chain.provider, guardian, "SeedReceiver.sol/SeedReceiver.json", &receiver_args).await;
    let (schedule, _) = chain.deploy_schedule(receiver).await;
    chain.schedule = Some(schedule);
    let registry = chain.registry;
    chain
        .govern_each(vec![
            (registry, setCustodyCall { vault_: vault, schedule_: schedule }.abi_encode()),
            (vault, setRegistryCall { registry_: registry }.abi_encode()),
            (minter, setVaultCall { next: vault }.abi_encode()),
        ])
        .await;
    let in_effect = chain.view(registry, operatorInEffectCall {}).await;
    assert_eq!(in_effect.adminKey, address(&operator), "the launch node's operator key is in effect");
    // The launch deployment's vote and fault proof contracts, over the launch schedule and vault, which the launch
    // node records and mirrors from the carried frontier on.
    let vote_args = (schedule, NORMAL_WINDOW_SECS, FAST_WINDOW_SECS).abi_encode_params();
    let vote = deploy(&chain.provider, guardian, "ParameterVote.sol/ParameterVote.json", &vote_args).await;
    let fault_proof =
        deploy(&chain.provider, guardian, "FaultProof.sol/FaultProof.json", &(vault,).abi_encode_params()).await;
    chain.vote = Some((vote, TTL_MS));
    chain.fault_proof = Some((fault_proof, Penalty { initial_bps: 5_000, decay_ms: 4 * DAY_SECS as Millis * 1000 }));
    println!("PASS: the guardian retired the alpha's signer, and one timelock batch moved custody and minting");

    // 5. The launch node passes the start's checks and sequences a genesis carrying the alpha's state.
    chain.carry = Some((alpha_vault, path.clone(), at));
    let head = chain.mine_at(chain.latest_timestamp().await + 1).await;
    let started = Instant::now();
    let mut node = Node::start_posting_checkpoints(&chain, launch).await;
    let alpha = crate::exit::engine_at_checkpoint(&alpha_dir, seq).unwrap();
    let carried = node.query(|e| (e.carried_from(), e.entitlements().clone(), e.awards().clone())).await;
    let unpaid: std::collections::BTreeMap<u64, Award> =
        alpha.awards().iter().map(|(id, award)| (*id, Award { paid: UsdcUnits(0), ..award.clone() })).collect();
    assert_eq!(carried.0, Some(file.source));
    assert_eq!((file.source.vault, file.source.seq), (AccountId(alpha_vault.into()), seq));
    assert_eq!(&carried.1, alpha.entitlements());
    assert_eq!(carried.2, unpaid);
    // The genesis carried no stake: the launch node mirrors the staking contract's events again from its
    // deployment, and each stake is the alpha's once it reaches the alpha's frontier.
    assert!(!file.parts.iter().any(|part| matches!(part, CarryPart::Stakes { .. })), "the carry holds no stake");
    node.wait("the carried stakes to be mirrored again", |e| !e.remirroring()).await;
    let theirs = alpha.staking().and_then(|staking| staking.completed_through).expect("the alpha completed its scan");
    let (stakes, completed) =
        node.query(|e| (e.staked_balances().clone(), e.staking().and_then(|staking| staking.completed_through))).await;
    assert_eq!(&stakes, alpha.staked_balances(), "each stake is the alpha's");
    assert!(!stakes.is_empty() && completed.is_some_and(|completed| completed.block >= theirs.block));
    println!("PASS: the launch log mirrored each of the alpha's {} stakes again, to its frontier", stakes.len());
    let recorded = node.query(|e| (e.governance().config, e.operator_faults().config)).await;
    assert_eq!(recorded.0.map(|config| config.vote.0), Some(vote.0.0), "the launch node records its vote contract");
    assert_eq!(recorded.1.map(|config| config.contract.0), Some(fault_proof.0.0), "and its fault proof contract");
    assert!(alpha.awards()[&0].paid.0 > 0, "the alpha paid its award in part");
    let after = node.query(|e| e.emissions().map(|emissions| emissions.entitled)).await;
    let alpha_entitled = alpha.emissions().map(|emissions| emissions.entitled);
    assert_eq!(after, alpha_entitled, "no period before the launch log's first command allocated");
    node.wait("the mirror to catch up past the move", move |e| completed_through(e) >= Some(head)).await;
    println!(
        "PASS: the launch log carried every entitlement and award, each award unpaid, from seq {seq}; its mirror \
         caught up from the staking contract's deployment in {:?}",
        started.elapsed()
    );

    // 6. The trader claims the rest of its MONKEY against the launch vault, a member's new stake mirrors once
    //    past the carried frontier, and a close above the floor pays the carried award from its whole amount.
    let rest = claim_monkey(&chain, &node, trader, entitled).await;
    assert_eq!(rest, U256::from(entitled.0 - first.0));
    assert_eq!(chain.view(minter, emissionsClaimedCall { account: trader }).await, U256::from(entitled.0));
    let member = AccountId(c.into());
    let staked = node.query(move |e| e.staked(member)).await;
    chain.stake(c, monkey(1_000)).await;
    let more = TokenUnits(staked.0 + monkey(1_000).to::<u128>());
    node.wait("the new stake to mirror", move |e| e.staked(member) == more).await;
    chain.fund(None, U256::from(FLOOR_UNITS) + U256::from(600_000 * USDC)).await;
    node.wait("the launch house to stand final above the floor", |e| {
        e.distributable_excess().is_some_and(|excess| excess > Fixed::from_int(500_000))
    })
    .await;
    let close = paid(&next_epoch(&mut node, &chain).await);
    let [award] = close[..] else { panic!("the close pays the carried award first: {close:?}") };
    assert_eq!((award.award, award.amount.0 + award.owed.0), (0, Severity::High.amount().0));
    assert_eq!(node.query(move |e| e.staked(member)).await, more, "the stake mirrored once");
    println!(
        "PASS: the trader claimed the rest, {rest}, the new stake mirrored once, and a close paid the carried award \
         {} of its whole {}",
        award.amount,
        Severity::High.amount()
    );

    // 7. The launch log replays, and checks against the alpha's; a carry file with one entitlement raised by one
    //    unit is refused.
    let launch_dir = node.dir.clone();
    node.stop().await;
    let replayed = replay_path(&launch_dir).expect("the launch log replays");
    assert_eq!(replayed.engine.carried_from(), Some(file.source));
    let from = crate::carry::check_carried(&launch_dir, &alpha_dir).expect("the launch log carried the alpha's state");
    assert_eq!(from.source, file.source);
    let mut raised = file.clone();
    let entries = raised.parts.iter_mut().find_map(|part| match part {
        CarryPart::Entitlements { entries } => Some(entries),
        _ => None,
    });
    entries.expect("the alpha carries entitlements")[0].amount.0 += 1;
    // With its totals raised to match, so the file reads and the engine's proof of the entitlement refuses it.
    raised.totals.entitled.0 += 1;
    let raised_path = dir.join("raised.json");
    crate::carry::write(&raised, &raised_path).unwrap();
    chain.carry = Some((alpha_vault, raised_path, at));
    let fresh = scratch_dir("vault-move-raised");
    let budget = Some(emission_budget(&chain).await);
    // At the time the launch node started, before its staking start, so the raised entitlement alone refuses.
    let started_at = crate::clock::test_clock::TestClock::at(at);
    let refused =
        crate::carry::boot::carried_parts(&carry_config(&chain, &fresh), budget, &started_at).await.unwrap_err();
    assert!(format!("{refused:#}").contains("does not hold"), "{refused:#}");
    println!("PASS: the launch log replays and checks against the alpha's, and a raised entitlement is refused");
}

/// The awards a close's events paid.
fn paid(events: &[Event]) -> Vec<AwardPaid> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::AwardPaid(paid) => Some(*paid),
            _ => None,
        })
        .collect()
}
