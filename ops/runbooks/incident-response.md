# Incident response

For a page you do not recognise, a page you cannot fix with its own runbook, or anything that looks
like an attack. The aim in the first minutes is to find what class of problem it is, keep the
books and the funds safe with the least drastic step that works, and keep the evidence. Read
[reference.md](reference.md) first, and find the alert in [alerts.md](alerts.md).

## Contacts

Fill this in before launch and keep a copy outside the systems it describes. Nothing in the node
decides who is called.

| Role | Name and number | Approves |
| --- | --- | --- |
| Deployment owner (OWNER) | | Node, config and admin-command changes; every step marked as changing the live system |
| Multisig signers (OWNER) | | Timelock proposals; the guardian's pause and signer retirement |
| Operator on call (OWNER) | | Runs the commands in these runbooks |
| Host and network provider (OWNER) | | The data disk, the VM, port 443 and the metrics network |
| DNS and registrar account holder (OWNER) | | The API domain's records |

The node posts Critical alerts to the `[alerts.critical]` webhook and Warnings to the optional
`[alerts.warning]` one. Prometheus rules (`ops/prometheus/engine-node-rules.yml`) page through a
different receiver for the node going silent and for pages that cannot be delivered.

## First five minutes

1. **Is the node answering?**

   ```sh
   curl -sf --max-time 10 $API/v1/health            # ok
   curl -s --max-time 10 http://<metrics host>:9464/metrics | grep -E '^engine_(withdrawals_paused|last_checkpoint_timestamp_seconds|alert_pages_pending) '
   ```

   No answer: [restart-and-recovery.md](restart-and-recovery.md). Answering: go on.
2. **What is open?** The pages received, and the console log if you have it. Find each alert in
   [alerts.md](alerts.md) and follow its runbook for the first step. A Critical page with no runbook
   for its situation is an incident; continue here.
3. **How bad is it?** Ask:
   - Can an account lose money it should not, or can the house? (Wrong prices, a wrong book, an
     admin command nobody sent.)
   - Can an account not get its money out? (Withdrawals paused, checkpoints stopped.)
   - Is a key or secret exposed? ([key-recovery.md](key-recovery.md).)
   - Is the public log or the vault inconsistent with what the node says? (`checkpoint_conflict`,
     `collateral_mismatch`, `log_tail_lost`, a failed replay.)
4. **Call the owner** for anything in the second to fourth class, and for any step below marked as
   changing the live system. Have the exact command ready.
5. **Keep the evidence** (below) before you restart or change anything.

## The ladder of containment

Take the least drastic step that stops the harm. Every step but the first two changes the live
system and needs the owner's approval for that action; the guardian steps need the multisig as well.

