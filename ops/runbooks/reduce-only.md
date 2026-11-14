# Make markets reduce-only

Reduce-only stops new risk in a market: an order that would open or grow a position is refused with
the reason `reduce_only`, while an order that shrinks or closes a position still fills. It is the
fastest way to cap the house's exposure without stopping trading. It is an admin command signed by
the operator key, recorded in the public log like any other.

Read [reference.md](reference.md) first, for `$ENGINE`, `$API`, `$PIN` and the config.

## What it does and does not do

- It refuses orders that grow a position, in the market (or in every market), whoever sends them.
- It does not close anything. Open positions stay open, keep accruing financing, and are closed out
  by the usual margin rule.
- It does not touch deposits, withdrawals, checkpoints or corporate actions.
- There are two independent switches. A market is reduce-only if **its own** flag is set **or** the
  **global** flag is. `set-reduce-only false` without `--market` clears only the global flag. A
  market you set earlier stays reduce-only until you clear it by its id.

## When

| Situation | Do |
| --- | --- |
| A market has no usable price, or its feeds disagree ([feed-and-custody-failures.md](feed-and-custody-failures.md)) | That market. It already cannot fill without a price; this stops the first fill after it recovers while you check it. |
| A corporate action failed (`corporate_action_failed`) | That market. A failed split already stops dealing there; a failed dividend does not. |
| House NAV is below zero (`house_deficit`) or exposure is near a cap | The markets that carry it, or all. |
| The house keeps losing to the same accounts through the raised spreads of `house_loss_stress` | The markets they trade, while you check them. |
| A venue or vendor is under suspected manipulation | The markets it prices. |
| An upgrade, handover or recovery where you want no new risk during the change | All markets, then lift it afterwards. |
| An incident you cannot yet explain ([incident-response.md](incident-response.md)) | All markets. It is reversible and loses nothing. |

Raising the stress floor (`set-stress-floor`) widens spreads instead of refusing orders. It is a
different tool and is not covered here.

## Stop new risk

1. Choose the scope: one market id (`curl -sf $API/v1/markets | jq -r '.[] | "\(.id) \(.symbol)"'`),
   or every market.
2. Send the command.
   **Changes the live system.** Needs the owner's approval of this admin command and its scope. It
   changes the rules for every account in the market. Have the exact command ready when you ask.

   ```sh
   # one market
   $ENGINE admin --config $CONFIG --node $API --pin $PIN set-reduce-only true --market 2
   # every market
   $ENGINE admin --config $CONFIG --node $API --pin $PIN set-reduce-only true
   ```

   The key comes from `--key-file`, `OPERATOR_KEY_FILE` or `OPERATOR_KEY`
   ([reference.md](reference.md)).
3. Check the command applied. The output ends with `[{"type":"admin_applied"}]`. If it ends with
   `admin_rejected`, the command prints `Error: the engine rejected the command: <reason>` and the
   flag is unchanged.
4. Check the flag is on:

   ```sh
   curl -sf $API/v1/markets | jq -c '.[] | select(.reduce_only) | {id, symbol}'
   ```

   Every market you meant is listed (with the global flag, all of them).
5. Check it bites. After a minute with traffic, the counter for refused orders rises:

   ```sh
   curl -s http://<metrics host>:9464/metrics | grep 'engine_rejections_total{event="order_rejected",reason="reduce_only"}'
   ```

## Resume normal trading

1. Confirm the cause is resolved (`engine_market_priced` reads 1 for the market, the alert has
   resolved, the exposure is under its cap). The market's `mark` is the last stored value and
   survives the loss of live pricing, so it does not show the market is priced.
2. Send the same command with `false` and the same scope. **Changes the live system.** Needs the
   owner's approval of this admin command.

   ```sh
   $ENGINE admin --config $CONFIG --node $API --pin $PIN set-reduce-only false --market 2
   $ENGINE admin --config $CONFIG --node $API --pin $PIN set-reduce-only false
   ```

3. Check that `curl -sf $API/v1/markets | jq -c '.[] | select(.reduce_only) | .id'` lists only the
   markets you meant to keep restricted. Clear a market's own flag even after clearing the global
   one.

## If the command fails

| Output | Meaning | Do |
| --- | --- | --- |
| `Error: no operator key: pass --key-file, or set OPERATOR_KEY_FILE or OPERATOR_KEY` | No key given. | Give the operator's key. |
| `the key is not the operator key 0x… in <config>` | The key is not the one the config names. After a rotation or an election, sign with `--rotated` and the new key. | See [key-recovery.md](key-recovery.md), "Operator key". |
| `the node's TLS key is not the pinned one` | `$PIN` is stale, or the thing answering is not the enclave. | Re-take the pin from a verified attestation. Do not use `--insecure`. |
| `the node URL must be https://host:port` | `--node` has a path or the wrong scheme. | Use `$API` with no trailing path. |
| `401 Unauthorized {"error":"invalid signature"}` | The key is not the one the log records, or the config's chain and vault are not the log's. | Use the operator's key and the config the node runs. After a rotation, add `--rotated`. |
| `the engine rejected the command: unknown market` | The id is not listed. | Use an id from `/v1/markets`. |
| The call times out after 10 seconds | The node is down, or the sequencer is stalled. | [restart-and-recovery.md](restart-and-recovery.md). If `sequencer_stalled` or `clock_drift` is firing, the command cannot be sequenced either. |

If you cannot reach the node at all, reduce-only cannot be set. The only lever left on the contracts
is the guardian's withdrawal pause ([incident-response.md](incident-response.md)), which does not
stop trading.
