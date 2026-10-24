# Custody vault

`Vault.sol` holds six-decimal USDC collateral on Arbitrum One. The deterministic engine owns trading balances and the FIFO withdrawal queue. The vault accepts deposits, stores enclave-signed checkpoints, and pays signed approvals proven against an accepted checkpoint. It is not upgradeable. Emergency exits remain separately scoped in #48.

Deposits accept 10 to 1e9 USDC per call. `deposit`, `depositFor` and `depositToHouse` share one deposit-id sequence. The events report the tokens actually received, including for a token with an incoming transfer fee. The engine credits finalized events once by transaction hash and log index.

## Governance and deployment

Deploy an [OpenZeppelin `TimelockController`](https://docs.openzeppelin.com/contracts/5.x/api/governance#TimelockController) with a deployment-selected, nonzero minimum delay and the deployment's multisig as proposer. Configure executor and administrator roles deliberately; the timelock must administer itself after setup. Pass that deployed timelock as the third Vault constructor argument, after the token and attestation-verified enclave signer. The Vault constructor checks that the timelock has code and a nonzero delay. Deployment must verify the supplied contract is the intended OZ implementation and that its roles belong to the intended multisig; a Solidity address/type cannot establish those facts.

Only calls executed by the timelock may invoke `setSigner(address)` or `setPauses(bool depositsPaused, bool withdrawalsPaused)`. Signer changes increment `signerEpoch`, initially 1, including when the same address is reused. Both checkpoint and withdrawal verification use the current `checkpointSigner`. Rotation immediately invalidates old withdrawal epochs; current-key reissues retain the original account, amount, nonce and approval sequence. The nonce-use mapping survives rotation. `SignerChanged` and `PausesChanged` record administration.

Deposit pause covers all three deposit routes, including house capital. Withdrawal pause blocks claims without consuming nonces. The flags are independent; checkpoint posting remains available during either pause. Changing either flag requires the configured timelock delay. Timelock delay/role changes themselves follow OZ governance; the deployment must preserve its intended delay and proposer policy.

This change requires a new Vault deployment and constructor updates in deployment clients. It provides no migration of an existing Vault's balances, accepted roots or used nonces. The signing node must read the current signer epoch and reconcile against the deployed Vault before releasing authorizations.

## Checkpoints and approval proofs

`postCheckpoint` retains the #24 digest and ABI: `keccak256("engine.checkpoint.v2" || uint64(chainId) || vault || uint64(seq) || root || bool(hasParent) || uint64(parentSeq) || parentRoot)`, using packed, big-endian integers. A checkpoint must increase sequence and name the exact latest accepted parent. Only one competing child can commit; stale-parent signatures cannot be relabeled with another parent.

`checkpointRoots(seq)` and `hasCheckpointAt(seq)` retain every accepted root. Claims can use older roots, which protects approvals still unpaid after later checkpoints remove or change their leaves. Roots rejected by the parent fence never become claimable. Storage grows by a bounded number of words per posted checkpoint; history has no eviction because there is no on-chain authority for deciding that every approval in a root is paid.

`checkpointSignerAt(seq)` and `checkpointEpochAt(seq)` record the signer and epoch when that exact checkpoint was accepted, and remain unchanged by later rotations. `totalWithdrawn()` counts cumulative USDC base units successfully paid; it changes atomically with the nonce, transfer and payment event, and never increases on a reverted claim. Checkpoint RPC reconciliation reads these fields at one canonical finalized block with the token balance and nonce-use status.

An approval key is `keccak256(abi.encodePacked(uint8(32), address account, uint64 nonce))`. Its value hash is `keccak256(abi.encode(address account, uint256 amountUsdc, uint64 nonce, uint64 approvedSeq))`. The compressed sparse Merkle tree hashes leaves as `keccak256(0x00 || key || valueHash)` and branches as `keccak256(0x01 || left || right)`. Siblings are supplied root-to-leaf, at most 256, and folded backwards with the key's bits from most significant to least significant. A single-leaf root takes an empty proof. Claims require an accepted checkpoint sequence at least as large as `approvedSeq`.

## Withdrawal authorization

The domain is `EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)`, with name `EngineWithdrawal`, version `1`, the current chain id and this Vault address. OZ `EIP712` builds the domain separator. The exact typed struct is:

```text
Withdrawal(address account,uint256 amount,uint64 nonce,uint64 approvedSeq,uint64 signerEpoch,uint64 issuedAt,uint64 expiresAt)
```

`withdraw(Withdrawal withdrawal, uint64 checkpointSeq, bytes32[] siblings, bytes signature)` is permissionless to relay and always transfers to `withdrawal.account`. `withdrawalDigest` exposes the EIP-712 digest. The signature must recover the current enclave signer using OZ ECDSA, and its epoch must equal the current epoch. `issuedAt` and `expiresAt` are Unix seconds; expiry is exactly 86400 seconds after issue, issue cannot be in the future, and expiry must be strictly after the claim's block timestamp.

An account/nonce pair can pay once across checkpoints, reissues and signer epochs. `usedNonces(account, nonce)` exposes payment status. Amounts are integer USDC base units without conversion or rounding; zero amounts, the zero account and the Vault as recipient are rejected. All entry points that transfer tokens are non-reentrant. A claim consumes its nonce before `SafeERC20` transfer, and checks both the vault debit and recipient credit equal the approved amount. A failed, fee-bearing or otherwise inexact outgoing transfer reverts atomically, preserving funds and the nonce. `WithdrawalPaid(account, amount, nonce, approvedSeq)` gives node reconciliation the full approval identity.

## Tests and shared vector

Dependencies are pinned in `soldeer.lock`. From this directory, install with `forge soldeer install`, then run `forge fmt --check`, `forge build` and `forge test`. Foundry needs Cancun for the existing transient reentrancy guard. Its filesystem permission grants read access only to `test/fixtures`.

`test/fixtures/withdrawal.json` is the shared Rust/Solidity interoperability vector. It includes the exact domain/type, approval leaf key/value, four ordered siblings, accepted root, hashes, signer and signature. The signer is the explicitly public test key `0x1234`. This artifact is independent of immutable protocol reference logs. `WithdrawalVector.t.sol` verifies its digest, recovered signer, proof and exact payout at the fixture's chain/address. The Rust signer must consume this same artifact.

`Withdrawals.t.sol`, `WithdrawalProofs.t.sol` and `Governance.t.sol` cover replay, roots and fork parents, proof/signature tampering, time/epoch boundaries, timelock access, independent pauses, fee tokens, failure rollback and reentrancy. `WithdrawalInvariant.t.sol` mixes deposits, approvals, claims, reissues, signer rotations, pauses and clock advances, checking token conservation, retained roots and exactly-once payment. The existing deposit and checkpoint suites remain in place.