| Step | Effect | Reversible | How |
| --- | --- | --- | --- |
| Observe | Nothing changes. | n/a | The checks above. |
| Reduce-only for a market or all | Refuses orders that grow a position. Closes still fill. | Yes, at once | [reduce-only.md](reduce-only.md) |
| Raise the stress floor | Widens spreads. From `V46` at most level 1, and it lapses after 72 hours unless set again. | Yes | `$ENGINE admin … set-stress-floor <level> [--market <id>]` (owner's approval) |
| Guardian: `pauseWithdrawals()` | No withdrawal or distribution claim pays for 7 days. Trading, checkpoints and deposits continue. | The timelock lifts it with `setPauses`; it also ends after 7 days | `cast send $VAULT 'pauseWithdrawals()' --rpc-url $RPC --account <guardian>` |
| Timelock: `setPauses(depositsPaused, withdrawalsPaused)` | Pauses deposits and/or withdrawals until the timelock lifts it. | Yes, through the timelock | Pattern in reference.md; 3-day delay |
| Guardian: `retireSigner()` | The vault accepts no checkpoint and no withdrawal signed in the current epoch, until the timelock installs a signer. | Only by a new signer through the timelock | [key-recovery.md](key-recovery.md), "New keys" |
| Stop the node (VM off) | Everything stops: trading, checkpoints, the API. The vault's deadlines begin to run. | Restart | [restart-and-recovery.md](restart-and-recovery.md) |

A guardian step is **immediate** and the quickest lever on chain; the timelock's is 3 days. The
guardian's pause cannot be extended by the guardian. Use it when funds are leaving wrongly or may be,
not for a feed outage.

## Keep the evidence

Do these first when the books, a key or a certificate may be involved, and note the time of each:

1. The pages received, with their timestamps, and the console log (serial output), kept as files.
2. The public log, downloaded and replayed ([reference.md](reference.md), "Download the public log and verify it"). Keep both the
   download and the `replay` output.
3. The node's identity: `curl -sf $API/v1/attestation > attestation-$(date -u +%Y%m%dT%H%M%SZ).json`.
4. The vault and registry state: run every command under "Read the vault and the registry" in
   reference.md and keep the output with its block number
   (`cast block-number --rpc-url $RPC`).
5. The metrics: `curl -s http://<metrics host>:9464/metrics > metrics-$(date -u +%Y%m%dT%H%M%SZ).txt`.
6. Do not delete, truncate or "repair" anything on the data disk, and do not restart a node that
   has stopped itself with `checkpoint_conflict`, `log_recovery_failed` or `log_tail_lost` until the
   owner has seen the evidence.

## By class

### The books look wrong

A failed `replay` of the public log, a root that does not match the vault's, `log_tail_lost`,
`checkpoint_conflict`, `checkpoint_accounting_overflow`, or an account whose balance does not follow
from its events.

- The log is the record. `engine replay --log archive --signer $SIGNER` checks every command and
  checkpoint against the rules and the signer; any other result than `verified:` is an integrity
  incident. Make all markets reduce-only. Keep the evidence. Call the owner.
- `log_tail_lost` (Critical) means a recovery found the log ends before commands that signed heads
  cover: acknowledged commands are lost from this disk. Do not start another node from this disk.
  Look for a copy that reaches further (a standby's, a mirror's).
- Two copies of the node running from one disk make `checkpoint_conflict`: all but one stops. Find
  and stop the extra copy; the host must show which VMs run.

### Funds may be at risk

Wrong withdrawal authorizations, a vault balance below the ledger (`collateral_mismatch`), an
exposed enclave key.

- Guardian `pauseWithdrawals()` first (immediate). Then reduce-only. Then
  [withdrawal-stalls.md](withdrawal-stalls.md) for the mismatch, or [key-recovery.md](key-recovery.md)
  for the key.
- Pausing does not hurt accounts' balances: it delays their exits. Weigh that against the harm.

### Prices look wrong

A market priced far from the venues, a page that the venues disagree, fills that look off.

- A crypto venue that is down or quiet leaves BTC-USD and ETH-USD unpriced, which is its own case:
  [feed-and-custody-failures.md](feed-and-custody-failures.md), "A crypto venue is down or quiet".
- Make the market reduce-only. If the price itself is wrong and fills continue, raise the stress
  floor, and consider the guardian's pause. See [feed-and-custody-failures.md](feed-and-custody-failures.md).
- Do not widen `max_divergence_bps` or change a market's price rules in the heat of an incident:
  they are protocol parameters and a change is an owner decision with a notice period.

### The host misbehaves

The host controls the clock, the disk, the network and when the VM runs. It cannot read or forge the
enclave's keys or the signed log. It can delay, drop, replay and stop things. What you may see:
`clock_drift`, `trusted_time_unavailable`, missing feeds, a disk that rolled back
(`checkpoint_startup_failed`: log shorter than the vault), a second copy of the node, a forwarded
port that answers for the CA (`unattested_certificate`).

- Do not trust what the host says about the node; trust the attestation, the signed answers
  ([reference.md](reference.md): `verify-response`) and the replay.
- `clock_drift` and `trusted_time_unavailable` halt sequencing by themselves and resume when time
  checks out. Fix the host's clock and its UDP path to the Roughtime servers.

### Checkpoints cannot resume

The node is down and cannot be restored, the key is lost beyond the timelock's reach, or the
operator cannot be replaced in time. The vault's own safety net is the **exit against the last
posted root**, which the owner must decide to let run, because it is permanent: it ends signed
withdrawals, checkpoints and distribution claims for good.

1. Do the arithmetic. `lastCheckpointAt()` plus `exitPeriod()` (14 days at launch) is when anyone may
   start it. A governance recovery (new keys, a new operator) needs the timelock's delay (3 days) after
   the replacement is ready ([key-recovery.md](key-recovery.md), [standby.md](standby.md)). The owner
   decides whether a recovery can land first.
2. Meanwhile, make the public log available: mirrors must serve every completed segment, and users
   need the log to build their proofs.
3. When the exit is open (`vault_exit_started` fires on a live node; on chain, `ExitActive` replaces the
   normal calls), anyone starts it and anyone registers claims for the claim window (`claimWindow`).
   After the window, accounts are paid pro rata from what the vault holds; the house remainder goes out
   through the timelock. The vault values each account's equity at the root's marks.
4. Build the calls from the log alone, for an account or the house:

   ```sh
   SEQ=$(cast call $VAULT 'latestSeq()(uint64)' --rpc-url $RPC)
   $ENGINE exit-proof --log archive --seq $SEQ --account 0x<address> > exit-$SEQ.json
   $ENGINE exit-proof --log archive --seq $SEQ --house > exit-house-$SEQ.json
   jq -r '.calls[] | .what, .calldata' exit-$SEQ.json
   ```

   The log's commands must reproduce the checkpoint's root; the vault checks every proof against the
   root it accepted. Send each call in the order listed, skipping a call whose description says the
   vault refuses it: `cast send $VAULT <calldata> --rpc-url $RPC --account <the sender>`. A payer other than the
   account may send it; the pay-out goes to the account.
5. Say plainly to the people affected what the owner decided, and when.

### Pages stop arriving

`EngineNodeDown` (no metrics scrape for a minute), `EngineAlertPagesFailing` (pages failed or
dropped in 15 minutes), `EngineAlertDeliveryFailing` (three failed posts in 5 minutes),
`EngineAlertLinesDropped` and `EngineAlertOutboxOverflow` (the node dropped or refused pages), and
`EngineAlertRouteOff` (no delivered bundle holds the webhook's URL; deliver it,
[key-recovery.md](key-recovery.md), "Secrets").

- The node posts through a durable outbox and retries each page with backoff for up to an hour per
  run, bounded by `alerts.outbox.max_age`. A page is removed after acceptance, a permanent refusal
  (a redirect or a 4xx but 408 and 429), or the budget running out. The Prometheus receiver is
  separate on purpose, so a dead webhook is not silent.
- Check the receiver first: the URL is a credential, so a rotated URL fails every post. Then the
  node's egress. `engine_alert_pages_pending` shows what is queued.
- Pages queued while the receiver was down are delivered when it returns, oldest first.

## After the incident

1. Write down what happened, in order, with the times, the commands run (with their approvals) and the
   evidence kept.
2. State the cause as far as it is known, and what is not.
3. For any alert that had no runbook for its situation, or any step here that was wrong or missing,
   change the runbook in the same week.
