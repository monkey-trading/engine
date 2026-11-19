//! An exit proof checked as a wallet checks it before keeping it: each vault call the exit takes,
//! run first as the verifier's read that anyone may make, with no sender, against the root the
//! vault accepted. A change to the verifier that leaves a wallet no read for a part the proof holds
//! fails here; the verifier's forge tests read every part from any address.

use alloy::primitives::{Address, B256, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, bail, ensure};
use engine_core::merkle::ExitProof;
use engine_core::types::AccountId;

use crate::exit::account_calls;
use crate::sequencer::withdrawal_scenario::rpc::Chain;

// The verifier's reads, and the vault calls whose arguments they take, as
// `contracts/src/ExitVerifier.sol` and `contracts/src/Vault.sol` declare them.
sol! {
    struct AccountState {
        int128 balance;
        int128 unfinalisedDeposits;
        uint64 withdrawalNonce;
        int64 toxicityBps;
        uint32 openPositions;
        int128 toxicityFraction;
        int128 toxicityHistory;
        bytes32[] siblings;
    }
    struct PositionState { uint32 market; int128 qty; int128 entry; bytes32[] siblings; }
    struct MarketState {
        uint32 market;
        bool marked;
        int128 mark;
        uint8 quoteCurrency;
        uint32 fxMarket;
        bool fxMarked;
        int128 fxMark;
        int128 clientQty;
        int128 clientCost;
        bytes32 stateHash;
        bytes32[] siblings;
    }
    struct ExitTerms { uint64 depositCursor; uint64 depositBase; int128 pending; int128 held; int128 unheld; int128 heldReceipts; uint64[] gaps; }
    struct QueuedWithdrawal { address account; int128 amount; int128 remaining; uint64 requestedSeq; uint64 ticket; }
    struct ReceiptLeaf { bytes chainRef; uint64 depositId; address account; address payer; int128 amount; bytes32[] siblings; }
    struct Approval { address account; uint256 amount; uint64 nonce; uint64 approvedSeq; }

    function startExit(ExitTerms terms, bytes32[] siblings) external;
    function registerExit(address account, AccountState state, PositionState[] positions, MarketState[] markets) external;
    function registerExitCredits(address account, ReceiptLeaf[] credits) external;
    function registerExitWithdrawal(Approval approval, bytes32[] siblings) external;
    function registerExitQueued(QueuedWithdrawal queued, bytes32[] siblings) external;
    function registerExitDistribution(address account, uint256 entitled, bytes32[] siblings) external;
    function claimExit(address account) external returns (uint256);
    function refundReceipt(ReceiptLeaf leaf, uint8 kind) external;

    function exitVerifier() external view returns (address);
    function exitClaimed(address account) external view returns (uint256);
    function checkpointRoots(uint64 seq) external view returns (bytes32);
    function provesTerms(bytes32 root, ExitTerms terms, bytes32[] siblings) external view returns (bool);
    function previewClaim(
        bytes32 root,
        address account,
        AccountState state,
        PositionState[] positions,
        MarketState[] markets,
        ReceiptLeaf[] credits
    ) external view returns (uint256);
    function provesApproval(bytes32 root, Approval approval, bytes32[] siblings) external view returns (bool);
    function provesQueued(bytes32 root, QueuedWithdrawal queued, bytes32[] siblings) external view returns (bool);
    function provesDistribution(bytes32 root, address account, uint256 entitled, bytes32[] siblings)
        external view returns (bool);
    function provesReceipt(bytes32 root, ReceiptLeaf leaf, uint8 kind) external view returns (bool proven, bool matched);
}

