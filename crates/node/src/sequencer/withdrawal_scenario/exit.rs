//! The exit against the last posted root on the actual vault: proofs built from the stopped node's
//! public log alone, the exit started once checkpoints stop, a claim registered and paid once, a
//! deposit after the root refunded, and the house remainder released through the timelock.

use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use engine_core::event::Event;
use engine_core::types::Side;
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};

use crate::exit::{ExitSubject, exit_bundle};
use crate::log::LogEntry;
use crate::sequencer::withdrawal_scenario::client::User;
use crate::sequencer::withdrawal_scenario::flows::{entries, last_seq};
use crate::sequencer::withdrawal_scenario::node::Node;
use crate::sequencer::withdrawal_scenario::rpc::{balanceOfCall, latestSeqCall};
use crate::test_support::{DEV_EXIT_SCHEDULE, LOADED_WAIT};

sol! {
    function exitStartedAt() external view returns (uint64);
    function exitClaims(address account) external view returns (uint256);
    function refundDeposit(uint256 id) external;
    function releaseHouseRemainder(address to) external;
    function schedule(address target, uint256 value, bytes data, bytes32 predecessor, bytes32 salt, uint256 delay) external;
    function execute(address target, uint256 value, bytes payload, bytes32 predecessor, bytes32 salt) external payable;
}

/// The house deposits 50 USDC and a trader 100, the trader buys BTC at 100 and the mark moves to
/// 150, and a checkpoint covering that mark reaches the vault before the node stops. Another account
/// then deposits 20, which no posted root holds. With the proofs built from the log, the exit starts
/// once the exit period passes; the trader registers its equity at the root's mark, once; the late
/// deposit is refunded to its payer, once; and after the claim window the trader is paid its claim,
/// once, and the timelock releases the rest, leaving the vault empty.
/// Error cases:
/// - any node, chain, proof or balance step differs from the above
pub(super) async fn exit_against_the_last_posted_root() -> anyhow::Result<()> {
    let mut node = Node::start("exit-against-root").await?;
    let trader = User::new(1)?;
    let late = User::new(2)?;
    node.chain.deposit(node.chain.funder, 50, true).await?;
    node.client.wait_house_cash("50").await?;
    node.chain.deposit(trader.address(), 100, false).await?;
    node.client.declare(&trader).await?;
    node.client.wait_balance(&trader, "100").await?;
    node.client.wait_mark(100).await?;
    let events = node.client.order(&trader, Side::Buy, "0.1").await?;
    ensure!(events.iter().any(|event| matches!(event, Event::Fill(_))), "the trader's buy did not fill: {events:?}");
    node.services.set_price(150);
    node.client.wait_mark(150).await?;
    let marked = last_seq(&node.dir.join("log"))?;
    timeout(LOADED_WAIT, async {
        while node.chain.read(node.chain.vault, latestSeqCall {}).await? < marked {
            sleep(Duration::from_millis(200)).await;
        }
        anyhow::Ok(())
    })
    .await
    .context("a checkpoint covering the moved mark reaching the vault")??;
    node.stop().await?;
    // Draining may post one more checkpoint; the exit proves against whichever the vault holds last.
    let posted = node.chain.read(node.chain.vault, latestSeqCall {}).await?;
    node.chain.deposit(late.address(), 20, false).await?;

    let log = node.dir.join("log");
    let bundle = exit_bundle(&log, posted, ExitSubject::Account(trader.account))?;
    let proof = bundle.account.as_ref().context("the trader's exit proof")?;
    let account = proof.account.as_ref().context("the trader's account proof")?;
    ensure!(proof.terms.terms.deposit_cursor == 2, "the root holds the two deposits before it: {:?}", proof.terms);
    let equity = account.equity().context("a valued account")?;
    ensure!(equity > account.leaf.balance, "the open position's gain at the root's mark is part of the claim");
    let claim = U256::from(equity.raw() / 1_000);
    let calls: Vec<Vec<u8>> = bundle.calls.iter().map(|call| call.calldata.to_vec()).collect();
    let [start, register, claim_exit] = &calls[..] else { panic!("start, register, and claim: {:?}", bundle.calls) };

    node.chain.refuses(start.clone()).await?;
    warp(&node, DEV_EXIT_SCHEDULE.0).await?;
    node.chain.send(node.chain.payer, node.chain.vault, start.clone()).await?;
    ensure!(node.chain.read(node.chain.vault, exitStartedAtCall {}).await? != 0, "the exit did not start");
    node.chain.send(node.chain.relayer, node.chain.vault, register.clone()).await?;
    let owner = trader.address();
    ensure!(
        node.chain.read(node.chain.vault, exitClaimsCall { account: owner }).await? == claim,
        "the registered claim is not the account's equity at the root's marks"
    );
    node.chain.duplicate_reverts(register.clone()).await?;
    let refund = refundDepositCall { id: U256::from(2) }.abi_encode();
    node.chain.send(node.chain.relayer, node.chain.vault, refund.clone()).await?;
    ensure!(balance(&node, late.address()).await? == U256::from(20_000_000), "the late deposit was not refunded");
    node.chain.duplicate_reverts(refund).await?;
    node.chain.refuses(claim_exit.clone()).await?;

    warp(&node, DEV_EXIT_SCHEDULE.1).await?;
    node.chain.send(node.chain.relayer, node.chain.vault, claim_exit.clone()).await?;
    ensure!(balance(&node, owner).await? == claim, "the exit did not pay the proven balance");
    node.chain.duplicate_reverts(claim_exit.clone()).await?;
    let treasury = Address::repeat_byte(0x7e);
    release(&node, treasury).await?;
    ensure!(
        balance(&node, treasury).await? == U256::from(150_000_000) - claim,
        "the remainder is not the house's capital and the trader's losses"
    );
    ensure!(balance(&node, node.chain.vault).await? == U256::ZERO, "the exit left collateral behind");
    ensure!(
        entries(&log)?.iter().any(|entry| matches!(entry, LogEntry::Checkpoint(cp) if cp.seq == posted)),
        "the exit root is not a checkpoint of the public log"
    );
    println!(
        "PASS: exit from the public log: equity {} at the root's mark paid once, a deposit after the root refunded \
         once, and the remainder released through the timelock",
        equity
    );
    Ok(())
}

