# Make markets reduce-only

Use this runbook to stop new risk in one market or in every market, and to lift that restriction
once its cause has gone. While a market is reduce-only, the engine refuses any order that would open
or increase a position in it, with the reason `reduce_only`. Orders that reduce or close a position
are still executed. Reduce-only is the quickest way to cap the house's exposure without stopping
trading.

Reduce-only is set by an administrative instruction that the operator's key signs, and the public
log records it like any other instruction. The steps use the shell variables that
[Variables](reference.md#variables) sets up. Commands and their output call a running trading server
a node.

## When to make a market reduce-only

| Situation | Scope |
| --- | --- |
| A market has no usable price, or its venues disagree, as [Price feeds](feed-and-custody-failures.md#price-feeds) describes. | That market. It cannot be traded without a price, and reduce-only stops the first execution after the price returns while you check it. |
| A corporate action failed, and `corporate_action_failed` fired. | That market. A failed split already stops trading in the market, but a failed dividend does not. |
| The house's net asset value is below zero and `house_deficit` fired, or the house's exposure is close to a cap. | The markets that carry the exposure, or every market. |
| The same accounts keep winning against the house despite the wider spreads that `house_loss_stress` sets. | The markets those accounts trade, while you investigate them. |
| A venue or a data vendor is suspected of manipulation. | The markets it prices. |
| You want no new risk during an upgrade, a handover or a recovery. | Every market, lifted once the change is complete. |
| An incident you cannot yet explain, as [Incident response](incident-response.md) describes. | Every market. Reduce-only can be lifted at any time, and setting it loses nothing. |

To widen a market's spreads instead of refusing orders, raise its stress floor with
`set-stress-floor`. This runbook does not cover the stress floor.

## Stop new risk

Use this procedure when one of the situations above applies.

Reduce-only refuses every order that would open or increase a position in the market, whoever sends
it. It has limits that you need to know before you set it:

- It closes nothing. Open positions stay open, continue to accrue financing, and are closed out
  under the usual margin rule.
- It does not affect deposits, withdrawals, checkpoints or corporate actions.
- It does not tell clients. Post a status notice when you set it, as
  [Tell users](incident-response.md#tell-users) describes, and remove the notice when you lift it.
- There are two independent switches, one for each market and one for every market. A market is
  reduce-only while either its own switch or the global switch is on.

1. Choose the scope. To list each market's id and symbol, run:

   ```shell
   curl -sf $API/v1/markets | jq -r '.[] | "\(.id) \(.symbol)"'
   ```

   Each line shows a market's id, followed by its symbol.

2. Send the instruction, for one market or for every market.

   **Changes the live system.** The owner must approve this administrative instruction and its
   scope, because it changes the rules for every account in the market. Have the exact command ready
   when you ask.

   For one market, where `<id>` is the market's id from step 1:

   ```shell
   $ENGINE admin --config $CONFIG --node $API --pin $PIN set-reduce-only true --market <id>
   ```

   For every market:

   ```shell
   $ENGINE admin --config $CONFIG --node $API --pin $PIN set-reduce-only true
   ```

   The command reads the operator's key from the file that `--key-file` or `OPERATOR_KEY_FILE`
   names, or from `OPERATOR_KEY`. It prints the signed instruction, and then the events that the
   instruction produced. The instruction has been applied when the last line is
   `[{"type":"admin_applied"}]`. If the engine rejected it, the command ends with
   `Error: the engine rejected the command: <reason>` instead, and the switch is unchanged.
   [If the command fails](#if-the-command-fails) lists the other errors.

3. Confirm that the switch is on:

   ```shell
   curl -sf $API/v1/markets | jq -c '.[] | select(.reduce_only) | {id, symbol}'
   ```

   The output lists every market that you meant to restrict. After the instruction for every market,
   it lists all of them.

4. After a minute of client traffic, confirm that orders are being refused:

   ```shell
   curl -s http://<metrics host>:9464/metrics | grep 'engine_rejections_total{event="order_rejected",reason="reduce_only"}'
   ```

   `<metrics host>` is the address of the trading server's metrics listener. The counter rises while
   clients send orders that would increase a position.

**How to tell it is resolved.** Reduce-only is in force once step 3 lists the markets you chose.
Keep it in force until its cause has gone, and then follow
[Resume normal trading](#resume-normal-trading).

**When to escalate.** If you cannot reach the trading server, reduce-only cannot be set. The only
lever left on the contracts is the guardian's withdrawal pause, which does not stop trading, as
[The ladder of containment](incident-response.md#the-ladder-of-containment) describes. Tell the
owner.

## Resume normal trading

Use this procedure once the cause of the restriction has gone.

1. Confirm that the cause is resolved. Check each of these that applies:

   - the alert that led you to restrict the market has resolved;
   - `engine_market_priced` reads 1 for the market;
   - the house's exposure is back under its cap.

   A market's `mark` in `/v1/markets` is the last stored price, and it remains after live pricing is
   lost, so it does not show that the market has a price.

2. Send the same instruction with `false`, for the same scope.

   **Changes the live system.** The owner must approve this administrative instruction.

   For one market:

   ```shell
   $ENGINE admin --config $CONFIG --node $API --pin $PIN set-reduce-only false --market <id>
   ```

   For every market:

   ```shell
   $ENGINE admin --config $CONFIG --node $API --pin $PIN set-reduce-only false
   ```

   **The instruction without `--market` turns off only the global switch. A market whose own switch
   you turned on stays reduce-only until you turn it off by the market's id.**

   The last line of the output is `[{"type":"admin_applied"}]`.

3. Confirm which markets are still restricted:

   ```shell
   curl -sf $API/v1/markets | jq -c '.[] | select(.reduce_only) | .id'
   ```

   The output lists only the markets that you meant to keep restricted. If a market you cleared is
   still listed, turn off its own switch as in step 2.

**How to tell it is resolved.** Step 3 lists no market, or only those you meant to keep restricted.
Remove the status notice that you posted.

## If the command fails

| Output | Meaning | What to do |
| --- | --- | --- |
| `Error: no operator key: pass --key-file, or set OPERATOR_KEY_FILE or OPERATOR_KEY` | No key was given. | Give the operator's key. |
| `the key is not the operator key 0x… in <config>` | The key is not the one that the configuration names. After a rotation or an election, the operator signs with the new key and `--rotated`. | Follow [Operator key](key-recovery.md#operator-key). |
| `the node's TLS key is not the pinned one` | `$PIN` is out of date, or something other than the enclave answered. | Take the pin again from a verified attestation, as [The trading server's attested key and its pin](reference.md#the-trading-servers-attested-key-and-its-pin) describes. Do not work around the pin. |
| `the node URL must be https://host:port` | `--node` has a path, or a scheme other than `https`. | Use `$API`, with no trailing path. |
| `401 Unauthorized {"error":"invalid signature"}` | The key is not the one that the log records, or the configuration's chain and vault are not the log's. | Use the operator's key and the configuration that the trading server runs. After a rotation, add `--rotated`. |
| `the engine rejected the command: unknown market` | The market id is not listed. | Use an id from `/v1/markets`. |
| `no answer within 10s` | The trading server is down, or its sequencer has stalled. | Follow [Restart and recovery](restart-and-recovery.md). While `sequencer_stalled` or `clock_drift` is firing, no instruction can be sequenced, this one included. |
