# Withdrawal stalls and `checkpoint_backlog`

A withdrawal is not paid by the node. The node approves it, signs an authorization once a
checkpoint that covers it is final on chain, and the vault pays whoever submits the authorization.
Withdrawals therefore stall when no checkpoint can be posted or become final, when the node holds
back its signature because the books and the vault disagree, or when the vault itself is paused.
Trading is not affected in any of these cases. Read [reference.md](reference.md) first.

## How a withdrawal moves

1. The account asks (`POST /v1/withdrawals`, at least 10 USDC). The engine approves it, in order, up
   to the cash available, and the approval is queued (`GET /v1/withdrawals`).
2. Every approval forces a checkpoint. The node signs a checkpoint covering it and posts it to the
   vault through the relayer. It posts **one checkpoint at a time**: a new one waits until the
   previous is final at the `finalized` tag, so the cadence follows the chain's finality. An approval
   made while one is on its way waits for that one, then for its own.
3. When the covering checkpoint is `finalized` and collateral reconciles, the node signs a 24-hour
   authorization and streams it. Anyone may submit it to the vault, which pays the account. While a
   daily turn is in effect while rotation is on, and its rotation set names two or more members, the
   node also waits until
   the boundary's log check has verified the log through the approval ("The boundary's log check").
4. An authorization that expires unclaimed is not signed again by itself. Its account asks for a
   new one (`POST /v1/accounts/{address}/withdrawals/{nonce}/reissue`, signed by the account).

