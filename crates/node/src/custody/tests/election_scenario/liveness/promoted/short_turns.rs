//! Five-minute turns on real nodes (#850), in a `short-turns` build, which `scripts/dev-short-turns.sh`
//! runs. On Anvil, A, B and C register with standby keys sealed for their releases and vouched for by
//! the vault's signer, a close adopts them, and the turn in effect now is drawn for A; the chain then
//! runs on true time with a block a second, and A, B and C run the node binary's own start as child
//! processes, A holding the turn and B and C following it. Once A's log has named the set a turn ahead,
//! the next two turns are drawn for B and then C at their draw deadlines. It checks that at each of the
//! two boundaries the turn's member takes the keys by itself, logs `TurnTaken` and its admin key signs
//! while the previous turn's is refused; that with C, the incumbent, killed once its head latched its
//! turn, the callers promote the next member, whose node serves from its copy under its standby key,
//! whose admin key signs and whose first checkpoint lands; and that its log replays across the change of
//! signer.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{Context, ensure};
use engine_core::auth::Operator;
use engine_core::command::{AdminCommand, AdminRequest};
use engine_core::event::Event;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde_json::Value;
use tokio::time::sleep;

use super::nodes::{Deployment, Seat};
use super::{adopt, latestSeqCall, now_s, until};
use crate::checkpoints::scenario::deploy;
use crate::clock::now_ms;
use crate::custody::tests::election_scenario::chain::Chain;
use crate::custody::tests::election_scenario::liveness::{
    DIGEST_C, KEYS, RULE, checkpointSignerCall, drawCall, etch_arb_sys, latchesCall, receiveSeedCall, relayAliasCall,
    seedEpochCall, signerEpochCall, turnOfCall,
};
use crate::custody::tests::election_scenario::{DEPLOYER_DIGEST, DIGEST_A, DIGEST_B, address};
use crate::log::CheckpointDomain;
use crate::seed_relay::schedule::{BOUNDARY_OFFSET, TURN_SECS};
use crate::sequencer::replay_path_with_attested;
use crate::sequencer::withdrawal_scenario::client::Client;
use crate::sequencer::withdrawal_scenario::services::Services;
use crate::tee::EnclaveKey;
use crate::tee::keys::KeySet;
use crate::tee::sealing::{KeyStore, MockSealer};

alloy::sol! {
    function memberAt(uint32 setId, bytes32 seed, uint64 turn, uint16 position) external view returns (address);
    function checkpointSignerAt(uint64 seq) external view returns (address);
}

/// The switch budget the schedule asserts, as the other three-miss scenarios': short enough that its
/// take window, at the bounds' shortest interval, spans less than a five-minute turn, and that the rule's
/// misses leave a skip's member a take before the untaken-turn promotion.
const SWITCH_BUDGET_SECS: u64 = 5;
/// The least time left before a deadline that the scenario still relays or draws for.
const LEAD_SECS: u64 = 60;
/// How long a member gets past its boundary to take the keys and log `TurnTaken`.
const TAKE_WAIT: Duration = Duration::from_secs(120);
/// How long a node that has just taken the keys gets to open sequencing to an admin request.
const ADMIN_WAIT: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Anvil, compiled contracts and a short-turns build; scripts/dev-short-turns.sh runs it"]
async fn dev_scenario_five_minute_turns_hand_over_and_promote() {
    run().await.unwrap();
}

