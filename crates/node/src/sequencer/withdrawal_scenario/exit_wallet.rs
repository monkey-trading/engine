//! An exit proof checked as a wallet checks it before keeping it: the terms and the account's claim,
//! with its credits, read from the verifier as anyone may, with no sender, against the root the
//! vault accepted. A change to the verifier that leaves a wallet no such read fails here; the
//! verifier's forge tests read the approvals, queued requests, distributions and pending deposits.

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
    struct ReceiptLeaf { bytes chainRef; uint64 depositId; address account; address payer; int128 amount; bytes32[] siblings; }

    function startExit(ExitTerms terms, bytes32[] siblings) external;
    function registerExit(address account, AccountState state, PositionState[] positions, MarketState[] markets) external;
    function registerExitCredits(address account, ReceiptLeaf[] credits) external;
    function claimExit(address account) external returns (uint256);

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
}

/// Checks `proof`, `account`'s exit proof, against the root the vault accepted at its seq through
/// the verifier's reads alone, and returns the USDC base units its equity and credits preview at:
/// the terms, and the account with its credits, which must be `account`'s own.
/// Error cases:
/// - the vault holds another root at the proof's seq, or a read reverts or does not prove
/// - the account proof is another account's
/// - the proof holds a part this check leaves to the forge tests (an approval, a queued request, a
///   distribution or a pending deposit), or a call's data does not decode
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
            Some(claimExitCall::SELECTOR) => true,
            _ => bail!("the wallet check leaves this part to the verifier's forge tests: {}", call.what),
        };
        ensure!(proves, "the verifier does not prove what {} carries", call.what);
    }
    let Some(held) = registered else { return Ok(U256::ZERO) };
    let own = Address::from(account.0);
    ensure!(held.account == own, "the proof is account {}'s, not {own}'s", held.account);
    let preview = previewClaimCall {
        root,
        account: own,
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
