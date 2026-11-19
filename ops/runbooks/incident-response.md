# Incident response

Use this runbook for an alert you do not recognise, for an alert that its own runbook does not
resolve, and for anything that looks like an attack. In the first minutes, your aims are to find
which class of problem it is, to keep the books and the funds safe with the least drastic step that
works, and to keep the evidence.

- [Contacts](#contacts)
- [First five minutes](#first-five-minutes)
- [The ladder of containment](#the-ladder-of-containment)
- [Tell users](#tell-users)
- [Keep the evidence](#keep-the-evidence)
- [By class](#by-class): [The books look wrong](#the-books-look-wrong),
  [Funds may be at risk](#funds-may-be-at-risk), [Prices look wrong](#prices-look-wrong),
  [The host misbehaves](#the-host-misbehaves),
  [Checkpoints cannot resume](#checkpoints-cannot-resume),
  [Pages stop arriving](#pages-stop-arriving)
- [After the incident](#after-the-incident)

Find each alert in [Alerts](alerts.md) first. The steps use the shell variables that
[Variables](reference.md#variables) sets up. Commands and their output call a running trading server
a node.

## Contacts

Fill in this table before launch, and keep a copy outside the systems it describes. Nothing in the
trading server decides who is called.

| Role | Name and number | Approves or does |
| --- | --- | --- |
| Deployment owner (OWNER) | | Approves changes to the trading server, its configuration and its administrative instructions, and every step marked as changing the live system. |
| Multisig signers (OWNER) | | Approve timelock proposals, and the guardian's pause and signer retirement. |
| Operator on call (OWNER) | | Runs the commands in these runbooks. |
| Host and network provider (OWNER) | | Controls the data disk, the virtual machine, port 443 and the metrics network. |
| DNS and registrar account holder (OWNER) | | Controls the API domain's records. |

The trading server posts Critical alerts to the `[alerts.critical]` webhook, and Warnings to the
optional `[alerts.warning]` webhook. The Prometheus rules in `ops/prometheus/engine-node-rules.yml`
page through a different receiver when the trading server goes silent, and when its pages cannot be
delivered.

## First five minutes

Use this procedure when a page arrives that its own runbook does not cover, or when something looks
like an attack.

1. Check whether the trading server answers:

   ```shell
   curl -sf --max-time 10 $API/v1/health
   curl -s --max-time 10 http://<metrics host>:9464/metrics | grep -E '^engine_(withdrawals_paused|last_checkpoint_timestamp_seconds|alert_pages_pending) '
   ```

   `<metrics host>` is the address of the trading server's metrics listener. The first command
   prints `ok`. If it prints nothing, follow [Restart and recovery](restart-and-recovery.md).

2. List the open alerts from the pages you received, and from the console log if you have it. Find
   each alert in [Alerts](alerts.md), and take the first step of its runbook. A Critical page with
   no runbook for its situation is an incident, so continue here.

3. Decide how serious it is, by asking these questions:

   - Can an account, or the house, lose money that it should not? Wrong prices, a wrong book, or an
     administrative instruction that nobody sent are signs of this.
   - Is an account unable to get its money out? Paused withdrawals and stopped checkpoints are signs
     of this.
   - Is a key or a secret exposed? If so, follow [Key recovery](key-recovery.md).
   - Do the public log or the vault disagree with what the trading server reports?
     `checkpoint_conflict`, `collateral_mismatch`, `log_tail_lost` and a failed replay are signs of
     this.

4. Call the owner if the answer to any of the last three questions is yes, and before any step that
   is marked as changing the live system. Have the exact command ready, and the notice that clients
   will see if the step affects them, as [Tell users](#tell-users) describes.

5. Keep the evidence before you restart or change anything, as
   [Keep the evidence](#keep-the-evidence) describes.

## The ladder of containment

Take the least drastic step that stops the harm. Every step except the first two changes the live
system and needs the owner's approval for that action. The guardian's steps also need the multisig.

| Step | Effect | Reversible | How |
| --- | --- | --- | --- |
| Observe | Nothing changes. | Not applicable | The checks in [First five minutes](#first-five-minutes). |
| Make one market, or every market, reduce-only | Orders that would open or increase a position are refused. Orders that reduce or close a position are still executed. | Yes, at once | [Make markets reduce-only](reduce-only.md) |
| Post a status notice | One short message is shown to every client. Nothing else changes. | Yes, at once | `$ENGINE admin … set-notice "<text>"` and `clear-notice`, with the owner's approval, as [Tell users](#tell-users) describes. |
| Raise the stress floor | Spreads widen. The floor is at most level 1, and it lapses 72 hours after it is set, unless it is set again. | Yes | `$ENGINE admin … set-stress-floor <level> --market <id>`, with the owner's approval. Without `--market`, the floor applies to every market. |
| The guardian calls `pauseWithdrawals()` | No withdrawal and no distribution claim is paid for seven days. Trading, checkpoints and deposits continue. | The timelock lifts it with `setPauses`, and it also ends after seven days. | `cast send $VAULT 'pauseWithdrawals()' --rpc-url $RPC --account <guardian>` |
| The timelock calls `setPauses(depositsPaused, withdrawalsPaused)` | Deposits, withdrawals or both are paused until the timelock lifts the pause. | Yes, through the timelock | [Calling a contract through the timelock](reference.md#calling-a-contract-through-the-timelock), with a delay of three days. |
| The guardian calls `retireSigner()` | Until the timelock installs a signer, the vault accepts no checkpoint, and no withdrawal signed in the current epoch. | Only when the timelock installs a new signer | [New keys](key-recovery.md#new-keys) |
| Stop the trading server by powering off its virtual machine | Everything stops, including trading, checkpoints and the API. The vault's deadlines begin to run. | Restart it | [Restart and recovery](restart-and-recovery.md) |

A guardian step takes effect immediately, and it is the quickest lever on chain. A timelock step
takes at least three days. The guardian cannot extend its own pause. Use the guardian's pause when
funds are leaving wrongly, or may be, and not for a price feed outage.

## Tell users

Use this procedure whenever a containment step affects clients, such as a withdrawal pause or
markets made reduce-only. Tell clients at once, in neutral wording, on the announcement channel and
in the console. The console shows the operator's status notice, which you post as follows.

1. Write the notice. The text is 1 to 280 characters of plain text, with no markup, link or line
   break, and no space at its start or end. The command refuses anything else before it signs.

   - State what clients need to know, and nothing about the cause. Say what is paused or restricted,
     and what is unaffected. Say when the next update will come, if you know.
   - Do not name an attacker, a suspected cause or an amount.

2. Post the notice together with the containment step, not after it.

   **Changes the live system.** The owner must approve this administrative instruction.

   ```shell
   $ENGINE admin --config $CONFIG --node $API --pin $PIN set-notice "Security maintenance: withdrawals are paused."
   ```

   The command reads the operator's key as in [Variables](reference.md#variables). A new text
   replaces the notice in force. The last line of the output is `[{"type":"admin_applied"}]`.

3. Confirm that the trading server serves the notice:

   ```shell
   curl -sf $API/v1/notice | jq .
   ```

   The answer shows the notice's `text` and the time at which it was set, `set_at_ms`.

4. Replace the notice whenever the situation changes, by posting a new text as in step 2.

5. Remove the notice as soon as the containment step is lifted.

   **Changes the live system.** The owner must approve this administrative instruction.

   ```shell
   $ENGINE admin --config $CONFIG --node $API --pin $PIN clear-notice
   ```

   The engine refuses to clear a notice when none is in force, which is harmless. Afterwards,
   `curl -sf $API/v1/notice` prints `{"notice":null}`.

The notice is an administrative instruction that the trading server sequences. Every trading server
therefore serves it signed, at `/v1/notice` and on the signed stream when the client asks for
`notice=1`. A handover or a restart carries the notice with the log, and the public log shows who
set it and when. The notice moves no value, so the operator in office may set it while votes govern.

**The notice needs a trading server that is sequencing. While sequencing is frozen or halted, for
example by `log_rollback`, a trusted-time halt or a held log sync, or while the trading server is
down, the command is refused or cannot reach it. The announcement channel is then the only way to
tell clients.**

## Keep the evidence

Use this procedure before you restart or change anything, whenever the books, a key or a certificate
may be involved. Note the time at which you take each step.

1. Save the pages you received, with their timestamps, and the console log, which is the virtual
   machine's serial output, as files.

2. Download the public log and replay it, as
   [Download the public log and verify it](reference.md#download-the-public-log-and-verify-it)
   describes. Keep both the download and the output of `replay`.

3. Save the trading server's identity:

   ```shell
   curl -sf $API/v1/attestation > attestation-$(date -u +%Y%m%dT%H%M%SZ).json
   ```

4. Save the state of the vault and the registry. Run every command in
   [Read the vault and the registry](reference.md#read-the-vault-and-the-registry), and keep the
   output together with the block number at which you read it:

   ```shell
   cast block-number --rpc-url $RPC
   ```

5. Save the metrics:

   ```shell
   curl -s http://<metrics host>:9464/metrics > metrics-$(date -u +%Y%m%dT%H%M%SZ).txt
   ```

**Do not delete, truncate or repair anything on the data disk. Do not restart a trading server that
has stopped itself with `checkpoint_conflict`, `log_recovery_failed` or `log_tail_lost` until the
owner has seen the evidence.**

## By class

### The books look wrong

Use this procedure when a replay of the public log fails, when a root does not match the vault's, or
when `log_tail_lost`, `checkpoint_conflict` or `checkpoint_accounting_overflow` fires. Use it also
when an account's balance does not follow from its events.

The log is the record. Replay checks every instruction and checkpoint in it against the rules and
the signer.

1. Replay the downloaded log against the signer:

   ```shell
   $ENGINE replay --log archive --signer $SIGNER
   ```

   The last line begins `verified:`. **Any other result is an integrity incident.**

2. If the replay fails, make every market reduce-only, as [Make markets reduce-only](reduce-only.md)
   describes.

3. Keep the evidence, and call the owner.

Two of these alerts need more:

- **`log_tail_lost`.** Recovery found that the log ends before instructions that signed log heads
  cover, so instructions that clients saw acknowledged are missing from this disk. **Do not start
  another trading server from this disk.** Look for a copy that reaches further, such as a standby's
  copy or a mirror's.
- **`checkpoint_conflict`.** Two copies of the trading server running from one disk cause this
  alert, and every copy but one stops. Find and stop the extra copy. The host must show which
  virtual machines are running.

### Funds may be at risk

Use this procedure when withdrawal authorisations look wrong, when the vault's balance is below the
ledger and `collateral_mismatch` fires, or when an enclave key is exposed.

1. Have the guardian pause withdrawals. The pause takes effect immediately.

   **Changes the live system.** The owner and the multisig must approve this call.

   ```shell
   cast send $VAULT 'pauseWithdrawals()' --rpc-url $RPC --account <guardian>
   ```

2. Make the markets reduce-only, as [Make markets reduce-only](reduce-only.md) describes.

3. For a collateral mismatch, follow
   [Collateral does not reconcile](withdrawal-stalls.md#collateral-does-not-reconcile). For an
   exposed key, follow [Key recovery](key-recovery.md).

A pause does not reduce any account's balance. It delays withdrawals and exits. Weigh that delay
against the harm that the pause prevents.

### Prices look wrong

Use this procedure when a market's price is far from the venues' prices, when a page says that the
venues disagree, or when executions look wrong.

1. If a crypto venue is down or quiet, BTC-USD, ETH-USD and SOL-USD can lose their price. That case
   has its own procedure in
   [A crypto venue is down or quiet](feed-and-custody-failures.md#a-crypto-venue-is-down-or-quiet).

2. Make the market reduce-only, as [Make markets reduce-only](reduce-only.md) describes.

3. If the price itself is wrong and executions continue, raise the market's stress floor, and
   consider the guardian's pause. [Price feeds](feed-and-custody-failures.md#price-feeds) covers the
   feeds.

   **Do not widen `max_divergence_bps`, or change a market's price rules, during an incident.** They
   are protocol parameters, and changing them is the owner's decision, with a notice period that the
   engine enforces. Looser price rules apply only seven days after a listing notice, given with
   `announce-listing --price-rules-from`, or after a vote, and never beyond the market's launch
   rules.

A market's spread bands and margin bands change only seven days after a listing notice given with
`announce-listing`, and only within the market's launch bands. During an incident, you therefore
widen spreads with the stress floor and stop new risk with reduce-only, never by listing the market
again.

While votes govern, as [Parameter votes](operator-candidate.md#parameter-votes) describes, the
engine also refuses the operator's listing notices. The members' fast-path vote is then the way to
raise a margin or a close-out level, to set a stress floor above level 1, or to make a market
reduce-only, and it applies at once. The operator keeps its own level-1 stress floor and its own
reduce-only switch.

The same rules apply to a market's weekend margin, which a notice given with
`announce-listing --weekend-margin-bps` sets. The production settings give no market a weekend
margin. A market's calendar has a bound of its own: a new listing of a listed market may not close it
within seven days at any time its calendar has it open, whatever its weekend margin and whether or not
it holds positions, and for a market with a weekend margin it also may not shorten or remove a weekend
closure that starts within seven days. A new listing that only adds hours, or that closes the market
from seven days on, is not held back by this. Hours once added can be taken back only from seven days
on, so check such a listing before you send it. An unplanned closure at a venue, an early close
included, therefore stays off the calendar: the market has no price while its venues stop quoting,
and making it reduce-only stops new risk.

**Close-outs when a market reopens after a closure are the rule working, not an incident.** Margins
do not rise ahead of a weekend or holiday closure, and no account is closed out because a market
closes. A market can reopen at a different price, which takes some accounts below their close-out
level. Those accounts are closed out on the first prices after the reopening, and the house bears
any loss beyond their balances. A burst of close-outs when a market reopens after a weekend is
therefore expected. It is most likely in WTI, silver, Japan 225 and US Tech 100, whose weekend moves
are the largest against their margin.

### The host misbehaves

Use this procedure when `clock_drift`, `trusted_time_unavailable` or `unattested_certificate` fires,
when feeds go missing, when a start fails with `checkpoint_startup_failed` because the log is
shorter than the vault, or when a second copy of the trading server appears.

The host controls the clock, the disk, the network, and when the virtual machine runs. It cannot
read or forge the enclave's keys or the signed log. It can delay, drop, replay and stop things. A
disk that was rolled back, a second copy of the trading server, and a forwarded port that answers
the certificate authority are all signs of a misbehaving host.

1. Do not trust what the host says about the trading server. Trust the attestation, the signed
   answers, which `engine verify-response` checks, and the replay.
2. For `clock_drift` and `trusted_time_unavailable`, fix the host's clock, and its UDP path to the
   Roughtime servers. Both alerts halt sequencing by themselves, and sequencing resumes once the
   time checks out again. [Trusted time](feed-and-custody-failures.md#trusted-time) has the details.

### Checkpoints cannot resume

Use this procedure when the trading server is down and cannot be restored, when the key is lost
beyond the timelock's reach, or when the operator cannot be replaced in time. Use it also when
`vault_exit_started` fires.

The vault's own safety net is the exit against the last posted root. The owner must decide to let it
run, because the exit is permanent. It ends signed withdrawals, checkpoints and distribution claims
for good.

1. Work out the deadline. Anyone may start the exit at `lastCheckpointAt()` plus `exitPeriod()`,
   which is 14 days at launch. A governance recovery, such as new keys or a new operator, needs the
   timelock's three-day delay after the replacement is ready, as [Key recovery](key-recovery.md) and
   [Running as a standby](standby.md) describe. The owner decides whether a recovery can finish
   first.

2. Meanwhile, keep the public log available. The mirrors must serve every completed segment, because
   clients need the log to build their proofs.

3. Once the exit is open, anyone can start it, and anyone can register claims during the claim
   window, `claimWindow()`. A running trading server raises `vault_exit_started`, and on chain the
   normal calls revert with `ExitActive`. After the window, accounts are paid pro rata from what the
   vault holds, and the house's remainder goes out through the timelock. The vault values each
   account's equity at the root's marks.

4. Build the vault calls from the log alone, for an account, for the house, or for a vault deposit
   id:

   ```shell
   SEQ=$(cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC)
   $ENGINE exit-proof --log archive --seq $SEQ --account <address> > exit-$SEQ.json
   $ENGINE exit-proof --log archive --seq $SEQ --house > exit-house-$SEQ.json
   $ENGINE exit-proof --log archive --seq $SEQ --deposit <id> > exit-deposit-$SEQ.json
   jq -r '.calls[] | .what, .calldata' exit-$SEQ.json
   ```

   `<address>` is the account's address, and `<id>` is a vault deposit id. The last command lists
   each call's description and its encoded data. `exit-proof` fails unless the log's instructions
   reproduce the checkpoint's root, and the vault checks every proof against the root it accepted.

   An account's principal that is not yet final joins its claim through the `registerExitCredits`
   calls that follow its `registerExit`. It does so only where the vault's own deposit records match
   the credits that the root holds for the account. A deposit made after a reorganisation of the
   chain, at an id where the root holds another receipt, is refunded to its payer through the
   `refundReceipt` call that `--deposit <id>` builds. Send that call while claims are being
   registered.

5. Send each call in the order listed. Skip a call whose description says that the vault refuses it.

   **Changes the live system.** The owner must approve each call that you send.

   ```shell
   cast send $VAULT <calldata> --rpc-url $RPC --account <sender>
   ```

   `<calldata>` is one call's data from step 4, and `<sender>` is the Foundry keystore account that
   sends it. Someone other than the account may send the call, and the payment still goes to the
   account.

6. Tell the people affected, plainly, what the owner decided, and when.

### Pages stop arriving

Use this procedure when one of these Prometheus rules fires:

- `EngineNodeDown`: no metrics scrape has succeeded for a minute.
- `EngineAlertPagesFailing`: pages failed or were dropped in the last 15 minutes.
- `EngineAlertDeliveryFailing`: three posts failed in 5 minutes.
- `EngineAlertLinesDropped` or `EngineAlertOutboxOverflow`: the trading server dropped or refused
  pages.
- `EngineAlertRouteOff`: no delivered bundle holds a webhook's URL. Deliver it, as
  [Secrets](key-recovery.md#secrets) describes.

The trading server posts its pages through a durable outbox. It retries each page with a backoff for
up to an hour per run, bounded by `alerts.outbox.max_age`. A page is removed once the receiver
accepts it, once the receiver refuses it permanently, or once its retry budget runs out. A permanent
refusal is a redirect, or a 4xx status other than 408 and 429. The Prometheus receiver is separate
on purpose, so that a dead webhook is not silent.

1. Check the receiver first. The webhook's URL is a credential, so a rotated URL fails every post.
2. Check the trading server's outbound network.
3. Read what is queued:

   ```shell
   curl -s http://<metrics host>:9464/metrics | grep '^engine_alert_pages_pending'
   ```

**How to tell it is resolved.** Pages queued while the receiver was down are delivered once it
returns, oldest first, and `engine_alert_pages_pending` falls back to 0.

## After the incident

1. Write down what happened, in order, with the times, the commands you ran, the approvals for them,
   and the evidence you kept.
2. State the cause as far as it is known, and state what is not known.
3. For every alert that had no runbook for its situation, and every step in these runbooks that was
   wrong or missing, change the runbook within the same week.