/// The scenario: see the module docs.
/// Error cases:
/// - any chain, node or client step, or a behavioural check, fails
async fn run() -> anyhow::Result<()> {
    ensure!(TURN_SECS == 300, "a short-turns build has five-minute turns, not {TURN_SECS} s");
    let signing = Arc::new(EnclaveKey::generate());
    let deployer = SigningKey::from_slice(&[0x41; 32])?;
    let mut chain =
        Chain::start_with_trailing_finality(Address::from(signing.address()), address(&deployer), DEPLOYER_DIGEST)
            .await;
    chain.fund(None, U256::from(1_000_000_000_000u64)).await;
    etch_arb_sys(&chain).await;
    let receiver = deploy(
        &chain.provider,
        chain.accounts[0],
        "SeedReceiver.sol/SeedReceiver.json",
        &(chain.accounts[9], 1_606_824_023u64, TURN_SECS).abi_encode_params(),
    )
    .await;
    let (schedule, log_heads) = chain.deploy_schedule_with(receiver, RULE, SWITCH_BUDGET_SECS).await;
    chain.schedule = Some(schedule);
    let domain = engine_core::eip712::domain(31337, AccountId(chain.vault.into()));
    let services = Services::start(&chain.url, Arc::default(), domain).await?;
    let staking_start_ms = now_ms() + 300_000;
    let mut seats = Vec::new();
    for (i, (name, digest)) in [("A", DIGEST_A), ("B", DIGEST_B), ("C", DIGEST_C)].into_iter().enumerate() {
        // Each member posts through relayers of its own, as every member's node does.
        let caller = chain.impersonated(&format!("short-turns-caller-{name}")).await;
        let checkpoints = chain.impersonated(&format!("short-turns-checkpoints-{name}")).await;
        let heads = chain.impersonated(&format!("short-turns-heads-{name}")).await;
        let relayers = Some((checkpoints, heads));
        let deployment =
            Deployment { chain: &chain, services: &services, schedule, log_heads, staking_start_ms, relayers };
        seats.push(Seat::new(name, (chain.accounts[2 + i], KEYS[i], digest), caller, &deployment)?);
    }
    adopt(&mut chain, &seats, schedule, &signing).await;

    // The turn in effect now, drawn for A at its deadline, with a boundary at least a minute off; then
    // the chain runs on true time.
    let mut turn = (now_s() - BOUNDARY_OFFSET) / TURN_SECS;
    if boundary(turn + 1) < now_s() + LEAD_SECS {
        sleep(Duration::from_secs(boundary(turn + 1) + 5 - now_s())).await;
        turn += 1;
    }
    let seed = seed_picking(&chain, schedule, turn, seats[0].candidate).await?;
    chain.mine_at(boundary(turn) - TURN_SECS - 10).await;
    relay(&chain, receiver, turn, seed).await;
    chain.mine_at(boundary(turn) - TURN_SECS).await;
    chain.send(chain.accounts[1], schedule, drawCall { turn }.abi_encode()).await;
    let drawn = chain.view(schedule, turnOfCall { turn }).await;
    ensure!(drawn.candidate == seats[0].candidate, "turn {turn} names {}", drawn.candidate);
    chain.mine_at(now_s()).await;
    let miner = chain.provider.clone();
    let mining = tokio::spawn(async move {
        loop {
            let _ = miner.request::<_, String>("evm_mine", ()).await;
            sleep(Duration::from_secs(1)).await;
        }
    });
    let sealer = MockSealer { launch_digest: DIGEST_A };
    let keys = KeySet::for_key(&signing)?;
    KeyStore::create(&seats[0].dir.join("keys"), &sealer, seats[0].config_hash, drawn.generation, keys)?;
    let vault = chain.vault;
    for seat in &mut seats {
        seat.start(&services)?;
    }
    seats[0].serving(vault).await?;
    println!("PASS: turn {turn} names A, which serves; B and C follow it");

    // The first turn whose key the log has named a turn ahead, and the one after it, drawn for B and C.
    let a = seats[0].client(vault)?;
    let logged_ms = until("A's log to name the set", Duration::from_secs(120), &mut seats, async || {
        a.get("/v1/operator").await.ok()?["rotation_set"]["logged_at"].as_i64()
    })
    .await?;
    let noticed = u64::try_from(logged_ms / 1000)? + TURN_SECS;
    let first = ((noticed.max(now_s() + TURN_SECS + LEAD_SECS) - BOUNDARY_OFFSET).div_ceil(TURN_SECS)).max(turn + 1);
    // Both seeds are relayed now, before the first deadline: the second's is the first's boundary.
    for (next, holder) in [(first, 1), (first + 1, 2)] {
        let seed = seed_picking(&chain, schedule, next, seats[holder].candidate).await?;
        relay(&chain, receiver, next, seed).await;
    }
    let mut previous = (0, seats[0].admin.clone());
    for (next, holder) in [(first, 1), (first + 1, 2)] {
        wait_chain(&chain, boundary(next) - TURN_SECS).await?;
        chain.send(chain.accounts[1], schedule, drawCall { turn: next }.abi_encode()).await;
        let drawn = chain.view(schedule, turnOfCall { turn: next }).await;
        ensure!(drawn.candidate == seats[holder].candidate, "turn {next} names {}", drawn.candidate);
        println!("turn {next} drawn for {}, from {} s on", seats[holder].name, boundary(next));

        // At the boundary the member takes the keys by itself: its log takes the turn and its key signs.
        wait_chain(&chain, boundary(next)).await?;
        let client = seats[holder].client(vault)?;
        let key = engine_core::eip712::address(seats[holder].admin.verifying_key());
        let operator = until("the member's TurnTaken", TAKE_WAIT, &mut seats, async || {
            let view = client.get("/v1/operator").await.ok()?;
            let taken = view["turn"]["turn"].as_u64() == Some(next) && view["turn"]["taken"].as_bool() == Some(true);
            taken.then_some(view["operator"].clone())
        })
        .await?;
        ensure!(operator == serde_json::json!(key), "turn {next}'s operator is {operator}");
        ensure!(signs(&client, &seats[holder].admin, vault, true).await?, "turn {next}'s key is refused");
        ensure!(!signs(&client, &previous.1, vault, false).await?, "the previous turn's key signs");
        println!(
            "PASS: at turn {next}'s boundary {} took the keys by itself, logged TurnTaken, and its key signs while {}'s \
             is refused",
            seats[holder].name, seats[previous.0].name
        );
        previous = (holder, seats[holder].admin.clone());
    }

    // C dies once its head latched its turn. The callers promote the next member.
    let last = first + 1;
    let position = chain.view(schedule, turnOfCall { turn: last }).await.position;
    let c_candidate = seats[2].candidate;
    until("C's head to latch its turn", Duration::from_secs(120), &mut seats, async || {
        let latch = chain.view(log_heads, latchesCall { turn: last, position }).await;
        (latch.member == c_candidate).then_some(())
    })
    .await?;
    seats[2].kill().await?;
    let window = Duration::from_secs(RULE.0 * (u64::from(RULE.2) + 1) + RULE.1);
    let epoch = until("a promotion", window + Duration::from_secs(120), &mut seats[..2], async || {
        let epoch = chain.view(vault, signerEpochCall {}).await;
        (epoch > 1).then_some(epoch)
    })
    .await?;
    let signer = AccountId(chain.view(vault, checkpointSignerCall {}).await.into());
    let p = seats.iter().position(|seat| seat.standby == signer).context("the vault names no member's standby key")?;
    println!("PASS: with C killed, the callers promoted {}'s standby key in signer epoch {epoch}", seats[p].name);
    let promoted = seats[p].client(vault)?;
    let candidate = serde_json::json!(AccountId(seats[p].candidate.into()));
    until("the promoted member's log to apply the promotion", TAKE_WAIT, &mut seats, async || {
        let view = promoted.get("/v1/operator").await.ok()?;
        (view["turn"]["reason"] == "promotion" && view["turn"]["candidate"] == candidate).then_some(())
    })
    .await?;
    ensure!(signs(&promoted, &seats[p].admin, vault, false).await?, "the promoted member's key is refused");
    println!("PASS: {} serves from its copy under its standby key, and its admin key signs", seats[p].name);
    until("the promoted member's first checkpoint", Duration::from_secs(120), &mut seats, async || {
        let seq = chain.view(vault, latestSeqCall {}).await;
        let by = chain.view(vault, checkpointSignerAtCall { seq }).await;
        (AccountId(by.into()) == signer).then_some(seq)
    })
    .await?;
    println!("PASS: {}'s first checkpoint under its standby key landed", seats[p].name);

    // The promoted member's log replays across the change of signer.
    for seat in &mut seats {
        if seat.name != "C" {
            seat.stop().await?;
        }
    }
    mining.abort();
    let signers: BTreeSet<AccountId> = [AccountId(signing.address()), signer].into();
    let checkpoints = CheckpointDomain { chain_id: 31337, vault: AccountId(vault.into()) };
    let replayed = replay_path_with_attested(&seats[p].log_dir(), Some(checkpoints), signers.clone())?;
    ensure!(replayed.checkpoint_signers == signers, "the replay names {:?}", replayed.checkpoint_signers);
    println!("PASS: {}'s log replays across the change of signer, each checkpoint by an attested key", seats[p].name);
    Ok(())
}

