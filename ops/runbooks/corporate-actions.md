# Corporate actions

A dividend or a split on a share, or an index dividend point, is scheduled by a signed admin
command before the market's previous close. The engine captures each account's holdings at that
close and applies the adjustment at the ex-date open, before any opening fill. How the engine
treats the money is in [the node README](../../crates/node/README.md#corporate-actions-operator-runbook);
this runbook is the sequence to follow. Read [reference.md](reference.md) first.

An accepted instruction cannot be cancelled or replaced. A mistake in one is repaired by the owner
deciding the remedy, not by sending another instruction: take your time over the inputs.

## 1. Gather the inputs

For each action, from the venue's or issuer's announcement:

| Input | Meaning |
| --- | --- |
| `id` | A name no earlier action used, such as `AAPL-2026-11-04-dividend`. It cannot be reused. |
| `market` | The market id (`curl -sf $API/v1/markets \| jq -r '.[] \| "\(.id) \(.symbol)"'`). Shares and indices only; ETFs, bonds, currencies, commodities and crypto take none, and an index does not split. |
| `previous_close` | The venue's close immediately before the ex-date open, in UTC epoch milliseconds. |
| `ex_open` | The ex-date open, in UTC epoch milliseconds. |
| dividend: `gross_per_unit` | In the market's quote currency, per share or index unit held at `previous_close`. A dividend going ex after a split is per post-split share. |
| dividend: `withholding_rate` | A decimal from `"0"` to `"1"`. The engine has no default: the owner supplies the rate for every instruction. |
| split: `new_units`, `old_units` | Positive, unequal integers: 4 and 1 for a four-for-one. |

## 2. Fix the two times

Both times must be an actual close and open of the market's listed calendar, including holidays,
early closes and daylight saving. Read the calendar the node holds, not the venue's website:

```sh
curl -sf $API/v1/markets | jq '.[] | select(.id == 40) | .schedule'
```

`tz` is the venue's zone, `windows` the weekly sessions, `closed_dates` whole days shut, `closures`
dated periods shut inside the windows, `special_sessions` dated extra sessions. Find the session
that opens on the ex-date and the session that closes before it, and convert local time to epoch
milliseconds with the zone's rules:

```sh
python3 -c 'from datetime import datetime; from zoneinfo import ZoneInfo; z = ZoneInfo("America/New_York"); print(int(datetime(2026, 11, 3, 16, 0, tzinfo=z).timestamp() * 1000))'
```

Check: for the example, the close of Tuesday 3 November 2026, 16:00 New York, prints `1793739600000`,
and the open of Wednesday 4 November, 09:30, prints `1793802600000`. If the calendar does not list a
close or an open you expect (a half-day, a holiday), stop: the calendar is changed by an
`upsert-market` admin command, which is a protocol change for the owner, and a pending action locks
the calendar once scheduled. While votes govern ([operator-candidate.md](operator-candidate.md) §7),
only a normal vote changes the calendar, applying seven days after the engine logs it: until then,
schedule an action only at an open the calendar already lists.

## 3. Write the instruction

One file per instruction (a dividend and a split at one opening are two files):

```toml
id = "AAPL-2026-11-04-dividend"
market = 40
previous_close = 1793739600000
ex_open = 1793802600000

[adjustment]
kind = "dividend"
gross_per_unit = "0.26"
withholding_rate = "0.15"
```

```toml
id = "AAPL-2026-11-04-split"
market = 40
previous_close = 1793739600000
ex_open = 1793802600000

[adjustment]
kind = "split"
new_units = 4
old_units = 1
```

The values above are examples: use the real figures.

## 4. Submit

**Submit strictly before `previous_close`.** An instruction submitted at the close or later is
rejected, because the engine cannot infer historical holdings. Aim to submit at least one full
session earlier, so a rejection can be fixed.

1. Send it. **Changes the live system.** Needs the owner's approval of this admin command with the
   inputs above, including the withholding rate. It changes balances for every holder of the
   market.

   ```sh
   $ENGINE admin --config $CONFIG --node $API --pin $PIN schedule-corporate-action div.toml
   ```

   The command checks the file locally first (an unknown field or a number it cannot read stops it
   before it sends anything), then prints the signed request and the events the node produced.
2. Check the answer ends `[{"type":"corporate_action_scheduled", …},{"type":"admin_applied"}]`.

   | Output | Meaning | Do |
   | --- | --- | --- |
   | `the engine rejected the command: corporate boundaries must be an actual venue close and open` | `previous_close` or `ex_open` is not on the listed calendar, or the close is not the one immediately before the open. | Redo section 2. |
   | `the engine rejected the command: duplicate corporate action id` | The id was used before. | Choose another id. |
   | `the engine rejected the command: duplicate or inconsistent corporate action at this market boundary` | An action of the same kind is already scheduled for that market's opening, or the boundaries disagree with one that is. | Check the log for what exists (step 3). |
   | Any other `admin_rejected` reason | The text names the cause (for example a market that is not a share or index). | Fix the input. Nothing was recorded but the rejection. |

3. Check it is on the public log, and that the listing is now locked (a pending action locks the
   market's calendar, symbol, asset class and quote currency; margin and spread changes stay open):

   ```sh
   curl -f --retry 5 -o archive/manifest.jsonl $API/v1/archive/manifest.jsonl     # then the segments, see reference.md
   jq -c 'select(.entry=="command") | .events[]? | select(.type | startswith("corporate")) | {type, id: .action.id}' archive/0*.jsonl
   ```

   The archive holds completed segments only, so a new instruction can take minutes to appear in it.

## 5. Watch it happen

| When | Event in the log | Check |
| --- | --- | --- |
| At `previous_close` | `corporate_entitlements_captured` | One per action; the holdings are recorded. |
| At `ex_open`, before any opening fill | `corporate_action_applied` | A dividend lists each account's USD movement and the FX mark used; a split lists the ratio, then each account's cash in lieu and each cancelled order. |
| Every accepted action | `corporate_action_scheduled` | The one you sent. |

Replay verifies these events and the roots they produce: `engine replay --log archive` after the
open ends in `verified:`. A boundary that falls while the node is down is processed before the first
command the node sequences after it returns. The split's effect on resting orders and prices is
described in the node README.

The local rehearsal, with a mock operator key and time, is `./scripts/dev-corporate-actions.sh`.

## 6. When an action fails

A failed action emits the event `corporate_action_failed` and the Critical alert of the same name
(`action`, `market` in its fields). No partial change is committed.

- **A split failed** (a price or entry rounds to zero, a value overflows, a position would exceed the
  input bound, or the rounded-off fraction has no mark or usable FX valuation): the market stops
  dealing, including closes, refuses quotes and further actions, keeps its listing locked, and stops
  accruing financing. **Nothing in the protocol unblocks it.** Make the market reduce-only
  ([reduce-only.md](reduce-only.md)) if it is not already refused, and call the owner: the remedy is
  a protocol change.
- **A dividend failed** (no usable FX valuation mark, or an unrepresentable transfer): it paid nothing
  and the market keeps dealing. If the cause was a missing FX mark, check the FX market's feeds
  ([feed-and-custody-failures.md](feed-and-custody-failures.md)); the owner decides whether the
  holders are made whole outside the protocol.
