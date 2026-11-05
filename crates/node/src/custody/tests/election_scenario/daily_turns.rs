//! The daily rotation dev scenario, from `V51`, which `scripts/dev-daily-rotation.sh` runs: on a
//! custody chain whose finality trails its head by two blocks, A (the operator in effect's
//! candidate), B and C register with their handover endpoints and delegate 50, 30 and 20 thousand
//! MONKEY, and a close adopts them as a rotation set. Eight days on, five daily turns are drawn
//! from RANDAO seeds the node's own relayer relays from a stand-in Ethereum, each at its draw
//! deadline. At each boundary the turn's key signs the admin commands and the previous turn's key is
//! refused; a member taking the turn from another takes the keys and the log by the existing
//! handover, from the source the finalised schedule names, while the vault's finalised view still
//! names the incumbent's release; the member's first command is `TurnTaken`, from which the time in
//! office is its; `/v1/operator` shows the sets, the turns and their history; and the last member's
//! log replays to its roots.

mod checks;
mod seat;
mod seeds;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use axum::http::StatusCode;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::turns::SetMember;
use engine_core::types::{AccountId, Millis};
use serde_json::{Value, json};

use super::chain::{Chain, delegateCall, setRotationCall};
use super::node::Node;
use super::{DIGEST_A, DIGEST_B, FINALITY_MARGIN_SECS, T0_SECS, address, monkey};
use crate::checkpoints::scenario::deploy;
use crate::checkpoints::{CheckpointChain, View};
use crate::clock::test_clock::TestClock;
use crate::sequencer::replay_path;
use crate::tee::keys::KeySet;
use checks::{credited, logged_commands, office, operator_view, turn_taken};
use seat::{Holder, Member};
use seeds::Seeds;

sol! {
    struct Turn { address candidate; uint16 position; uint8 reason; uint32 setId; uint64 generation; uint64 prior; bytes32 seed; address source; }
    function setVault(address vault_) external;
    function installOperator(address candidate, address adminKey, bytes calldata launchDigest, bytes32 configHash) external;
    function setRegistry(address registry_) external;
    function setSchedule(address schedule_) external;
    function draw(uint64 turn) external returns (address);
    function turnOf(uint64 turn) external view returns (Turn);
    function currentRelease() external view returns (bytes32);
    function releaseGeneration() external view returns (uint64);
}

/// The members' admin keys: A's is the dev operator key `config/dev.toml` names, the operator in
/// effect before any turn.
const KEYS: [[u8; 32]; 3] = [[0x42; 32], [0x55; 32], [0x56; 32]];
const DIGEST_C: [u8; 48] = [0xc1; 48];
/// Each member's stake, all of it delegated to itself, in whole MONKEY, and its self-bond.
const STAKES: [u64; 3] = [50_000, 30_000, 20_000];
const BOND: u64 = 2_000;
/// The close that adopts the rotation set, and when the set takes effect: seven days and the
/// finality margin later.
const SET_CLOSE_SECS: u64 = T0_SECS + 3 * 3_600;
const SET_EFFECTIVE_SECS: u64 = SET_CLOSE_SECS + 7 * DAY_SECS + FINALITY_MARGIN_SECS;
/// The first turn whose draw deadline finds the set in effect: Wed 28 Oct 2026, from 22:30 UTC.
const FIRST_TURN: u64 = 20_754;
const TURNS: u64 = 5;
const DAY_SECS: u64 = 86_400;