**How long that takes.** On Arbitrum One `finalized` follows the posting of a block's batch to
Ethereum by about one Ethereum finality period (about 15 minutes, `custody.rs`). Simulated with
Ethereum's 13-minute finality and `checkpoint_every = 1000`, an approval is authorised 13 to 27
minutes later (#469). That was not measured on Arbitrum One, where the gap until the batch is posted
adds to it. A withdrawal that is still inside that wait is not a stall; tell the account the wait
and check again. Replace this paragraph's numbers with measured ones after launch.

## Find the cause

Start from `engine_withdrawals_paused` and the open alerts.

```sh
curl -s http://<metrics host>:9464/metrics | grep -E '^engine_(withdrawals_paused|withdrawals_verdict_seq|last_checkpoint_timestamp_seconds) '
cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC                       # latest posted
cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC --block finalized      # posted and final
cast call $VAULT 'checkpointSigner()(address)' --rpc-url $RPC
cast call $VAULT 'withdrawalsPaused()(bool)' --rpc-url $RPC
```

| You see | It means | Go to |
| --- | --- | --- |
| `engine_withdrawals_paused 1` just after a start | The first reconciliation has not run. | Wait a few minutes. |
| `engine_withdrawals_paused 0`, the account waits | A normal wait, or the account's authorization expired. | "How long that takes", then the account reissues. |
| `vault_withdrawals_halted` | The vault itself refuses: paused, or no signer. | "The vault is paused or has no signer" |
| `collateral_pending_deposits` | The checkpoint's deposit coverage is unproven. | "Collateral does not reconcile" |
| `collateral_mismatch` | The books and the vault disagree. | "Collateral does not reconcile" |
| `checkpoint_withheld` | The node is not signing or posting a checkpoint; the `cause` field says why. | "Checkpoints are held" |
| `checkpoint_backlog` | No checkpoint became final for too long, or the posting queue is full. | "`checkpoint_backlog`" |
| `checkpoint_post_failed` | A post failed and is retried every `retry_interval`. | "Posting fails" |
| `checkpoint_conflict`, `vault_exit_started` | The node has stopped itself. | "The node has stopped" |
| `custody_deposits_stalled`, `custody_finality_stalled`, `custody_head_stale`, `custody_rpc_quorum_lost` | The chain reads fail or lag. | [feed-and-custody-failures.md](feed-and-custody-failures.md) |
| Clients receive `payout_unavailable` | A new withdrawal is refused while the gate is paused. | The row for the alert that is open. |
| The account's payouts read names a `check` | The approval waits for that boundary's log check. | "The boundary's log check" |
| `log_check_pending` | No member has verified the log at a boundary the node holds on through. | "The boundary's log check" |

## The vault is paused or has no signer

`vault_withdrawals_halted` has two causes, in its `cause` field.

- **`paused`.** The guardian paused withdrawals (seven days; it cannot extend its own pause) or the
  timelock did (until the timelock lifts it). The node signs no authorization, first or reissued, and
  checkpoints keep posting. Find who and why before anything else:
  `cast call $VAULT 'withdrawalsPausedUntil()(uint256)' --rpc-url $RPC`; a value of
  `115792089237316195423570985008687907853269984665640564039457584007913129639935` is the timelock's
  pause. It resolves when the pause ends or the timelock lifts it with
  `setPauses(false,false)` (pattern in reference.md). **Changes the live system.** Needs the
  owner's approval.
- **`signer_retired`.** The guardian cleared the signer. The vault accepts no checkpoint, so the
  node signs none (`checkpoint_withheld`, cause `signer_retired`) and keeps sequencing. It resolves
  when the timelock installs a signer. The same node's key is installed with
  `setSigner(<its address>)`; another key stops the node with `checkpoint_conflict`. See
  [key-recovery.md](key-recovery.md), "New keys".

## Collateral does not reconcile

The node compares the vault with the finalized checkpoint's ledger. It pauses withdrawals, not
trading, when they differ.

- **`collateral_pending_deposits` (Warning).** The finalized checkpoint's deposit coverage has no
  sequenced canonical receipts behind it. Checkpoints keep being signed; a newer one with a proven
  prefix clears it. If it persists, a deposit scan is not running: check `custody_deposits_lagging`
  and `custody_deposits_stalled`.
- **`collateral_mismatch` (Critical) naming "still uncredited after the catch-up deadline".** A
  deposit the vault holds has not been credited in `custody.evm.stall_critical_after`. If the chain
  is healthy and this fires within minutes of an ordinary deposit, the deadline is shorter than the
  credit tag's lag plus one finality delay plus the wait for the next checkpoint. That is a config
  value, so the fix is a new release ([deploy-and-rollback.md](deploy-and-rollback.md)); until then
  withdrawals pause for every deposit's wait. If the chain is not healthy, fix that first.
- **`collateral_mismatch` naming "paid more in distributions than the engine allocated".** The vault
  has paid stakers more than the engine's ledger says. Stop and call the owner.
- **`collateral_mismatch` naming "holds less USDC than the ledger requires, or its deposits or
  payments differ".** Read the vault against the log before anything else:

  ```sh
  cast call $VAULT 'totalDeposited()(uint256)' --rpc-url $RPC
  cast call $VAULT 'totalWithdrawn()(uint256)'  --rpc-url $RPC
  cast call $VAULT 'distributionsPaid()(uint256)' --rpc-url $RPC
  cast call $USDC 'balanceOf(address)(uint256)' $VAULT --rpc-url $RPC
  ```

  A shortfall is a possible loss of funds or a fault in the books. Do not try to repair it. Make
  the markets reduce-only ([reduce-only.md](reduce-only.md)), keep the log and the console output,
  and call the owner. [incident-response.md](incident-response.md) covers what the owner may decide
  (a guardian pause, the exit).
- **`collateral_surplus` (Warning).** The vault holds USDC that no deposit brought in. Nothing
  pauses. Anyone may book it as house capital: `cast send $VAULT 'bookSurplus()' --rpc-url $RPC
  --account <any>`. **Changes the live system.** Needs the owner's approval of this contract call: it
  creates house capital. A surplus under 10 USDC reverts with `NoSurplus` and must be topped up
  first.

## Checkpoints are held

`checkpoint_withheld` (Warning) is one episode per `cause`, resolved when the hold clears.

| `cause` | Meaning | Do |
| --- | --- | --- |
| `collateral` | No checkpoint signed at seq N: collateral is unvalidated or diverged. | The `collateral_mismatch` rows above. |
| `custody_prefix` | The custody prefix awaits a canonical scan. | `collateral_pending_deposits`, above. |
| `post_diverged` | A checkpoint is not posted while the reconciliation is diverged. | The `collateral_mismatch` rows above. |
| `signer_retired` | The vault's signer is retired. | "The vault is paused or has no signer". |

## `checkpoint_backlog`

A Warning with two meanings, both of which leave the node sequencing and approvals waiting for the
next checkpoint:

- **"none has become final for …"**: a checkpoint was held back, waiting for the previous one to
  become final, for longer than `custody.evm.finality_stalled_after` (30 minutes in the template),
  and none became final. Normal waits are about one finality delay, so this is a stuck chain
  or a stuck post.
- **"the posting queue is full"**: checkpoints are being signed faster than they can be posted.

Diagnose in this order:

1. Is the chain's `finalized` tag advancing? `cast block finalized --rpc-url $RPC` twice, a few
   minutes apart; the block number must rise. If not, `custody_finality_stalled` fires too: the
   problem is the chain or the provider quorum, not the node. Query another provider.
2. Is the last posted checkpoint on chain? Compare `latestSeq()` at the latest block and at
   `finalized` (the commands above). Latest ahead of finalized by one checkpoint is a normal wait.
   Latest equal to the last checkpoint the node has signed, not yet final, means the post is mined
   and the chain has not finalized it.
3. Is the post stuck? If `latestSeq()` at the latest block is *behind* the node's newest signed
   checkpoint, the relayer's transaction has not been mined. Compare
   `cast nonce <relayer> --block pending` with `cast nonce <relayer> --block latest` (the
   address is `checkpoints.relayer`). A pending nonce above the latest one is a transaction waiting
   in the pool. `checkpoint_post_failed` carries the node's reason. Causes: the relayer has no gas
   (fund it), the endpoint is down ([feed-and-custody-failures.md](feed-and-custody-failures.md)),
   or the fee is too low.
4. A low fee: the node sends no fee bump and no cancel; it pauses posting and waits for you. Speed
   it up at the endpoint by re-sending **the same transaction** (same recipient, data and nonce)
   with a higher fee. Do not replace it with a different transaction, such as a zero-value transfer
   to cancel: the node refuses a nonce consumed by anything that is not its own call.
   **Changes the live system.** Needs the owner's approval; it spends the relayer's funds.

The alert resolves when the next signed checkpoint is accepted.

## Posting fails

`checkpoint_post_failed` (Warning) means a post failed and is retried every
`checkpoints.retry_interval`; the checkpoint stays queued and signing continues. The alert carries
the node's reason:

- A transport or send error: the checkpoint RPC endpoint is down or the relayer cannot pay
  ([feed-and-custody-failures.md](feed-and-custody-failures.md), "Checkpoint endpoint").
- A conflicting posted root or signer, or a vault checkpoint this node did not sign: seen once, it
  is retried as a failed post, because one read can be torn across a block the endpoint is still
  mining. If the next retry's fresh read shows it again, the node stops with `checkpoint_conflict`.
- Pending work at the relayer that is not the node's, or a nonce used by another transaction: posting
  pauses until you act ("`checkpoint_backlog`", step 4).

## The boundary's log check

While rotation is on and a daily turn is in effect whose rotation set names two or more members, the node signs an
authorization only for an approval at or below the head of the log the last boundary's check
verified. `GET /v1/accounts/{address}/payouts` names, for each approval still waiting, the turn and
`boundary_ms` of the check that will cover it: the turn in effect's, while its check is still
outstanding and will reach the approval, otherwise the next turn's. An approval made during a turn
therefore waits until the next boundary, a day at most plus the switch. That wait is the rule, not
a stall.

- **At a handover** the successor's node is the check: it verified the log through the checkpoint it
  was handed before it started. Nothing to do.
- **At a boundary the operator holds on through** the operator's node asks the members of the
  turn's order, first to last, for an attested verification of the checkpoint it wrote after its
  `TurnTaken`. It skips its own candidate, each member with no handover endpoint, and each member
  whose release sequenced part of the range since the last verified head. `log_check_pending` pages
  when no member has verified it for 30 minutes. Check the members' nodes follow the operator and that
  their registered handover endpoints are reachable from the operator's node; a member answers only
  once its replica has verified that checkpoint, so one that lags answers once it catches up.
- **After a restart** the node restores the verified head sealed with its keys, if its log holds that
  checkpoint; otherwise it asks for the check again as at a boundary it holds on through.

## The node has stopped

- **`checkpoint_conflict` (Critical).** The vault holds a root or signer the log does not. Either a
  second copy of the node is running from a copy of the disk (every copy but one stops with this),
  or the log or the vault's view was altered. Do not restart the stopped node. Find the other copy
  first; if there is none, this is an integrity incident ([incident-response.md](incident-response.md)).
- **`vault_exit_started` (Critical).** The vault's exit against the last posted root has begun. No
  checkpoint can follow it: signed withdrawals, checkpoints and distribution claims have ended for
  good. Accounts exit with proofs ([incident-response.md](incident-response.md), "The exit").
