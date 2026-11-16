//! The promotion on real nodes (#595), which `scripts/dev-liveness.sh` runs beside the parts' legs.
//! On Anvil, A, B and C register with standby keys sealed for their releases and vouched for by the
//! vault's signer, a close adopts them, and the turn in effect now is drawn for A; the chain then runs
//! on true time with a block a second. A, B and C run the node binary's own start as child processes:
//! A holds the turn, posting heads at the seqs B and C confirm, and B and C follow it with their member
//! links and liveness callers. Under load, A takes a deposit, fills an order and authorizes a withdrawal
//! it then never sees paid, and is killed. It checks that the callers promote the next member, whose
//! node starts from its copy under its standby key and fills an order within its start budget, printed,
//! of the promotion and within `(heartbeatMisses + 1) x heartbeatInterval` plus that budget of A's
//! last head; that its first checkpoint lands once the promotion is final; that it vouches for its
//! replacement standby key with the signer key it now holds; that it reissues A's
//! authorization under the promotion's epoch, which pays once while A's reverts; and that its log
//! replays across the change of signer.

mod nodes;
#[cfg(feature = "short-turns")]
mod short_turns;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::{SolCall, SolValue};
use anyhow::{Context, ensure};
use engine_core::event::Event;
use engine_core::types::{AccountId, Side};
use serde_json::json;
use tokio::time::{sleep, timeout};

use super::{
    BOUNDARY_OFFSET_SECS, DAY_SECS, DIGEST_C, KEYS, RULE, SET_CLOSE_SECS, STAKES, Turn, checkpointSignerCall, drawCall,
    etch_arb_sys, headsCall, latchesCall, receiveSeedCall, relayAliasCall, seedEpochCall, setRegistryCall,
    setScheduleCall, setVaultCall, signerEpochCall, turnOfCall,
};
use crate::checkpoints::scenario::deploy;
use crate::clock::now_ms;
use crate::custody::tests::election_scenario::chain::{
    Chain, Parameters, delegateCall, setParametersCall, setRotationCall,
};
use crate::custody::tests::election_scenario::{DEPLOYER_DIGEST, DIGEST_A, DIGEST_B, address, dev_parameters, monkey};
use crate::log::CheckpointDomain;
use crate::sequencer::SignedWithdrawal;
use crate::sequencer::replay_path_with_attested;
use crate::sequencer::withdrawal_scenario::client::User;
use crate::sequencer::withdrawal_scenario::rpc::{Withdrawal, withdrawCall};
use crate::sequencer::withdrawal_scenario::services::Services;
use crate::tee::EnclaveKey;
use crate::tee::keys::KeySet;
use crate::tee::sealing::{KeyStore, MockSealer};
use nodes::{Deployment, Seat};

alloy::sol! {
    function latestSeq() external view returns (uint64);
}

/// How long a promoted member may take from the promotion to its first fill: the start budget the
/// criterion's bound adds to the detection window.
const START_BUDGET: Duration = Duration::from_secs(60);
/// Boundaries closer than this when the leg starts are waited out, so the leg runs inside one turn.
const BOUNDARY_GUARD_SECS: u64 = 20 * 60;
/// The trader's deposit, in USDC base units, and the house's.
const DEPOSIT: u64 = 100_000_000;
const HOUSE: u64 = 1_000_000_000_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Anvil and compiled contracts; scripts/dev-liveness.sh runs it"]
async fn dev_scenario_a_promoted_member_trades_within_its_start_budget() {
    run().await.unwrap();
}

