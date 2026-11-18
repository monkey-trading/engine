# Corporate actions

A dividend or a split on a share, or an index dividend point, is scheduled by a signed admin
command before the market's previous close. The engine captures each account's holdings at that
close and applies the adjustment at the ex-date open, before any opening fill. How the engine
treats the money and the books is in [What the engine does](#what-the-engine-does); sections 1 to 6
are the sequence to follow. Read [reference.md](reference.md) first.

An accepted instruction cannot be cancelled or replaced. A mistake in one is repaired by the owner
deciding the remedy, not by sending another instruction: take your time over the inputs.

From protocol `V76` the engine bounds each instruction by the parameters' `corporate_bounds`
(`curl -sf $API/v1/house/state | jq .params.corporate_bounds`; `null` means launch's: 1000, 20, 2):

- at least `min_trading_days` of the market's trading days close after the instruction is logged, the
  previous close the last of them: sessions closing on that many distinct dates, the previous close at
  least a day later for each date past the first. At launch, submit a dividend going ex on Wednesday by
  Monday's close, and at least 24 hours before Tuesday's close;
- a dividend logged less than seven days before `previous_close` is held, with every other such
  dividend on the market whose ex-date open is within seven days of its own, to `max_dividend_bps` of
  the market's mark when it is logged (10% at launch), in the shares held at its capture: after a split
  scheduled to go ex first, enter it per post-split share against the mark divided by the split. A
  larger dividend needs logging at least seven days before `previous_close`;
- a split's ratio is at most `max_split_ratio` either way (20 to 1 or 1 to 20 at launch), it needs
  logging at least seven days before `previous_close`, and it may not go ex at or before the previous
  close of a dividend already pending on the market: schedule the split first;
- while votes govern ([operator-candidate.md](operator-candidate.md) §7), the engine refuses the
  operator's instruction: a member proposes it on the vote contract, and it applies seven days after
  it passes, when the trading days above must still hold, so propose it well ahead.

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
rejected, because the engine cannot infer historical holdings. From `V76` it must also come before
`min_trading_days` of the market's trading days, the previous close the last (see the bounds above).
Aim to submit at least one full session earlier than that, so a rejection can be fixed.

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
   | `the engine rejected the command: a corporate action needs at least 2 of its market's trading days after it is scheduled, …` | From `V76`, too close to the ex-date. | Ask the owner; the action cannot be scheduled for that ex-date. |
   | `the engine rejected the command: dividends on short notice total at most 1000bps of their market's mark within seven days; …` | From `V76`, the dividend, with the market's other short-notice dividends within seven days, is above the cap, and it is less than seven days before `previous_close`. | Check the amount, and any split before it, against the announcement; a real dividend this large needs submitting seven days ahead. |
   | `the engine rejected the command: a split's ratio is at most 20 either way` | From `V76`, a split wider than the bound. | Ask the owner. |
   | `the engine rejected the command: a split needs a notice of at least seven days before its previous close` | From `V76`, a split logged less than seven days before `previous_close`. | Ask the owner; it cannot go ex at that open. |
   | `the engine rejected the command: the market has no mark to cap a short-notice dividend against` | From `V76`, the market has never had a price, so a dividend less than seven days ahead has no cap to meet. | Submit it at least seven days before `previous_close`. |
   | `the engine rejected the command: a split may not apply before a pending dividend's previous close` | From `V76`, a dividend already pending would be paid on the split shares. | Ask the owner. |
   | `the engine rejected the command: while rotation is on, only a passed vote changes this` | From `V76`, votes govern. | Ask a member to propose it on the vote contract. |
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
command the node sequences after it returns. The split's effect on positions, resting orders and
quotes is in [What the engine does](#what-the-engine-does).

The local rehearsal, with a mock operator key and time, is `./scripts/dev-corporate-actions.sh`.

## 6. When an action fails

A failed action emits the event `corporate_action_failed` and the Critical alert of the same name
(`action`, `market` in its fields). No partial change is committed, and other markets keep their
normal rules.

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

## What the engine does

`engine admin` signs the whole instruction, with its nonce and expiry, and sends it over the pinned
TLS channel. The public log records the signature, `admin_applied` or `admin_rejected`, and the
events of the close and the open, which replay verifies.

**At `previous_close`** the engine records each holder's quantity before any command stamped at that
time, and keeps it across restarts.

**At `ex_open`**, before any opening fill or margin check:

- Corporate boundaries and missed financing rollovers run in time order. At one instant the captures
  come first, then the openings: dividends pay the holdings captured before any split, then splits
  apply, each in market and id order. An opening quote that arrives with the command is stored in the
  new units before anything executes or is margin-checked, and a rollover at that instant follows.
- A dividend credits a long the gross amount less `withholding_rate`, and debits a short the gross.
  Every step rounds against the account (a credit down, a debit up), and the house takes the exact
  opposite of each transfer. A non-USD amount converts to USD at the market's recorded FX valuation
  mark, the one financing values at, even a stale one; with no usable mark the dividend fails
  (section 6). A dividend does not count toward MONKEY emission weight.
- A split checks every scaled value and cash transfer before it changes anything, then scales
  positions and their entry prices, resting orders, stored quotes, marks and indices, aggregates,
  stress price references and pending markouts. Each scaled price rounds once, at nine decimals:
  - an entry against its holder: up for a long, down for a short;
  - a resting limit or stop price so it never executes sooner than the client asked: a buy limit and
    a sell stop down, a sell limit and a buy stop up;
  - marks, quotes, indices, stress references and markout prices toward zero.

  Quantities round toward zero. A position's take profit scales as the limit that would close it and
  its stop loss as the stop, so neither fires sooner; a level that rounds to zero or leaves the input
  bound is removed (`position_levels_removed`, reason `split`).
- The fraction rounded off a position closes at the post-split mark. Its P&L against the rounded
  entry, rounded down and converted at the FX valuation mark, moves between the account and the house
  as `corporate_cash_in_lieu`, with no profit fee, and counts toward the account's MONKEY emission
  weight as a fill's realised P&L does.
- A resting order whose quantity or price rounds to zero, or whose quantity would pass the input
  bound of 1,000,000,000, is cancelled with reason `split`.
- An account the split leaves with no position writes off any negative balance against the house, as
  a close-out does.
- A split leaves unchanged the amounts of pending dividends, USD risk limits, historical fill
  notionals, and the age of each stored quote.

**Quotes around a split.** While a split is pending, the market rejects every quote from its
previous close on, and any quote stamped at or after its opening, since a venue may already publish
the new units during the closure. Once it applies, a quote stamped before the opening is rejected, and
the first quote at the opening is taken in the new units. After a failed split the market rejects
every quote (section 6).
