//! The boundary's log check leg (#594). The fixture's seeds draw A, B and B for the turns before,
//! at and after `LEG`. A withdrawal approved during A's turn waits for the next boundary's check,
//! though the vault holds its checkpoint, and pays only once B's handover has verified A's log; a
//! second, approved during B's turn, waits for the boundary B holds on through, where no handover
//! checks it. There B's node asks the members of the turn's order: with A, first in it, and C down,
//! the check stays due, and a verification from B's own release, which sequenced the range, is
//! refused; once C follows again, its verification releases the withdrawal.

use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use axum::http::StatusCode;
use engine_core::Fixed;
use engine_core::auth::RequestAuth;
use engine_core::command::WithdrawalRequest;
use engine_core::eip712::{self, B256};
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::super::chain::Chain;
use super::super::node::Node;
use super::FIRST_TURN;
use super::seat::{Holder, Member};
use super::switch::Members;
use crate::checkpoints::HandoverSource;
use crate::clock::Clock;
use crate::config::TeeProvider;
use crate::handover::evidence::{Quoting, produce};
use crate::log::CheckpointDomain;
use crate::log_check::ask::{Checks, admit};
use crate::log_check::{Answer, Statement, report_data};
use crate::sequencer::{Request, SignedWithdrawal};
use crate::tee::MockAttestor;
use crate::tee::sealing::Platform;
use crate::test_support::wait_for;

sol! {
    struct Withdrawal {
        address account;
        uint256 amount;
        uint64 nonce;
        uint64 approvedSeq;
        uint64 signerEpoch;
        uint64 issuedAt;
        uint64 expiresAt;
    }
    function withdraw(Withdrawal withdrawal, uint64 checkpointSeq, bytes32[] siblings, bytes signature) external;
    function balanceOf(address account) external view returns (uint256);
    function latestSeq() external view returns (uint64);
}

/// The turn B takes from A, which the turn after holds on.
pub(super) const LEG: u64 = FIRST_TURN + 3;
/// The members' indices in the scenario's list.
const A: usize = 0;
const B: usize = 1;
const C: usize = 2;
/// Each withdrawal, in whole USDC.
const AMOUNT: i64 = 1_000;

/// The leg's withdrawing account, and the nonces of the withdrawals approved during A's and B's turns.
pub(super) struct Leg {
    key: SigningKey,
    first: Option<u64>,
    second: Option<u64>,
}

impl Leg {
    /// Declares and funds the withdrawing account on `node`, through `chain`'s vault.
    pub(super) async fn start(chain: &Chain, node: &Node) -> Self {
        let key = SigningKey::from_slice(&[0x7b; 32]).unwrap();
        let account = eip712::address(key.verifying_key());
        super::super::operator_share::declare(node, &key).await;
        chain.fund(Some(Address::from(account.0)), U256::from(100_000_000_000u64)).await;
        chain.finalize().await;
        node.wait("the withdrawing account's deposit to credit", move |e| {
            e.accounts().get(&account).is_some_and(|held| held.balance > Fixed::ZERO)
        })
        .await;
        Self { key, first: None, second: None }
    }

    /// Before `turn`'s boundary, whose member is `member`, with `holder` the incumbent: before `LEG`'s,
    /// during A's turn, a withdrawal is approved that waits for `LEG`'s check; before the boundary B holds
    /// on through, A and C stop following.
    pub(super) async fn before_boundary(
        &mut self,
        chain: &Chain,
        turn: u64,
        (holder, member): (&Holder, usize),
        shared: &Members,
    ) {
        if turn == LEG {
            assert_eq!((holder.member, member), (A, B), "the fixture draws A, then B at turn {LEG}");
            let nonce = self.approve(chain, &holder.node).await;
            self.waits(chain, &holder.node, nonce, LEG).await;
            self.first = Some(nonce);
            println!(
                "PASS: turn {}: a withdrawal approved during A's turn waits for turn {LEG}'s check, though the vault holds its checkpoint",
                turn - 1
            );
        } else if turn == LEG + 1 {
            assert_eq!((holder.member, member), (B, B), "the fixture draws B again for turn {}", LEG + 1);
            shared.pause(A);
            shared.pause(C);
        }
    }