#[tokio::test]
#[ignore = "requires Anvil, Python 3, and compiled contracts; scripts/dev-daily-rotation.sh runs it"]
async fn dev_scenario_each_daily_turn_takes_the_keys_at_its_boundary() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
    );
    let keys = KeySet::generate().unwrap();
    let enclave = Address::from(keys.enclave_key().unwrap().address());
    let a = address(&k256::ecdsa::SigningKey::from_slice(&KEYS[0]).unwrap());
    let mut chain = Chain::start_with_trailing_finality(enclave, a, DIGEST_A).await;
    let members = [
        Member::new("A", a, KEYS[0], DIGEST_A),
        Member::new("B", chain.accounts[3], KEYS[1], DIGEST_B),
        Member::new("C", chain.accounts[4], KEYS[2], DIGEST_C),
    ];
    let seeds = Seeds::start(&chain).await;
    let schedule_args = (chain.registry, seeds.receiver).abi_encode_params();
    let schedule =
        deploy(&chain.provider, chain.accounts[0], "OperatorSchedule.sol/OperatorSchedule.json", &schedule_args).await;
    chain.schedule = Some(schedule);

    // 1. A, B and C stake and register with their handover endpoints; the timelock names the vault,
    //    the registry and the schedule to each other, approves the three releases, and turns rotation
    //    on; each member delegates its whole stake to itself.
    chain.impersonate(a).await;
    for (member, stake) in members.iter().zip(STAKES) {
        chain.stake(member.candidate, monkey(stake)).await;
        chain.register_serving(member.candidate, &member.key, monkey(BOND), member.digest, &member.endpoint).await;
    }
    let mut calls = vec![
        (chain.registry, setVaultCall { vault_: chain.vault }.abi_encode()),
        (chain.vault, setRegistryCall { registry_: chain.registry }.abi_encode()),
        (chain.registry, setScheduleCall { schedule_: schedule }.abi_encode()),
    ];
    calls.extend(members.iter().map(|member| (chain.registry, chain.approval(member.candidate, member.digest))));
    calls.push((chain.registry, setRotationCall { enabled: true }.abi_encode()));
    chain.govern_each(calls).await;
    for (member, stake) in members.iter().zip(STAKES) {
        let delegate = delegateCall { candidate: member.candidate, amount: monkey(stake) };
        chain.send(member.candidate, chain.staking, delegate.abi_encode()).await;
    }
    chain.finalize().await;
    assert_eq!(chain.view(chain.vault, currentReleaseCall {}).await.0, members[0].release().id());

    // A runs the operator's node with the vault's signing key, sealed for its release.
    let clock = Arc::new(TestClock::at(T0_SECS as Millis * 1000));
    let generation = chain.view(chain.vault, releaseGenerationCall {}).await;
    let mut holder = Holder::start(&chain, &members, 0, members[0].seal(keys, generation), clock.clone()).await;
    holder.node.advance_to(chain.latest_timestamp().await as Millis * 1000).await;
    let c = AccountId(members[2].candidate.into());
    let staked = TokenUnits(monkey(STAKES[2]).to());
    holder
        .node
        .wait("rotation and the stakes to mirror", move |e| {
            e.operator_election().rotation_enabled && e.staked(c) == staked
        })
        .await;

    // 2. The close adopts A, B and C, weighted 50:30:20, to take effect seven days and the finality
    //    margin later; the engine logs it at its close.
    holder.node.advance_to((SET_CLOSE_SECS as Millis + 60) * 1000).await;
    chain.close_epoch_at(SET_CLOSE_SECS).await;
    chain.finalize().await;
    holder.node.wait("the rotation set to mirror", |e| e.daily_turns().sets.contains_key(&1)).await;
    let set_members: Vec<SetMember> = members
        .iter()
        .zip(STAKES)
        .map(|(member, stake)| SetMember {
            candidate: AccountId(member.candidate.into()),
            admin_key: member.operator(),
            weight: TokenUnits(monkey(stake).to()),
        })
        .collect();
    let set = holder.node.query(|e| e.daily_turns().sets[&1].clone()).await;
    assert_eq!((set.effective_at, &set.members), (SET_EFFECTIVE_SECS as Millis * 1000, &set_members));
    let view = operator_view(&holder.node).await;
    assert_eq!(view["rotation_set"], Value::Null, "{view}");
    assert_eq!(view["next_rotation_set"]["members"], serde_json::to_value(&set_members).unwrap(), "{view}");
    println!("PASS: a close adopted A, B and C at 50:30:20, taking effect seven days and the finality margin later");

    // 3. Eight days on the set is in effect, and the relayer relays the first two turns' seeds.
    let first_deadline = boundary_secs(FIRST_TURN) - DAY_SECS;
    let first_after_effect = (first_deadline - DAY_SECS..=first_deadline).contains(&SET_EFFECTIVE_SECS);
    assert!(first_after_effect, "the first turn's deadline is the first to find the set in effect");
    let relay_at = first_deadline - 3_600;
    holder.node.advance_to(relay_at as Millis * 1000).await;
    let [first_seed, _] = seeds.relay(&chain, relay_at, [FIRST_TURN, FIRST_TURN + 1]).await;
    holder.node.submit(Command::Tick).await;
    let view = operator_view(&holder.node).await;
    assert_eq!((&view["rotation_set"]["set"], &view["next_rotation_set"]), (&json!(1), &Value::Null), "{view}");
    println!("PASS: eight days on, the set is in effect and the relayer has relayed the first turns' seeds");

    // The first turn is drawn at its deadline; a second close, with the set in effect, adopts one
    // that waits its seven days.
    holder.node.advance_to((first_deadline as Millis + 60) * 1000).await;
    let (mut next, mut next_generation) =
        draw(&chain, &holder.node, &members, FIRST_TURN, first_deadline, first_seed).await;
    let close_at = first_deadline + 600;
    holder.node.advance_to((close_at as Millis + 60) * 1000).await;
    chain.close_epoch_at(close_at).await;
    chain.finalize().await;
    holder.node.wait("the second rotation set to mirror", |e| e.daily_turns().sets.contains_key(&2)).await;
    let waiting = (close_at + 7 * DAY_SECS + FINALITY_MARGIN_SECS) as Millis * 1000;
    let view = operator_view(&holder.node).await;
    assert_eq!(
        (&view["next_rotation_set"]["set"], &view["next_rotation_set"]["effective_at"]),
        (&json!(2), &json!(waiting))
    );

    // 4. Five turns, each crossing its boundary.
    let mut generations = vec![next_generation];
    let mut crossings = Vec::new();
    for k in 0..TURNS {
        let turn = FIRST_TURN + k;
        let (member, generation) = (next, next_generation);
        let (start_secs, start) = (boundary_secs(turn), boundary_secs(turn) as Millis * 1000);
        let named = |index: usize| members[index].name;

        // An hour before the boundary the relayer relays the next two turns' seeds; ten minutes
        // before, a tick catches up the day's financing rollover, and the time in office stands.
        let relay_at = start_secs - 3_600;
        holder.node.advance_to(relay_at as Millis * 1000).await;
        let [next_seed, _] = seeds.relay(&chain, relay_at, [turn + 1, turn + 2]).await;
        holder.node.advance_to(start - 600_000).await;
        holder.node.submit(Command::Tick).await;
        let (before, before_at) = office(&holder.node).await;
        let incumbent = holder.member;

        // A minute past the boundary one block past it is mined, and the incumbent's next command
        // hands the operator key to the turn's. The incumbent held the office until its turn ended,
        // or, before any turn, until this command.
        holder.node.advance_to(start + 60_000).await;
        chain.mine_at(start_secs + 30).await;
        holder.node.submit(Command::Tick).await;
        let (at_boundary, _) = office(&holder.node).await;
        let until = if k == 0 { start + 60_000 } else { start };
        let incumbent_account = AccountId(members[incumbent].candidate.into());
        assert_eq!(
            at_boundary,
            credited(before, incumbent_account, until - before_at),
            "turn {turn}: office at the boundary"
        );

        // The member takes the turn: by a handover from the incumbent, while the vault's finalised
        // view, two blocks behind, still names the incumbent's release; or by holding it on.
        let head = if member == incumbent {
            assert_eq!(chain.view(chain.vault, releaseGenerationCall {}).await, generation);
            println!(
                "PASS: turn {turn}: {} holds the turn on, logs TurnTaken at the boundary and keeps its generation",
                named(member)
            );
            None
        } else {
            let taken = holder.hand_over(&chain, &members[member]).await;
            let finalized = chain.view_at(chain.vault, currentReleaseCall {}, "finalized").await;
            assert_eq!(
                finalized.0,
                members[incumbent].release().id(),
                "turn {turn}: the finalised vault names the incumbent"
            );
            assert!(chain.timestamp_at("finalized").await < start_secs, "turn {turn}: finality passed the boundary");
            assert_eq!(chain.view(chain.vault, currentReleaseCall {}).await.0, members[member].release().id());
            let source = taken.scheduled.source.as_ref().map(|source| source.candidate);
            assert_eq!(
                (taken.scheduled.turn, source),
                (turn, Some(incumbent_account)),
                "turn {turn}: the handover source"
            );
            assert_eq!((taken.store.release(), taken.store.generation()), (members[member].release(), generation));
            // A minute of the turn passes with no node sequencing, then the member's node starts
            // from the log it took and sequences its first command.
            clock.advance(Duration::from_secs(60));
            holder = Holder::start(&chain, &members, member, taken.store, clock.clone()).await;
            holder.node.submit(Command::Tick).await;
            assert_eq!(
                office(&holder.node).await.0,
                at_boundary,
                "turn {turn}: no one held the office before TurnTaken"
            );
            println!(
                "PASS: turn {turn}: {} took the keys and the log from {} at the boundary, while the finalised vault still named {}'s release",
                named(member),
                named(incumbent),
                named(incumbent)
            );
            Some(taken.head)
        };

        // The turn is taken under its member's release, and its key alone signs admin commands.
        let taken = holder.node.query(|e| e.daily_turns().taken).await.expect("the turn is taken");
        assert_eq!((taken.turn, taken.release), (turn, B256::from(members[member].release().id())));
        let reduce_only = AdminCommand::SetReduceOnly { market: None, reduce_only: k % 2 == 0 };
        let applied = (StatusCode::OK, json!({ "type": "admin_applied" }));
        assert_eq!(holder.node.admin(&members[member].key, reduce_only.clone()).await, applied, "turn {turn}");
        if member != incumbent {
            let (status, body) = holder.node.admin(&members[incumbent].key, reduce_only).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "turn {turn}: the previous turn's key: {body}");
            println!(
                "PASS: turn {turn}: {}'s key applies an admin command and {}'s is refused",
                named(member),
                named(incumbent)
            );
        } else {
            println!("PASS: turn {turn}: {}'s key applies an admin command", named(member));
        }
        let view = operator_view(&holder.node).await;
        let record = turn_record(&view, turn);
        assert_eq!((&view["turn"]["turn"], &view["turn"]["taken"]), (&json!(turn), &json!(true)), "{view}");
        assert_eq!(view["operator"], json!(members[member].operator()), "{view}");
        let taken_ms = record["taken_ms"].as_i64().expect("the record has its TurnTaken");

        // The next turn is drawn at its deadline, this boundary, once the handover is done.
        if k + 1 < TURNS {
            (next, next_generation) = draw(&chain, &holder.node, &members, turn + 1, start_secs + 180, next_seed).await;
            if next == member {
                assert_eq!(
                    next_generation,
                    generation,
                    "turn {}: the member of the turn before keeps its generation",
                    turn + 1
                );
            } else {
                assert!(
                    generations.iter().all(|&earlier| next_generation > earlier),
                    "turn {}: a new generation",
                    turn + 1
                );
            }
            generations.push(next_generation);
            let view = operator_view(&holder.node).await;
            assert_eq!((&view["next_turn"]["turn"], &view["next_turn"]["taken"]), (&json!(turn + 1), &json!(false)));
            assert_eq!(view["next_turn"]["candidate"], json!(AccountId(members[next].candidate.into())));
        }

        // An hour on, the hour since the TurnTaken is the member's alone.
        holder.node.advance_to(taken_ms + 3_600_000).await;
        holder.node.submit(Command::Tick).await;
        let member_account = AccountId(members[member].candidate.into());
        assert_eq!(office(&holder.node).await.0, credited(at_boundary, member_account, 3_600_000), "turn {turn}");
        println!(
            "PASS: turn {turn}: the time in office follows TurnTaken: {} to the boundary, then {} from its TurnTaken",
            named(incumbent),
            named(member)
        );
        crossings.push((turn, start, head, taken_ms));
    }

    // 5. The operator view shows the set in effect, the one waiting, today's turn, and every
    //    assignment with its TurnTaken.
    let view = operator_view(&holder.node).await;
    assert_eq!((&view["rotation_set"]["set"], &view["next_rotation_set"]["set"]), (&json!(1), &json!(2)), "{view}");
    assert_eq!((&view["turn"]["turn"], &view["next_turn"]), (&json!(FIRST_TURN + TURNS - 1), &Value::Null), "{view}");
    let records = view["turns"]["records"].as_array().expect("the turn history");
    let listed: Vec<u64> = records.iter().map(|record| record["turn"].as_u64().unwrap()).collect();
    assert_eq!(listed, (FIRST_TURN..FIRST_TURN + TURNS).rev().collect::<Vec<_>>(), "{view}");
    println!(
        "PASS: /v1/operator shows the set in effect, the waiting set, today's turn, and all five assignments with their TurnTaken"
    );

    // 6. The last member's log replays to its roots across every handover, to the same turns,
    //    operator and time in office; each TurnTaken is its member's first command in its turn.
    let live = holder.node.query(|e| (e.daily_turns().clone(), e.operator().copied(), office_of(e))).await;
    let last = holder.member;
    let dir = holder.stop().await;
    let replayed = replay_path(&dir).expect("the log replays to its roots");
    let engine = &replayed.engine;
    assert_eq!((engine.daily_turns().clone(), engine.operator().copied(), office_of(engine)), live);
    let commands = logged_commands(&dir);
    let handovers = crossings.iter().filter(|(_, _, head, _)| head.is_some()).count();
    for (turn, start, head, taken_ms) in crossings {
        let (seq, ts) = turn_taken(&commands, turn);
        let record = turn_record(&view, turn);
        assert_eq!((record["taken_seq"].as_u64(), ts), (Some(seq), taken_ms), "turn {turn}");
        assert!(ts >= start, "turn {turn} was taken before its boundary");
        match head {
            Some(head) => assert_eq!(seq, head + 1, "turn {turn}: TurnTaken follows the handed-over head"),
            None => assert!(commands[seq as usize - 1].ts < start, "turn {turn}: a command preceded TurnTaken"),
        }
    }
    println!("PASS: the log replays to its roots across {handovers} handovers, and each TurnTaken opens its turn");

    // 7. The governance fallback ends the turns, even when it installs the last turn's own candidate.
    installation_ends_the_turns(&mut chain, &members[last]).await;
}

