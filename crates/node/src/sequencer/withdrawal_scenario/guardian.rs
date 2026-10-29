//! The guardian's emergency pause and signer retirement against an actual node, Vault and timelock.

use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use engine_core::event::{Event, Reject};
use engine_core::types::AccountId;
use tokio::time::timeout;

use crate::log::LogEntry;
use crate::sequencer::SignedWithdrawal;
use crate::sequencer::withdrawal_scenario::client::{Stream, User};
use crate::sequencer::withdrawal_scenario::flows::{entries, last_seq, pay, replay};
use crate::sequencer::withdrawal_scenario::node::Node;
use crate::sequencer::withdrawal_scenario::rpc::{
    latestSeqCall, pauseWithdrawalsCall, retireSignerCall, setPausesCall, setSignerCall, withdrawalsPausedCall,
};
use crate::test_support::LOADED_WAIT;

/// With an authorization issued but unclaimed, the guardian pauses withdrawals and then retires
/// the signer: the claim reverts, the node pages Critical and refuses new withdrawals, including
/// after a restart, and once the timelock installs this enclave's key again and lifts the pause,
/// the node reissues under the new epoch and the claim pays.
/// Error cases:
/// - any real node/chain step or behavioral assertion fails
pub(super) async fn pause_and_retirement() -> anyhow::Result<()> {
    let mut node = Node::start("guardian-pause").await?;
    let trader = User::new(1)?;
    node.chain.deposit(trader.address(), 100, false).await?;
    node.client.declare(&trader).await?;
    node.client.wait_balance(&trader, "100").await?;
    node.client.wait_finalised(&trader).await?;
    node.wait_withdrawals_enabled(last_seq(&node.dir.join("log"))?).await?;
    let mut stream = node.client.stream().await?;
    let request = node.client.withdrawal(&trader, "10").await?;
    node.client.request_withdrawal(&request).await?;
    let issued = stream.payout(trader.account, 10).await?;

    let guardian = node.chain.guardian();
    node.chain.send(guardian, node.chain.vault, pauseWithdrawalsCall {}.abi_encode()).await?;
    ensure!(node.chain.read(node.chain.vault, withdrawalsPausedCall {}).await?, "the guardian's pause is immediate");
    node.chain.refuses(node.chain.claim(&issued)?.abi_encode()).await?;
    let posted = node.chain.read(node.chain.vault, latestSeqCall {}).await?;
    node.wait_withdrawals_halted("paused", posted).await?;
    refused(&node, &trader).await?;
    timeout(LOADED_WAIT, async {
        while node.chain.read(node.chain.vault, latestSeqCall {}).await? <= posted {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        anyhow::Ok(())
    })
    .await
    .context("checkpoint posting continues during a pause")??;

    drop(stream);
    node.crash().await?;
    node.restart().await?;
    let posted = node.chain.read(node.chain.vault, latestSeqCall {}).await?;
    node.wait_withdrawals_halted("paused", posted).await?;
    refused(&node, &trader).await?;

    node.chain.send(guardian, node.chain.vault, retireSignerCall {}.abi_encode()).await?;
    node.chain.refuses(node.chain.claim(&issued)?.abi_encode()).await?;
    node.wait_withdrawals_halted("signer_retired", posted).await?;
    refused(&node, &trader).await?;

    let mut stream = node.client.stream().await?;
    let signer = alloy::primitives::Address::from(node.chain.signer.0);
    node.chain.govern(setSignerCall { signer }.abi_encode()).await?;
    node.chain.govern(setPausesCall { depositsPaused: false, withdrawalsPaused: false }.abi_encode()).await?;
    let reissued = payout_at_epoch(&mut stream, trader.account, 10, 2).await?;
    ensure!(
        (reissued.nonce, reissued.approved_seq) == (issued.nonce, issued.approved_seq),
        "the reissue changed the approval"
    );
    node.chain.refuses(node.chain.claim(&issued)?.abi_encode()).await?;
    pay(&node, &reissued).await?;
    let next = node.client.withdrawal(&trader, "5").await?;
    node.client.request_withdrawal(&next).await?;
    pay(&node, &payout_at_epoch(&mut stream, trader.account, 5, 2).await?).await?;
    node.client.wait_balance(&trader, "85").await?;
    drop(stream);
    node.stop().await?;

    let signed: Vec<_> = entries(&node.dir.join("log"))?
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Withdrawal(withdrawal) if withdrawal.nonce == issued.nonce => Some(withdrawal),
            _ => None,
        })
        .collect();
    ensure!(signed == [issued, reissued], "the halt signed an authorization: {signed:?}");
    replay(&node)?;
    println!(
        "PASS: guardian pause and signer retirement refuse the claim, page Critical, stop authorizations across a restart, and the timelock's new epoch and unpause reissue and pay"
    );
    Ok(())
}

/// Requires a new withdrawal request to be refused for want of a payout.
/// Error cases:
/// - the request fails or the engine does not refuse it as unpayable
async fn refused(node: &Node, trader: &User) -> anyhow::Result<()> {
    let request = node.client.withdrawal(trader, "5").await?;
    let events = node.client.request_withdrawal(&request).await?;
    ensure!(
        events.iter().any(|event| matches!(
            event,
            Event::WithdrawalRejected { account, reason: Reject::PayoutUnavailable, .. } if *account == trader.account
        )),
        "a withdrawal was accepted while the vault halts claims: {events:?}"
    );
    Ok(())
}

/// The account's next stream authorization of `amount` signed in `epoch`, skipping older ones.
/// Error cases:
/// - the stream fails, or no such authorization arrives before the deadline
async fn payout_at_epoch(
    stream: &mut Stream,
    account: AccountId,
    amount: i64,
    epoch: u64,
) -> anyhow::Result<SignedWithdrawal> {
    loop {
        let payout = stream.payout(account, amount).await?;
        if payout.authorization.as_ref().is_some_and(|auth| auth.signer_epoch == epoch) {
            return Ok(payout);
        }
    }
}