/// Moves the chain's clock on by `seconds` and mines a block at it.
/// Error cases:
/// - the local RPC refuses either call
async fn warp(node: &Node, seconds: u64) -> anyhow::Result<()> {
    let _: Value = node.chain.request("evm_increaseTime", json!([seconds])).await?;
    let _: Value = node.chain.request("evm_mine", json!([])).await?;
    Ok(())
}

/// Releases the house remainder to `to` through the timelock: scheduled by its proposer, then
/// executed once its one-second delay has passed.
/// Error cases:
/// - scheduling or execution fails
async fn release(node: &Node, to: Address) -> anyhow::Result<()> {
    let data = releaseHouseRemainderCall { to }.abi_encode();
    let vault = node.chain.vault;
    let scheduled = scheduleCall {
        target: vault,
        value: U256::ZERO,
        data: data.clone().into(),
        predecessor: B256::ZERO,
        salt: B256::ZERO,
        delay: U256::from(1),
    };
    node.chain.send(node.chain.deployer, node.chain.timelock, scheduled.abi_encode()).await?;
    warp(node, 2).await?;
    let executed = executeCall {
        target: vault,
        value: U256::ZERO,
        payload: data.into(),
        predecessor: B256::ZERO,
        salt: B256::ZERO,
    };
    node.chain.send(node.chain.relayer, node.chain.timelock, executed.abi_encode()).await?;
    Ok(())
}

/// `owner`'s USDC balance.
/// Error cases:
/// - the read fails
async fn balance(node: &Node, owner: Address) -> anyhow::Result<U256> {
    node.chain.read(node.chain.token, balanceOfCall { owner }).await
}
