//! The vault move scenario's steps on the alpha and the launch node: closing an emission period, settling at an
//! accepted checkpoint with nothing waiting in the staking mirror, and claiming MONKEY from the minter.

use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use axum::http::StatusCode;
use engine_core::command::Command;
use engine_core::emissions::TokenUnits;
use engine_core::merkle::EntitlementProof;
use engine_core::types::AccountId;

use super::super::chain::Chain;
use super::super::completed_through;
use super::super::node::Node;
use super::latestSeqCall;
use crate::sequencer::ClaimAnswer;

sol! {
    function claimEmissions(address account, uint256 entitled, uint64 checkpointSeq, bytes32[] siblings) external returns (uint256);
    function balanceOf(address account) external view returns (uint256);
}

/// Ticks at the end of the open emission period, or now if later, closing it.
pub(super) async fn close_period(node: &mut Node) {
    let end = node
        .query(|e| {
            let emissions = e.emissions().expect("emissions are initialised");
            emissions.schedule.period_end(emissions.period).max(e.now())
        })
        .await;
    node.advance_to(end).await;
    node.submit(Command::Tick).await;
}

/// Mines a block, waits for the staking mirror to complete a scan past it and past every block it mirrored an
/// event from, then ticks until the vault accepts a checkpoint at or after the last command so far, and returns
/// its seq: a checkpoint a carry reads with nothing waiting in the mirror.
pub(super) async fn settled_checkpoint(node: &Node, chain: &Chain) -> u64 {
    let block = chain.mine_at(chain.latest_timestamp().await + 1).await;
    node.wait("the staking mirror to complete its scan", move |e| {
        let mirrored = e.staking().and_then(|staking| staking.mirrored_through);
        completed_through(e) >= Some(block) && mirrored.is_none_or(|mirrored| completed_through(e) >= Some(mirrored))
    })
    .await;
    let last = node.query(|e| e.last_seq().expect("the log holds commands")).await;
    let vault = chain.vault;
    crate::test_support::wait_for("the vault to accept a checkpoint past the scan", async {
        loop {
            node.submit(Command::Tick).await;
            let posted = chain.view(vault, latestSeqCall {}).await;
            if posted >= last {
                return posted;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
}

/// Has a relayer claim `account`'s MONKEY from the minter, with the proof the node serves of `entitled` against
/// a checkpoint the minter's vault accepted, returning what `account` received.
pub(super) async fn claim_monkey(chain: &Chain, node: &Node, account: Address, entitled: TokenUnits) -> U256 {
    let proof = posted_entitlement(node, AccountId(account.into()), entitled).await;
    let before = chain.view(chain.monkey, balanceOfCall { account }).await;
    let call = claimEmissionsCall {
        account,
        entitled: U256::from(proof.entitled.0),
        checkpointSeq: proof.seq,
        siblings: proof.siblings.clone(),
    };
    chain.send(chain.accounts[1], chain.minter, call.abi_encode()).await;
    chain.view(chain.monkey, balanceOfCall { account }).await - before
}

/// `account`'s entitlement proof from the API once the vault has accepted a root naming `entitled`, ticking until
/// a checkpoint carries it there.
async fn posted_entitlement(node: &Node, account: AccountId, entitled: TokenUnits) -> EntitlementProof {
    crate::test_support::wait_for("the checkpoint worker to post the entitlement's root", async {
        loop {
            node.submit(Command::Tick).await;
            let (status, body) = node.get(&format!("/v1/accounts/{account}/emissions")).await;
            if status == StatusCode::OK {
                let answer: ClaimAnswer<EntitlementProof> = serde_json::from_value(body).unwrap();
                if answer.claimable && answer.proof.entitled == entitled {
                    return answer.proof;
                }
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
}