/// Checks `proof`, `account`'s exit proof, against the root the vault accepted at its seq through
/// the verifier's reads alone, and returns the USDC base units the account's claim previews at:
/// the terms, the account with its credits, and each approval, queued request, distribution and
/// pending deposit the exit would register or refund.
/// Error cases:
/// - the vault holds another root at the proof's seq, or a read reverts or does not prove
pub(super) async fn wallet_check(chain: &Chain, account: AccountId, proof: &ExitProof) -> anyhow::Result<U256> {
    let root = proof.root;
    let accepted = chain.read(chain.vault, checkpointRootsCall { seq: proof.seq }).await?;
    ensure!(accepted == root, "the vault holds root {accepted} at seq {}, the proof names {root}", proof.seq);
    let verifier = chain.read(chain.vault, exitVerifierCall {}).await?;
    let mut registered = None;
    let mut credits = Vec::new();
    for call in account_calls(account, proof) {
        let data = &call.calldata[..];
        let proves = match data.get(..4).and_then(|selector| <[u8; 4]>::try_from(selector).ok()) {
            Some(startExitCall::SELECTOR) => {
                let start = startExitCall::abi_decode(data)?;
                read(chain, verifier, provesTermsCall { root, terms: start.terms, siblings: start.siblings }).await?
            }
            Some(registerExitCall::SELECTOR) => {
                registered = Some(registerExitCall::abi_decode(data)?);
                true
            }
            Some(registerExitCreditsCall::SELECTOR) => {
                credits.extend(registerExitCreditsCall::abi_decode(data)?.credits);
                true
            }
            Some(registerExitWithdrawalCall::SELECTOR) => {
                let held = registerExitWithdrawalCall::abi_decode(data)?;
                read(chain, verifier, provesApprovalCall { root, approval: held.approval, siblings: held.siblings })
                    .await?
            }
            Some(registerExitQueuedCall::SELECTOR) => {
                let held = registerExitQueuedCall::abi_decode(data)?;
                read(chain, verifier, provesQueuedCall { root, queued: held.queued, siblings: held.siblings }).await?
            }
            Some(registerExitDistributionCall::SELECTOR) => {
                let held = registerExitDistributionCall::abi_decode(data)?;
                let call = provesDistributionCall {
                    root,
                    account: held.account,
                    entitled: held.entitled,
                    siblings: held.siblings,
                };
                read(chain, verifier, call).await?
            }
            Some(refundReceiptCall::SELECTOR) => {
                let refund = refundReceiptCall::abi_decode(data)?;
                read(chain, verifier, provesReceiptCall { root, leaf: refund.leaf, kind: refund.kind }).await?.proven
            }
            Some(claimExitCall::SELECTOR) => true,
            _ => bail!("an exit call the wallet check does not know: {}", call.what),
        };
        ensure!(proves, "the verifier does not prove what {} carries", call.what);
    }
    let Some(held) = registered else { return Ok(U256::ZERO) };
    let preview = previewClaimCall {
        root,
        account: held.account,
        state: held.state,
        positions: held.positions,
        markets: held.markets,
        credits,
    };
    read(chain, verifier, preview).await
}

/// `call` on `verifier`, read as a wallet reads it: an `eth_call` with no sender.
/// Error cases:
/// - the read reverts or its answer does not decode
async fn read<C: SolCall>(chain: &Chain, verifier: Address, call: C) -> anyhow::Result<C::Return> {
    let what = C::SIGNATURE;
    chain.read(verifier, call).await.with_context(|| format!("the verifier's {what} read for a wallet"))
}

/// What the verifier has counted of `account`'s registered equity and credits, in USDC base units.
/// Error cases:
/// - the read fails
pub(super) async fn registered_claim(chain: &Chain, account: Address) -> anyhow::Result<U256> {
    let verifier = chain.read(chain.vault, exitVerifierCall {}).await?;
    read(chain, verifier, exitClaimedCall { account }).await
}

/// The root the vault accepted at `seq`, or zero for none.
/// Error cases:
/// - the read fails
pub(super) async fn accepted_root(chain: &Chain, seq: u64) -> anyhow::Result<B256> {
    chain.read(chain.vault, checkpointRootsCall { seq }).await
}