/// Turns rotation off, waits out the last drawn turn, and has the timelock install `member`, the last
/// turn's candidate, under another release: once finalised the schedule names no turn at any time,
/// so neither side of a handover, nor a start, takes the old turn's release and generation.
async fn installation_ends_the_turns(chain: &mut Chain, member: &Member) {
    let vault = crate::handover::vault_reader(&member.config(chain)).unwrap().expect("[checkpoints] reads the vault");
    chain.govern_each(vec![(chain.registry, setRotationCall { enabled: false }.abi_encode())]).await;
    let ended = boundary_secs(FIRST_TURN + TURNS);
    chain.mine_at((ended + 60).max(chain.latest_timestamp().await + 1)).await;
    chain.finalize().await;
    let reached = (chain.timestamp_at("finalized").await as Millis) * 1000;
    let at = vault.observe(View::Finalized, &[]).await.unwrap();
    let over = vault.scheduled_turn(&at, reached).await.unwrap().expect("a turn over still names its member");
    assert_eq!((over.turn, over.candidate), (FIRST_TURN + TURNS - 1, AccountId(member.candidate.into())));
    let install = installOperatorCall {
        candidate: member.candidate,
        adminKey: Address::from(member.operator().0),
        launchDigest: [0xd1u8; 48].to_vec().into(),
        configHash: super::CONFIG_HASH.into(),
    };
    chain.govern_each(vec![(chain.registry, install.abi_encode())]).await;
    chain.finalize().await;
    let at = vault.observe(View::Finalized, &[]).await.unwrap();
    assert_eq!(vault.scheduled_turn(&at, reached).await.unwrap(), None, "the installation ended the last turn");
    assert!(!crate::handover::turns::drawn(&vault, &at).await.unwrap(), "a start finds a turn drawn");
    println!(
        "PASS: installing {}, the last turn's own candidate, under another release ends the turns: the schedule names no one",
        member.name
    );
}