/// The leg: see the module docs.
/// Error cases:
/// - any chain, node or client step, or a behavioural check, fails
async fn run() -> anyhow::Result<()> {
    let signing = Arc::new(EnclaveKey::generate());
    let deployer = k256::ecdsa::SigningKey::from_slice(&[0x41; 32])?;
    // Each block final once two follow it, as the members' reads of the schedule and the vault wait for.
    let operator = address(&deployer);
    let mut chain =
        Chain::start_with_trailing_finality(Address::from(signing.address()), operator, DEPLOYER_DIGEST).await;
    // Deposits before the chain moves on: the vault takes none once its checkpoints are stale.
    let trader = User::new(1)?;
    chain.fund(None, U256::from(HOUSE)).await;
    chain.fund(Some(trader.address()), U256::from(DEPOSIT)).await;
    etch_arb_sys(&chain).await;
    let receiver = deploy(
        &chain.provider,
        chain.accounts[0],
        "SeedReceiver.sol/SeedReceiver.json",
        &(chain.accounts[9], 1_606_824_023u64, DAY_SECS).abi_encode_params(),
    )
    .await;
    let (schedule, log_heads) = chain.deploy_schedule_with(receiver, RULE, 60).await;
    chain.schedule = Some(schedule);
    let domain = engine_core::eip712::domain(31337, AccountId(chain.vault.into()));
    let services = Services::start(&chain.url, Arc::default(), domain).await?;
    // Staking starts as the nodes do, on true time; every copy of the log shares it.
    let staking_start_ms = now_ms() + 300_000;
    let mut seats = Vec::new();
    for (i, (name, digest)) in [("A", DIGEST_A), ("B", DIGEST_B), ("C", DIGEST_C)].into_iter().enumerate() {
        let caller = chain.impersonated(&format!("promoted-caller-{name}")).await;
        let deployment =
            Deployment { chain: &chain, services: &services, schedule, log_heads, staking_start_ms, relayers: None };
        seats.push(Seat::new(name, (chain.accounts[2 + i], KEYS[i], digest), caller, &deployment)?);
    }
    adopt(&mut chain, &seats, schedule, &signing).await;

    // The turn in effect now, drawn for A at its deadline; then the chain runs on true time.
    let now = now_s();
    let mut turn = (now - BOUNDARY_OFFSET_SECS) / DAY_SECS;
    if boundary(turn + 1) < now + BOUNDARY_GUARD_SECS {
        println!("waiting out the boundary of turn {} first", turn + 1);
        sleep(Duration::from_secs(boundary(turn + 1) + 30 - now)).await;
        turn += 1;
    }
    let drawn = draw_for(&chain, (schedule, receiver), turn, seats[0].candidate).await?;
    chain.mine_at(now_s()).await;
    let mut mining = mine(&chain);
    let sealer = MockSealer { launch_digest: DIGEST_A };
    KeyStore::create(
        &seats[0].dir.join("keys"),
        &sealer,
        seats[0].config_hash,
        drawn.generation,
        KeySet::for_key(&signing)?,
    )?;
    println!("PASS: A, B and C hold vouched standby keys; turn {turn} names A at position {}", drawn.position);

    // A holds the turn; B and C follow it. A's heads land at the seqs they confirm and latch the turn.
    let vault = chain.vault;
    for seat in &mut seats {
        seat.start(&services)?;
    }
    seats[0].serving(vault).await?;
    let a_candidate = seats[0].candidate;
    let latched = until("A's heads latch the turn", Duration::from_secs(180), &mut seats, async || {
        let latch = chain.view(log_heads, latchesCall { turn, position: drawn.position }).await;
        (latch.member == a_candidate).then_some(latch.at)
    })
    .await?;
    println!("PASS: A serves, B and C follow it, and A's heads at their confirmed seqs latched the turn at {latched}");

    // Load on A: the trader declares, fills an order and has a withdrawal approved and authorized.
    let a = seats[0].client(vault)?;
    a.declare(&trader).await?;
    a.wait_balance(&trader, "100").await?;
    let filled = a.order(&trader, Side::Buy, "1").await?;
    ensure!(filled.iter().any(|event| matches!(event, Event::Fill(_))), "A filled no order: {filled:?}");
    a.wait_finalised(&trader).await?;
    let mut a_stream = a.stream().await?;
    let approved = timeout(Duration::from_secs(120), async {
        loop {
            let request = a.withdrawal(&trader, "10").await?;
            let events = a.request_withdrawal(&request).await?;
            if events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. })) {
                return anyhow::Ok(());
            }
            sleep(Duration::from_secs(1)).await;
        }
    })
    .await;
    approved.context("A approved no withdrawal")??;
    let issued = a_stream.payout(trader.account, 10).await?;
    let issued_epoch = issued.authorization.as_ref().map(|auth| auth.signer_epoch);
    ensure!(issued_epoch == Some(1), "A authorized under epoch {issued_epoch:?}");
    drop(a_stream);
    println!("PASS: A filled the trader's order and authorized a 10 USDC withdrawal it will not see paid");

    // A's heads follow its log at the seqs B and C confirm.
    let latched_seq = chain.view(log_heads, latchesCall { turn, position: drawn.position }).await;
    let first = chain.view(log_heads, headsCall { epoch: latched_seq.signerEpoch }).await.seq;
    let window = Duration::from_secs(RULE.0 * (u64::from(RULE.2) + 1));
    let advanced = until("A's heads to follow its log", window, &mut seats, async || {
        let head = chain.view(log_heads, headsCall { epoch: latched_seq.signerEpoch }).await;
        (head.seq > first).then_some(head.seq)
    })
    .await?;
    println!("PASS: A's heads moved on from seq {first} to {advanced} as B and C confirmed its log");

    // A dies. The callers witness its missed slots and promote the next member.
    let last_head = chain.view(log_heads, headsCall { epoch: latched_seq.signerEpoch }).await;
    seats[0].kill().await?;
    let killed = Instant::now();
    // From here nothing mines but the test, a block a second, and it looks for the promotion between blocks every
    // tenth of a second: it stops mining at the promotion's block, so finality, two blocks behind the
    // head, cannot reach it. Nothing then mines until the checks below have run, so the vault's
    // finalized view still names A's key, and what a node sends waits in the pool, where they see it.
    mining.abort();
    chain.rpc("evm_setAutomine", json!([false])).await;
    let epoch = timeout(window + Duration::from_secs(120), async {
        loop {
            for seat in &mut seats[1..] {
                seat.check()?;
            }
            for _ in 0..10 {
                let epoch = chain.view(vault, signerEpochCall {}).await;
                if epoch > 1 {
                    return anyhow::Ok(epoch);
                }
                sleep(Duration::from_millis(100)).await;
            }
            chain.rpc("evm_mine", json!([])).await;
        }
    })
    .await
    .context("waiting for a promotion")??;
    let promoted_at = Instant::now();
    // The block the promotion landed in, and finality's room: a block is final once two follow it.
    let head = block_number(&chain).await;
    let mut promotion_block = head;
    while promotion_block > 0
        && chain.view_at(vault, signerEpochCall {}, &format!("{:#x}", promotion_block - 1)).await >= epoch
    {
        promotion_block -= 1;
    }
    ensure!(
        head <= promotion_block + 1,
        "{} blocks followed the promotion: finality could not be held",
        head - promotion_block
    );
    ensure!(
        chain.view_at(vault, signerEpochCall {}, "finalized").await < epoch,
        "the promotion is already final: finality could not be held behind it"
    );
    let held = chain.view(vault, latestSeqCall {}).await;
    let signer = AccountId(chain.view(vault, checkpointSignerCall {}).await.into());
    let p = seats.iter().position(|seat| seat.standby == signer).context("the vault names no member's standby key")?;
    println!(
        "PASS: {:.1} s after A was killed, the callers promoted {}'s standby key in signer epoch {epoch}",
        killed.elapsed().as_secs_f64(),
        seats[p].name
    );

    // The promoted member serves from its copy under its standby key and fills an order.
    let promoted = seats[p].client(vault)?;
    let first_fill = timeout(START_BUDGET, async {
        loop {
            seats[p].check()?;
            if let Ok(events) = promoted.order(&trader, Side::Sell, "1").await
                && events.iter().any(|event| matches!(event, Event::Fill(_)))
            {
                return anyhow::Ok(Instant::now());
            }
            sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .with_context(|| format!("{} filled no order within the start budget", seats[p].name))??;
    let start = first_fill - promoted_at;
    let since_head = Duration::from_secs(now_s().saturating_sub(last_head.postedAt));
    println!(
        "PASS: {}'s first fill came {:.1} s after the promotion (the start budget measured), {:.1} s after A's \
         last head at seq {}, within (misses + 1) x interval + the start budget = {:?}",
        seats[p].name,
        start.as_secs_f64(),
        since_head.as_secs_f64(),
        last_head.seq,
        window + START_BUDGET
    );
    ensure!(since_head <= window + START_BUDGET, "the first fill came {since_head:?} after A's last head");

    // While finality is held it posts no checkpoint, though it trades and its relayer is funded: nothing
    // of its relayer's is in the pool, and with one block after the promotion, which is still not final,
    // nothing of its relayer's lands.
    sleep(Duration::from_secs(20)).await;
    seats[p].check()?;
    let relayer = format!("{:#x}", chain.accounts[8]);
    let sent_by_relayer = async || {
        let pool = chain.rpc_result("txpool_content", json!([])).await;
        let sent = pool["pending"].as_object().into_iter().chain(pool["queued"].as_object()).flatten();
        sent.clone().any(|(from, _)| from.eq_ignore_ascii_case(&relayer))
    };
    ensure!(!sent_by_relayer().await, "the promoted member sent a checkpoint before its promotion was final");
    if head == promotion_block {
        chain.rpc("evm_mine", json!([])).await;
    }
    sleep(Duration::from_secs(8)).await;
    seats[p].check()?;
    ensure!(
        chain.view_at(vault, signerEpochCall {}, "finalized").await < epoch,
        "the promotion became final with one block after it"
    );
    ensure!(chain.view(vault, latestSeqCall {}).await == held, "a checkpoint landed before the promotion was final");
    ensure!(!sent_by_relayer().await, "the promoted member sent a checkpoint before its promotion was final");
    println!("PASS: {} posted no checkpoint while its promotion was not final", seats[p].name);

    // Finality released, its first checkpoint extends the one the promotion fixed (the vault accepts a
    // checkpoint only over its latest), signed by its standby key.
    chain.rpc("evm_setAutomine", json!([true])).await;
    mining = mine(&chain);
    until("the promoted member's first checkpoint", Duration::from_secs(120), &mut seats, async || {
        let seq = chain.view(vault, latestSeqCall {}).await;
        (seq > held).then_some(seq)
    })
    .await?;
    println!("PASS: {}'s first checkpoint landed once the promotion was final, over the one it fixed", seats[p].name);

    // The promotion consumed its standby key: it vouches for the replacement with the signer key it
    // now holds, which a set of no other member needs, before asking another member.
    let written = seats[p].dir.join("keys").join("standby-registration.json");
    until("the promoted member's own vouch", Duration::from_secs(150), &mut seats, async || {
        let file: serde_json::Value = serde_json::from_slice(&std::fs::read(&written).ok()?).ok()?;
        (file["voucher"].as_str()?.parse::<AccountId>().ok()? == signer).then_some(())
    })
    .await?;
    println!("PASS: {} vouched for its replacement standby key with its own signer key", seats[p].name);

    // A's authorization is reissued under the promotion's epoch: the reissue pays once, A's reverts.
    // The stream replays the log from its start, A's authorization first: the reissue follows it.
    let mut stream = promoted.stream().await?;
    let reissued = loop {
        let payout = stream.payout(trader.account, 10).await?;
        if payout.authorization.as_ref().is_some_and(|auth| auth.signer_epoch != issued_epoch.unwrap_or(1)) {
            break payout;
        }
    };
    let auth = reissued.authorization.as_ref().context("the reissue is typed")?;
    ensure!(
        auth.signer_epoch == epoch && auth.signer == signer,
        "reissued under {}/{}",
        auth.signer_epoch,
        auth.signer
    );
    let claim = claim(vault, &reissued, signer)?.abi_encode();
    chain.send(chain.accounts[0], vault, claim.clone()).await;
    chain.reverted(chain.accounts[0], vault, claim).await;
    let outgoing = claim_unchecked(&issued)?.abi_encode();
    chain.reverted(chain.accounts[0], vault, outgoing).await;
    println!("PASS: the reissue under epoch {epoch} paid once, and A's authorization reverts");

    // The promoted member's log replays across the change of signer.
    for seat in seats.iter_mut().filter(|seat| seat.name != "A") {
        seat.stop().await?;
    }
    mining.abort();
    let signers: BTreeSet<AccountId> = [AccountId(signing.address()), signer].into();
    let checkpoints = CheckpointDomain { chain_id: 31337, vault: AccountId(vault.into()) };
    let replayed = replay_path_with_attested(&seats[p].log_dir(), Some(checkpoints), signers.clone())?;
    ensure!(replayed.checkpoint_signers == signers, "the replay names {:?}", replayed.checkpoint_signers);
    println!("PASS: {}'s log replays across the change of signer, each checkpoint by an attested key", seats[p].name);
    Ok(())
}

/// The chain's latest block number.
async fn block_number(chain: &Chain) -> u64 {
    let number = chain.rpc_result("eth_blockNumber", json!([])).await;
    u64::from_str_radix(number.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).unwrap_or(0)
}

/// A block a second on the chain, until the task is aborted.
fn mine(chain: &Chain) -> tokio::task::JoinHandle<()> {
    let miner = chain.provider.clone();
    tokio::spawn(async move {
        loop {
            let _ = miner.request::<_, String>("evm_mine", ()).await;
            sleep(Duration::from_secs(1)).await;
        }
    })
}

/// True time on this host, in whole seconds: the chain follows it once the turns are drawn.
fn now_s() -> u64 {
    u64::try_from(now_ms() / 1000).expect("the host clock is past the epoch")
}

/// The boundary of `turn`.
fn boundary(turn: u64) -> u64 {
    turn * DAY_SECS + BOUNDARY_OFFSET_SECS
}

/// Stakes, registers and approves the seats' candidates with their releases, turns rotation on, has
/// each delegate to itself and register its standby key with the vault's signer's vouch, and closes
/// the epoch that adopts them.
async fn adopt(chain: &mut Chain, seats: &[Seat], schedule: Address, signing: &EnclaveKey) {
    for (seat, stake) in seats.iter().zip(STAKES) {
        chain.stake(seat.candidate, monkey(stake)).await;
        let release = (seat.digest, seat.config_hash);
        let endpoint = seat.endpoint.to_string();
        chain
            .register_release_at(seat.candidate, &seat.admin, monkey(2_000), release, &endpoint, &seat.hostname())
            .await;
    }
    let daily = Parameters { epochLength: DAY_SECS, ..dev_parameters().0 };
    let mut calls = vec![
        (chain.registry, setParametersCall { parameters: daily }.abi_encode()),
        (chain.registry, setVaultCall { vault_: chain.vault }.abi_encode()),
        (chain.vault, setRegistryCall { registry_: chain.registry }.abi_encode()),
        (chain.registry, setScheduleCall { schedule_: schedule }.abi_encode()),
    ];
    calls.extend(
        seats.iter().map(|seat| (chain.registry, chain.approval_of(seat.candidate, seat.digest, seat.config_hash))),
    );
    calls.push((chain.registry, setRotationCall { enabled: true }.abi_encode()));
    chain.govern_each(calls).await;
    for (seat, stake) in seats.iter().zip(STAKES) {
        let delegate = delegateCall { candidate: seat.candidate, amount: monkey(stake) };
        chain.send(seat.candidate, chain.staking, delegate.abi_encode()).await;
        let release = B256::from(seat.release().id());
        chain.register_standby(seat.candidate, release, Address::from(seat.standby.0), signing).await;
    }
    chain.close_epoch_at(SET_CLOSE_SECS).await;
}

/// Draws `turn` for `candidate` at its deadline: relays a seed from the relay's alias and draws, taking
/// the next seed after reverting each draw that names another member.
/// Error cases:
/// - no seed of the first forty draws `candidate`
async fn draw_for(
    chain: &Chain,
    (schedule, receiver): (Address, Address),
    turn: u64,
    candidate: Address,
) -> anyhow::Result<Turn> {
    let alias = chain.view(receiver, relayAliasCall {}).await;
    let seed_epoch = chain.view(receiver, seedEpochCall { turn }).await;
    for seed in 1..=40u8 {
        let before = chain.rpc_result("evm_snapshot", json!([])).await;
        chain.impersonate(alias).await;
        let relay = receiveSeedCall { turn, epoch: seed_epoch, seed: B256::repeat_byte(seed) };
        chain.send(alias, receiver, relay.abi_encode()).await;
        chain.mine_at(boundary(turn) - DAY_SECS + 60).await;
        chain.send(chain.accounts[1], schedule, drawCall { turn }.abi_encode()).await;
        let drawn = chain.view(schedule, turnOfCall { turn }).await;
        if drawn.candidate == candidate {
            return Ok(drawn);
        }
        chain.rpc("evm_revert", json!([before])).await;
    }
    anyhow::bail!("no seed drew turn {turn} for {candidate}")
}

/// Polls `check` every half second until it gives a value, failing if a seat's node exits first or
/// `within` passes.
/// Error cases:
/// - a node exits, or `within` passes
async fn until<T>(
    what: &str,
    within: Duration,
    seats: &mut [Seat],
    check: impl AsyncFn() -> Option<T>,
) -> anyhow::Result<T> {
    timeout(within, async {
        loop {
            for seat in seats.iter_mut() {
                seat.check()?;
            }
            if let Some(found) = check().await {
                return anyhow::Ok(found);
            }
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .with_context(|| format!("waiting for {what}"))?
}

/// The vault call paying `signed`, checked against `signer`.
/// Error cases:
/// - the authorization does not verify, or is malformed
fn claim(vault: Address, signed: &SignedWithdrawal, signer: AccountId) -> anyhow::Result<withdrawCall> {
    crate::withdrawals::verify(signed, CheckpointDomain { chain_id: 31337, vault: AccountId(vault.into()) }, signer)?;
    claim_unchecked(signed)
}

/// The vault call paying `signed`, unchecked.
/// Error cases:
/// - the authorization is malformed
fn claim_unchecked(signed: &SignedWithdrawal) -> anyhow::Result<withdrawCall> {
    let auth = signed.authorization.as_ref().context("a typed authorization")?;
    Ok(withdrawCall {
        withdrawal: Withdrawal {
            account: Address::from(signed.account.0),
            amount: crate::withdrawals::token_units(signed.amount)?,
            nonce: signed.nonce,
            approvedSeq: signed.approved_seq.context("approval seq")?,
            signerEpoch: auth.signer_epoch,
            issuedAt: auth.issued_at,
            expiresAt: auth.expires_at,
        },
        checkpointSeq: auth.checkpoint_seq,
        siblings: auth.siblings.clone(),
        signature: hex::decode(signed.signature.trim_start_matches("0x"))?.into(),
    })
}
