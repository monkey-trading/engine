# Withdrawal stalls and `checkpoint_backlog`

Use this runbook when an account's withdrawal is not being paid, when `engine_withdrawals_paused`
reads 1, or when one of the alerts in [Find the cause](#find-the-cause) fires. The trading server
does not pay a withdrawal itself. It approves the withdrawal, and once a checkpoint that covers the
approval is final on chain, it signs an authorisation. The vault then pays whoever submits that
authorisation.

- [Find the cause](#find-the-cause)
- [The vault is paused or has no signer](#the-vault-is-paused-or-has-no-signer)
- [Collateral does not reconcile](#collateral-does-not-reconcile)
- [Checkpoints are held](#checkpoints-are-held)
- [`checkpoint_backlog`](#checkpoint_backlog)
- [Posting fails](#posting-fails)
- [The boundary's log check](#the-boundarys-log-check)
- [The trading server has stopped](#the-trading-server-has-stopped)
- [How a withdrawal moves](#how-a-withdrawal-moves)

Withdrawals therefore stall in three cases. No checkpoint can be posted or become final. The trading
server holds back its signature because its books and the vault disagree. Or the vault itself is
paused. Trading continues in every one of these cases.

The steps use the shell variables that [Variables](reference.md#variables) sets up. Commands and
their output call a running trading server a node.

## Find the cause

Use this procedure first, whenever a withdrawal seems stuck.

1. Read the trading server's withdrawal gate and its last checkpoint:

   ```shell
   curl -s http://<metrics host>:9464/metrics | grep -E '^engine_(withdrawals_paused|withdrawals_verdict_seq|last_checkpoint_timestamp_seconds) '
   ```

   `<metrics host>` is the address of the trading server's metrics listener. The command prints
   three lines. `engine_withdrawals_paused 0` means that authorisations are being signed.

2. Read the vault's latest checkpoint, at the latest block and at the `finalized` block:

   ```shell
   cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC
   cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC --block finalized
   ```

   The first number is the sequence number of the last posted checkpoint. The second is the last one
   that is final.

3. Read the vault's signer and its pause:

   ```shell
   cast call $VAULT 'checkpointSigner()(address)' --rpc-url $RPC
   cast call $VAULT 'withdrawalsPaused()(bool)' --rpc-url $RPC
   ```

   A signer of `0x0000000000000000000000000000000000000000` means that the guardian retired it.
   `true` means that withdrawals are paused.

4. Find what you see in the table, together with the open alerts, and go to the section it names.

| What you see | What it means | Where to go |
| --- | --- | --- |
| `engine_withdrawals_paused 1` just after a start | The first reconciliation has not run yet. | Wait a few minutes, then read the metric again. |
| `engine_withdrawals_paused 0`, and the account is still waiting | A normal wait, or the account's authorisation expired unclaimed. | [How long a withdrawal takes](#how-long-a-withdrawal-takes). An expired authorisation is signed again only when the account asks for it. |
| `vault_withdrawals_halted` | The vault refuses withdrawals, because it is paused or has no signer. | [The vault is paused or has no signer](#the-vault-is-paused-or-has-no-signer) |
| `collateral_pending_deposits` | The checkpoint's deposit coverage is not yet proven. | [Collateral does not reconcile](#collateral-does-not-reconcile) |
| `collateral_mismatch` | The books and the vault disagree. | [Collateral does not reconcile](#collateral-does-not-reconcile) |
| `checkpoint_withheld` | The trading server is not signing or posting a checkpoint. The alert's `cause` field says why. | [Checkpoints are held](#checkpoints-are-held) |
| `checkpoint_backlog` | No checkpoint has become final for too long, or the posting queue is full. | [`checkpoint_backlog`](#checkpoint_backlog) |
| `checkpoint_post_failed` | A post failed, and it is retried every `checkpoints.retry_interval`. | [Posting fails](#posting-fails) |
| `checkpoint_conflict` or `vault_exit_started` | The trading server has stopped itself. | [The trading server has stopped](#the-trading-server-has-stopped) |
| `custody_deposits_stalled`, `custody_finality_stalled`, `custody_head_stale` or `custody_rpc_quorum_lost` | Reads of the chain are failing or lagging. | [Custody RPC providers](feed-and-custody-failures.md#custody-rpc-providers) |
| Clients receive `payout_unavailable` | New withdrawals are refused while the gate is paused. | The row for the alert that is open. |
| The account's payouts read names a `check` | The approval is waiting for that boundary's log check. | [The boundary's log check](#the-boundarys-log-check) |
| `log_check_pending` | No member has verified the log at a boundary through which the operator kept its turn. | [The boundary's log check](#the-boundarys-log-check) |

## The vault is paused or has no signer

Use this procedure when `vault_withdrawals_halted` fires. The trading server signs no authorisation,
neither a first one nor a reissued one, while the alert is open. The alert's `cause` field names one
of two causes.

### The cause is `paused`

The guardian or the timelock has paused withdrawals. A pause by the guardian lasts seven days, and
the guardian cannot extend its own pause. A pause by the timelock lasts until the timelock lifts it.
Checkpoints continue to be posted during either pause.

1. Find out who paused withdrawals, and why, before you do anything else. Read when the pause ends:

   ```shell
   cast call $VAULT 'withdrawalsPausedUntil()(uint256)' --rpc-url $RPC
   ```

   The command prints the Unix time, in seconds, at which the pause ends. The value
   `115792089237316195423570985008687907853269984665640564039457584007913129639935` means that the
   timelock paused withdrawals, and that the pause lasts until the timelock lifts it.

2. To lift the pause before it ends, have the timelock call `setPauses(false,false)`, as
   [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock)
   describes.

   **Changes the live system.** The owner must approve this timelock call.

**How to tell it is resolved.** The alert resolves when the pause ends, or when the timelock lifts
it.

### The cause is `signer_retired`

The guardian has cleared the vault's signer. The vault accepts no checkpoint, so the trading server
signs none and raises `checkpoint_withheld` with the cause `signer_retired`. The trading server
keeps sequencing instructions.

1. Have the timelock install a signer, as [New keys](key-recovery.md#new-keys) describes. To install
   the same trading server's key again, the timelock calls `setSigner(<address>)`, where `<address>`
   is that key's address.

   **Changes the live system.** The owner must approve this timelock call.

   **If the timelock installs any other key, the running trading server stops with
   `checkpoint_conflict`.**

**How to tell it is resolved.** The alert resolves once the timelock's call is final and the vault
names a signer.

## Collateral does not reconcile

Use this procedure when `collateral_pending_deposits`, `collateral_mismatch` or `collateral_surplus`
fires. The trading server compares the vault with the ledger of the last finalised checkpoint. When
the two differ, it pauses withdrawals, but trading continues. Each alert's message names the case.

### `collateral_pending_deposits`

This Warning means that the finalised checkpoint covers deposits for which no canonical receipts
have been sequenced. Checkpoints continue to be signed, and a newer checkpoint whose deposit prefix
is proven clears the alert.

1. If the alert persists, check whether a deposit scan has stopped. Look for
   `custody_deposits_lagging` and `custody_deposits_stalled`, and follow
   [Custody RPC providers](feed-and-custody-failures.md#custody-rpc-providers) if either is open.

### `collateral_mismatch` naming "still uncredited after the catch-up deadline"

A deposit that the vault holds has not been credited within `custody.evm.stall_critical_after`.

1. If the chain is not healthy, fix the chain reads first, as
   [Custody RPC providers](feed-and-custody-failures.md#custody-rpc-providers) describes.
2. If the chain is healthy and the alert fires within minutes of an ordinary deposit, the deadline
   is too short. It must exceed the credit tag's lag, plus one finality delay, plus the wait for the
   next checkpoint. The deadline is a configuration value, so the fix is a new release, as
   [Deploy and rollback](deploy-and-rollback.md) describes. Until that release runs, withdrawals
   pause while each deposit waits.

### `collateral_mismatch` naming "paid more in distributions than the engine allocated"

The vault has paid stakers more than the engine's ledger allocated.

1. Stop, and call the owner.

### `collateral_mismatch` naming "holds less USDC than the ledger requires, or its deposits or payments differ"

1. Read the vault's totals and its USDC balance, so that you can compare them with the log:

   ```shell
   cast call $VAULT 'totalDeposited()(uint256)' --rpc-url $RPC
   cast call $VAULT 'totalWithdrawn()(uint256)' --rpc-url $RPC
   cast call $VAULT 'distributionsPaid()(uint256)' --rpc-url $RPC
   cast call $USDC 'balanceOf(address)(uint256)' $VAULT --rpc-url $RPC
   ```

   Each command prints an amount in USDC base units, of which there are 1,000,000 to one USDC.

2. Make every market reduce-only, as [Make markets reduce-only](reduce-only.md) describes.

   **A shortfall is a possible loss of funds or a fault in the books. Do not try to repair it.**

3. Keep the log and the console output, as
   [Keep the evidence](incident-response.md#keep-the-evidence) describes, and call the owner.
   [Funds may be at risk](incident-response.md#funds-may-be-at-risk) covers what the owner may
   decide, such as a guardian pause or the exit.

### `collateral_surplus`

This Warning means that the vault holds USDC that no deposit brought in. Nothing is paused.

1. Anyone may book the surplus as house capital:

   ```shell
   cast send $VAULT 'bookSurplus()' --rpc-url $RPC --account <account>
   ```

   `<account>` is any Foundry keystore account that can pay for gas.

   **Changes the live system.** The owner must approve this contract call, because it creates house
   capital. A surplus below 10 USDC reverts with `NoSurplus`, and must be topped up to at least 10
   USDC first.

**How to tell it is resolved.** Each alert resolves once a reconciliation finds the vault and the
ledger in agreement. `engine_withdrawals_paused` then reads 0.

## Checkpoints are held

Use this procedure when `checkpoint_withheld` fires. The Warning opens one episode for each `cause`,
and each episode resolves when its hold clears.

| `cause` | What it means | What to do |
| --- | --- | --- |
| `collateral` | No checkpoint was signed at the sequence number in the message, because collateral is not yet validated or has diverged. | Follow the `collateral_mismatch` cases in [Collateral does not reconcile](#collateral-does-not-reconcile). |
| `custody_prefix` | The custody prefix is waiting for a canonical scan, or a deposit was reversed and the next finalised custody frontier has not yet applied. | Follow [`collateral_pending_deposits`](#collateral_pending_deposits). After `deposit_reversed`, the hold clears at the next finalised frontier. If it does not, follow the `custody_deposits_stalled` and `custody_finality_stalled` rows in [Find the cause](#find-the-cause). |
| `post_diverged` | A checkpoint is not posted while the reconciliation has diverged. | Follow the `collateral_mismatch` cases in [Collateral does not reconcile](#collateral-does-not-reconcile). |
| `signer_retired` | The vault's signer has been retired. | Follow [The cause is `signer_retired`](#the-cause-is-signer_retired). |

## `checkpoint_backlog`

Use this procedure when `checkpoint_backlog` fires. The trading server keeps sequencing, and
approvals wait for the next checkpoint. The Warning's message names one of two cases:

- **"none has become final for …"**. A checkpoint has been held back, waiting for the previous one
  to become final, for longer than `custody.evm.finality_stalled_after`, which is 30 minutes in the
  production template, and no checkpoint has become final. A normal wait is about one finality
  delay, so either the chain or a post is stuck.
- **"the posting queue is full"**. Checkpoints are being signed faster than they can be posted.

Work through the steps in order.

1. Check that the chain's `finalized` block is advancing. Run the command twice, a few minutes
   apart:

   ```shell
   cast block finalized --rpc-url $RPC
   ```

   The block number must rise. If it does not, `custody_finality_stalled` fires too. The problem is
   then the chain or the provider quorum, not the trading server. Query another provider.

2. Check whether the last posted checkpoint is on chain. Compare `latestSeq()` at the latest block
   with `latestSeq()` at the `finalized` block, using the commands in step 2 of
   [Find the cause](#find-the-cause).

   The latest block being one checkpoint ahead of the `finalized` block is a normal wait. When the
   latest block holds the trading server's newest signed checkpoint and that checkpoint is not yet
   final, the post has been mined and the chain has not yet finalised it.

3. If `latestSeq()` at the latest block is behind the trading server's newest signed checkpoint,
   check whether the relayer's transaction is waiting to be mined:

   ```shell
   cast nonce <relayer> --block pending --rpc-url $RPC
   cast nonce <relayer> --block latest --rpc-url $RPC
   ```

   `<relayer>` is the address in `checkpoints.relayer`. A pending nonce above the latest nonce means
   that a transaction is waiting in the pool. `checkpoint_post_failed` carries the trading server's
   reason. The usual causes are a relayer with no gas, which you fund, an endpoint that is down, as
   [Checkpoint endpoint](feed-and-custody-failures.md#checkpoint-endpoint) describes, or a fee that
   is too low.

4. If the fee is too low, speed the transaction up at the endpoint. The trading server sends no fee
   increase and no cancellation. It pauses posting and waits for you. Send the same transaction
   again, with the same recipient, data and nonce, and a higher fee.

   **Changes the live system.** The owner must approve this transaction, because it spends the
   relayer's funds.

   **Do not replace the transaction with a different one, such as a zero-value transfer to cancel
   it. The trading server refuses a nonce that any transaction other than its own call has used.**

**How to tell it is resolved.** The alert resolves when the vault accepts the next signed
checkpoint.

## Posting fails

Use this procedure when `checkpoint_post_failed` fires. A post failed, and the trading server
retries it every `checkpoints.retry_interval`. The checkpoint stays queued, and signing continues.

1. Read the reason in the alert's message, and act on it:

   - **A transport or send error.** The checkpoint endpoint is down, or the relayer cannot pay.
     Follow [Checkpoint endpoint](feed-and-custody-failures.md#checkpoint-endpoint).
   - **A conflicting posted root or signer, or a vault checkpoint that this trading server did not
     sign.** The first time, the trading server retries the post as a failed one, because a single
     read can be torn across a block that the endpoint is still mining. If the next retry's fresh
     read shows the same conflict, the trading server stops with `checkpoint_conflict`. Follow
     [The trading server has stopped](#the-trading-server-has-stopped).
   - **Pending work at the relayer that is not the trading server's, or a nonce that another
     transaction has used.** Posting pauses until you act. Follow step 4 of
     [`checkpoint_backlog`](#checkpoint_backlog).

**How to tell it is resolved.** The alert resolves when a post succeeds.

## The boundary's log check

Use this procedure when an account's payouts read names a `check` for a waiting approval, or when
`log_check_pending`, `log_check_unattributable`, `log_check_unavailable` or
`log_check_awaiting_installation` fires.

The check applies while a daily turn is in effect whose rotation set names two or more members.
While it applies, the trading server signs an authorisation only for an approval at or below the
head of the log that the last boundary's check verified. An approval made during a turn therefore
waits until the next boundary, which is at most a day plus the time a handover takes. That wait is
the rule, not a stall. Turning rotation off leaves the drawn turns in effect, and the check with them,
until the timelock's installation of an operator ends the turns, so an approval made during the last
drawn turn waits for that installation. Its payouts read then shows `waits` as `installation`, and
`log_check_awaiting_installation` pages while any approval waits so.

1. Find which check each of an account's waiting approvals depends on:

   ```shell
   curl -sf $API/v1/accounts/<address>/payouts | jq .
   ```

   `<address>` is the account's address. For each approval that is still waiting, the answer's
   `check` says what will cover it. With `waits` as `boundary`, it names the turn and the
   `boundary_ms` of that check: the check of the turn in effect, if that check is still outstanding
   and will reach the approval, and otherwise the check of the next turn. With `waits` as
   `installation`, rotation is off and no turn after the last drawn one is assigned, so the approval
   waits for the timelock's installation of an operator.

2. Find how the check runs at that boundary, and act on it:

   - **Waiting for the installation,** no boundary will check the approval.
     `log_check_awaiting_installation` fires while one waits. Carry on with the governance fallback
     in [When no standby qualifies](standby.md#when-no-standby-qualifies): the approvals are signed
     once the installation ends the turns.
   - **At a handover,** the successor's trading server is the check. It verified the log through the
     checkpoint it was handed before it started. Nothing needs to be done.
   - **At a boundary through which the operator keeps its turn,** the operator's trading server asks
     the members of the turn's order, first to last, to verify the checkpoint that it wrote after
     its `TurnTaken`. Its enclave signs each question, and each answer is an attested verification.
     It skips its own candidate, each member with no handover endpoint, and each member that
     sequenced part of the range since the last verified head under any release, or that runs a
     release that did. `log_check_pending` fires when no member has verified the checkpoint for 30
     minutes. Check that the members' trading servers follow the operator, and that their registered
     handover endpoints are reachable from the operator's trading server. A member answers only once
     its replica has verified that checkpoint, so a member that lags answers once it catches up.
   - **After a restart,** the trading server restores the verified head that it sealed with its
     keys, if its log holds that checkpoint. Otherwise, it asks for the check again, as at a
     boundary through which the operator keeps its turn.
   - **After a start from a snapshot past the verified head,** the trading server cannot tell who
     sequenced the range, so it asks no member, and `log_check_unattributable` fires. Restart the
     trading server without `[snapshots]`, so that it replays the whole log.

     **Changes the live system.** The owner must approve the restart.

   - **With no verified head at all,** as when the host has kept back the sealed one, the next check
     covers the latest holding, so it needs a member other than the one that held that turn.
   - **When `log_check_unavailable` fires,** this trading server runs without
     `operator.rotation_enabled` or without `[tee.sealing]` while the check applies, so it can
     neither take a turn nor ask for a check. Restart it with both set, as
     [Daily turns](operator-candidate.md#daily-turns) describes for rotation-set members.

     **Changes the live system.** The owner must approve the restart.

**How to tell it is resolved.** `log_check_pending` resolves once a member verifies the checkpoint,
and the waiting approvals are then signed. `log_check_unattributable` resolves once the check can be
asked for or is no longer due. `log_check_unavailable` resolves once the check no longer applies,
for example once an installation ends the turns.

## The trading server has stopped

Use this procedure when `checkpoint_conflict` or `vault_exit_started` fires. Both are Critical, and
in both cases the trading server stops itself.

- **`checkpoint_conflict`.** The vault holds a root or a signer that the log does not hold. Either a
  second copy of the trading server is running from a copy of the disk, in which case every copy but
  one stops with this alert, or the log or the vault's view has been altered.

  1. Find the other copy before you do anything else. The host must show which virtual machines are
     running.

     **Do not restart the stopped trading server until the other copy has been found and stopped.**

  2. If there is no other copy, treat it as an integrity incident, as
     [The books look wrong](incident-response.md#the-books-look-wrong) describes.

- **`vault_exit_started`.** The vault's exit against the last posted root has begun. No checkpoint
  can follow it, so signed withdrawals, checkpoints and distribution claims have ended for good.
  Accounts recover their funds with proofs, as
  [Checkpoints cannot resume](incident-response.md#checkpoints-cannot-resume) describes.

## How a withdrawal moves

1. The account asks for a withdrawal with `POST /v1/withdrawals`, for at least 10 USDC. The engine
   approves withdrawals in order, up to the cash available, and queues each approval. The queue is
   public at `GET /v1/withdrawals`.
2. Every approval forces a checkpoint. The trading server signs a checkpoint that covers the
   approval, and posts it to the vault through the relayer. It posts one checkpoint at a time. A new
   checkpoint waits until the previous one is final at the `finalized` block, so the pace of
   checkpoints follows the chain's finality. An approval made while a checkpoint is on its way waits
   for that checkpoint, and then for its own.
3. When the covering checkpoint is final and collateral reconciles, the trading server signs an
   authorisation that is valid for 24 hours, and streams it to the account. Anyone may submit the
   authorisation to the vault, which pays the account. While the boundary's log check applies, the
   trading server also waits until that check has verified the log through the approval, as
   [The boundary's log check](#the-boundarys-log-check) describes.
4. An authorisation that expires unclaimed is not signed again automatically. Its account asks for a
   new one with `POST /v1/accounts/{address}/withdrawals/{nonce}/reissue`, which the account signs.

### How long a withdrawal takes

On Arbitrum One, a block reaches the `finalized` tag about one Ethereum finality period, around 15
minutes, after its batch is posted to Ethereum. A simulation that assumed Ethereum's 13-minute
finality and `checkpoint_every = 1000` found that an approval is authorised 13 to 27 minutes after
it is made. The wait has not been measured on Arbitrum One, where the time until a block's batch is
posted comes on top. Measure the wait on the launch deployment, and replace these figures with
the measurement.

A withdrawal that is still within that wait is not a stall. Tell the account how long the wait is,
and check again afterwards.