/// The boundary of `turn`, in seconds.
fn boundary(turn: u64) -> u64 {
    turn * TURN_SECS + BOUNDARY_OFFSET
}

/// A seed whose draw of `turn` from the set in effect picks `candidate` first.
/// Error cases:
/// - no seed of the first hundred picks it
async fn seed_picking(chain: &Chain, schedule: Address, turn: u64, candidate: Address) -> anyhow::Result<B256> {
    for byte in 1..=100u8 {
        let seed = B256::repeat_byte(byte);
        if chain.view(schedule, memberAtCall { setId: 1, seed, turn, position: 0 }).await == candidate {
            return Ok(seed);
        }
    }
    anyhow::bail!("no seed draws turn {turn} for {candidate}")
}

/// Relays `seed` for `turn` from the relay's alias, before its draw deadline.
async fn relay(chain: &Chain, receiver: Address, turn: u64, seed: B256) {
    let alias = chain.view(receiver, relayAliasCall {}).await;
    let epoch = chain.view(receiver, seedEpochCall { turn }).await;
    chain.impersonate(alias).await;
    chain.send(alias, receiver, receiveSeedCall { turn, epoch, seed }.abi_encode()).await;
}

/// Waits until the chain's latest block is at or past `at`, in seconds.
/// Error cases:
/// - it is not within the time to `at` and a minute more
async fn wait_chain(chain: &Chain, at: u64) -> anyhow::Result<()> {
    let within = Duration::from_secs(at.saturating_sub(now_s()) + 60);
    tokio::time::timeout(within, async {
        while chain.latest_timestamp().await < at {
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .with_context(|| format!("the chain did not reach {at}"))
}

/// Whether `client`'s node applies setting every market reduce-only, or lifting it, under `key`'s
/// signature: `true` once it applies, `false` once the node refuses the signature (401). A node that
/// has just taken the keys answers 503 until its sequencing opens, which is retried.
/// Error cases:
/// - neither answer comes within `ADMIN_WAIT`, or the node answers anything else
async fn signs(client: &Client, key: &SigningKey, vault: Address, reduce_only: bool) -> anyhow::Result<bool> {
    let answered = tokio::time::timeout(ADMIN_WAIT, async {
        loop {
            match admin(client, key, vault, reduce_only).await {
                Ok(events) if events.iter().any(|event| matches!(event, Event::AdminApplied)) => return Ok(true),
                Ok(events) => anyhow::bail!("the admin request was answered {events:?}"),
                Err(e) if e.to_string().contains("401") => return Ok(false),
                Err(e) if e.to_string().contains("503") => sleep(Duration::from_millis(500)).await,
                Err(e) => return Err(e),
            }
        }
    });
    answered.await.context("the node neither applied nor refused the admin request")?
}

/// Sets every market reduce-only, or lifts it, under `key`'s signature through `client`'s admin route.
/// Error cases:
/// - the nonce cannot be read, or the node refuses the request
async fn admin(client: &Client, key: &SigningKey, vault: Address, reduce_only: bool) -> anyhow::Result<Vec<Event>> {
    let address = engine_core::eip712::address(key.verifying_key());
    let next: Value = client.get(&format!("/v1/accounts/{address}/nonce")).await?;
    let nonce = next["next_nonce"].as_u64().context("the operator's next nonce")?;
    let operator = Operator { address, chain_id: 31337, vault: AccountId(vault.into()) };
    let command = AdminCommand::SetReduceOnly { market: None, reduce_only };
    let request: AdminRequest = crate::admin::sign(key, &operator, command, nonce, now_ms() + 60_000);
    client.post("/v1/admin", &request).await
}