    /// Once `turn`'s boundary has passed, with `holder` the turn's: at `LEG` the first withdrawal pays,
    /// B's handover having verified A's log, and a second is approved that waits; at the turn after, B
    /// holds on, refuses a check from its own release, and C's verification releases the second.
    pub(super) async fn after_boundary(
        &mut self,
        chain: &Chain,
        turn: u64,
        (holder, members): (&Holder, &[Member]),
        shared: &mut Members,
    ) {
        if turn == LEG {
            let first = self.first.expect("approved during A's turn");
            self.pays(chain, &holder.node, first).await;
            println!(
                "PASS: turn {LEG}: the withdrawal approved during A's turn paid only once B's handover verified A's log"
            );
            let second = self.approve(chain, &holder.node).await;
            self.waits(chain, &holder.node, second, LEG + 1).await;
            self.second = Some(second);
            println!("PASS: turn {LEG}: a withdrawal approved during B's turn waits for turn {}'s check", LEG + 1);
        } else if turn == LEG + 1 {
            let second = self.second.expect("approved during B's turn");
            refuse_own(&holder.node, &members[B]).await;
            self.unsigned(&holder.node, second).await;
            println!(
                "PASS: turn {}: B holds on; with A and C down the check stays due, and a verification from B's own release is refused",
                LEG + 1
            );
            shared.restart(C);
            self.pays(chain, &holder.node, second).await;
            println!(
                "PASS: turn {}: C's verification, with A first in the turn's order down, released the withdrawal approved during B's turn, with no handover",
                LEG + 1
            );
        }
    }