/// Draws `turn` in a block stamped `at`, finalises it, and waits for `node` to mirror its key from
/// the boundary: the drawn member's index and the turn's generation. The draw must use `seed`.
async fn draw(chain: &Chain, node: &Node, members: &[Member], turn: u64, at: u64, seed: B256) -> (usize, u64) {
    let schedule = chain.schedule.expect("the scenario deploys a schedule");
    chain.next_block_at(at).await;
    chain.send(chain.accounts[1], schedule, drawCall { turn }.abi_encode()).await;
    chain.finalize().await;
    let drawn = chain.view(schedule, turnOfCall { turn }).await;
    assert_eq!(drawn.seed, seed, "turn {turn} draws from its relayed seed");
    let member = members.iter().position(|member| member.candidate == drawn.candidate).expect("a member is drawn");
    node.wait(&format!("turn {turn}'s draw to mirror"), move |e| e.daily_turns().assigned.contains_key(&turn)).await;
    let assigned = node.query(move |e| e.daily_turns().assigned[&turn]).await;
    let boundary = boundary_secs(turn) as Millis * 1000;
    let expected = (AccountId(drawn.candidate.into()), members[member].operator(), boundary);
    assert_eq!((assigned.candidate, assigned.admin_key, assigned.applies_at), expected, "turn {turn}");
    println!(
        "PASS: turn {turn} was drawn for {} from its relayed seed at its deadline, and its key mirrored to apply from the boundary",
        members[member].name
    );
    (member, drawn.generation)
}

/// When turn `turn` starts, in seconds.
fn boundary_secs(turn: u64) -> u64 {
    u64::try_from(engine_core::turns::boundary(turn).unwrap() / 1000).unwrap()
}

/// The turn history's record of `turn` in an operator view.
fn turn_record(view: &Value, turn: u64) -> &Value {
    let records = view["turns"]["records"].as_array().expect("the turn history");
    records
        .iter()
        .find(|record| record["turn"] == json!(turn))
        .unwrap_or_else(|| panic!("no record of turn {turn}: {view}"))
}

/// The open epoch's time in office.
fn office_of(engine: &engine_core::Engine) -> Option<BTreeMap<AccountId, Millis>> {
    engine.staking().and_then(|staking| staking.office.as_ref()).map(|office| office.served.clone())
}