    /// Requests a withdrawal on `node`, signed by the leg's key: the approval's nonce.
    async fn approve(&self, chain: &Chain, node: &Node) -> u64 {
        let account = eip712::address(self.key.verifying_key());
        let nonce = node.query(move |engine| engine.next_request_nonce(account)).await;
        let expires_at = node.clock.now_ms() + 60_000;
        let amount = Fixed::from_int(AMOUNT);
        let domain = eip712::domain(31_337, AccountId(chain.vault.into()));
        let signature = eip712::sign(&self.key, &eip712::withdrawal_hash(&domain, account, amount, nonce, expires_at));
        let request = WithdrawalRequest { account, amount, auth: Some(RequestAuth { nonce, expires_at, signature }) };
        let (status, events) = node.post("/v1/withdrawals", serde_json::to_value(request).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{events}");
        let approved = events.as_array().unwrap().iter().find(|event| event["type"] == "withdrawal_approved");
        approved.and_then(|event| event["nonce"].as_u64()).unwrap_or_else(|| panic!("no approval: {events}"))
    }

    /// Waits until the vault holds a checkpoint past approval `nonce`, then requires it still unsigned,
    /// waiting for the check at `turn`'s boundary, as the account's payouts read names it.
    async fn waits(&self, chain: &Chain, node: &Node, nonce: u64, turn: u64) {
        let approved = node.query(|engine| engine.last_seq().unwrap()).await;
        let vault = chain.vault;
        // Each post waits for the one before to be final, two blocks on, which only mined blocks bring.
        wait_for("the vault to hold a checkpoint past the approval", async {
            while chain.view_at(vault, latestSeqCall {}, "finalized").await < approved {
                chain.finalize().await;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
        .await;
        // The poster observes the posted checkpoint within a few of its 40 ms rounds.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let payout = self.payout(node, nonce).await;
        assert_eq!(payout["withdrawal"], Value::Null, "signed before the boundary's check: {payout}");
        let boundary = engine_core::turns::boundary(turn).unwrap();
        assert_eq!(payout["check"], json!({ "turn": turn, "boundary_ms": boundary }), "{payout}");
    }

    /// Requires approval `nonce` still unsigned on `node`.
    async fn unsigned(&self, node: &Node, nonce: u64) {
        let payout = self.payout(node, nonce).await;
        assert_eq!(payout["withdrawal"], Value::Null, "signed without a qualifying check: {payout}");
    }

    /// Waits until `node` signs approval `nonce`, then claims it on the vault, which pays the account.
    async fn pays(&self, chain: &Chain, node: &Node, nonce: u64) {
        let signed = wait_for("the approval's signature", async {
            let mut round = 0_u64;
            loop {
                let payout = self.payout(node, nonce).await;
                if !payout["withdrawal"].is_null() {
                    break serde_json::from_value::<SignedWithdrawal>(payout["withdrawal"].clone()).unwrap();
                }
                // Now and then a block, so a post waiting for finality goes on.
                round += 1;
                if round.is_multiple_of(10) {
                    chain.finalize().await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        let auth = signed.authorization.clone().expect("a typed authorization");
        let account = Address::from(signed.account.0);
        let claim = withdrawCall {
            withdrawal: Withdrawal {
                account,
                amount: crate::withdrawals::token_units(signed.amount).unwrap(),
                nonce: signed.nonce,
                approvedSeq: signed.approved_seq.expect("the approval's seq"),
                signerEpoch: auth.signer_epoch,
                issuedAt: auth.issued_at,
                expiresAt: auth.expires_at,
            },
            checkpointSeq: auth.checkpoint_seq,
            siblings: auth.siblings.clone(),
            signature: hex::decode(signed.signature.trim_start_matches("0x")).unwrap().into(),
        };
        let before = chain.view(chain.usdc, balanceOfCall { account }).await;
        chain.next_block_at((chain.latest_timestamp().await + 1).max(auth.issued_at)).await;
        chain.send(chain.accounts[1], chain.vault, claim.abi_encode()).await;
        let paid = chain.view(chain.usdc, balanceOfCall { account }).await - before;
        assert_eq!(paid, crate::withdrawals::token_units(Fixed::from_int(AMOUNT)).unwrap(), "the vault paid {paid}");
    }

    /// The leg account's unpaid approval `nonce`, as `node`'s payouts read answers.
    async fn payout(&self, node: &Node, nonce: u64) -> Value {
        let account = eip712::address(self.key.verifying_key());
        let (status, page) = node.get(&format!("/v1/accounts/{account}/payouts")).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let payouts = page["payouts"].as_array().cloned().unwrap_or_default();
        payouts
            .into_iter()
            .find(|payout| payout["nonce"] == json!(nonce))
            .unwrap_or_else(|| panic!("no payout {nonce}: {page}"))
    }
}

/// With the check due at the boundary B holds on through, a verification from B's own release is
/// refused: the operator's admission of a member's answer, and its sequencer, both turn it away.
async fn refuse_own(node: &Node, b: &Member) {
    let (_, due) = node.asking.as_ref().expect("a seat asks for its checks");
    let mut due = due.clone();
    let due = wait_for("the check to come due", async {
        loop {
            if let Some(due) = due.borrow_and_update().clone() {
                break due;
            }
            due.changed().await.unwrap();
        }
    })
    .await;
    let boundary = engine_core::turns::boundary(due.turn).unwrap();
    let statement =
        Statement { signer_epoch: 1, boundary, turn: due.turn, position: due.position, set: due.set, head: due.head };
    let (nonce, domain) = ([0x6b; 32], CheckpointDomain { chain_id: 31_337, vault: node.state.operator.vault });
    let release = b.release();
    let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
    let platform = Platform { launch_digest: release.launch_digest, policy: 0, tcb: 0 };
    let bound = report_data(domain, &nonce, &statement, &release.config_sha256);
    let evidence = produce(&quoting, &MockAttestor, &platform, bound).await.unwrap();
    let answer = Answer::Verified { config_sha256: hex::encode(release.config_sha256), evidence };
    let source = HandoverSource {
        candidate: AccountId(b.candidate.into()),
        release: release.id(),
        api_hostname: b.hostname(),
        handover_endpoint: b.endpoint.clone(),
    };
    let own = Platform { launch_digest: [0; 48], policy: 0, tcb: 0 };
    let checks = Checks { domain, provider: TeeProvider::Mock, own: &own, now_seconds: None };
    let refused = admit(&checks, (&nonce, &statement), &source, &due.excluded, answer).unwrap_err();
    assert!(format!("{refused:#}").contains("sequenced part of the range"), "{refused:#}");
    let (reply, taken) = oneshot::channel();
    let release = B256::from(release.id());
    node.state.tx.send(Request::LogChecked { head: due.head, release, reply }).await.unwrap();
    assert!(!taken.await.unwrap(), "B's sequencer took a verification from its own release");
}
