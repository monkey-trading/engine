# Corporate actions

Use this runbook to schedule a dividend or a split on a share, or a dividend in index points on an
index, and to act when `corporate_action_failed` fires. A corporate action is scheduled by a signed
administrative instruction before the market's previous close. The engine records each account's
holdings at that close, and applies the adjustment at the ex-date open, before any execution at the
opening. [What the engine does](#what-the-engine-does) explains how the engine treats the money and
the books.

The steps use the shell variables that [Variables](reference.md#variables) sets up. Commands and
their output call a running trading server a node.

**An accepted instruction cannot be cancelled or replaced. A mistake in one is repaired by the owner
deciding the remedy, not by sending another instruction, so take your time over the inputs.**

## The bounds on an instruction

The engine holds each instruction to the parameters' `corporate_bounds`. To read the bounds in
force:

```shell
curl -sf $API/v1/house/state | jq .params.corporate_bounds
```

The command prints `null` while the launch bounds apply. At launch, `max_dividend_bps` is 1000,
`max_split_ratio` is 20 and `min_trading_days` is 2. The bounds are:

- **Trading days.** At least `min_trading_days` of the market's trading sessions must close after
  the instruction is logged, on as many distinct dates, with the previous close as the last of them.
  The previous close must also fall at least one day after the instruction is logged for each of
  those dates after the first. With the launch bounds, submit a dividend that goes ex on a
  Wednesday by Monday's close, and at least 24 hours before Tuesday's close.
- **Dividends on short notice.** A dividend logged less than seven days before its
  `previous_close` is capped at `max_dividend_bps` of the market's mark at the time it is logged,
  which is 10% at launch. The cap counts together every such dividend on the market whose ex-date
  open is within seven days of its own, in the shares held at its capture. For a dividend after a
  split that is scheduled to go ex first, enter the amount for each post-split share, against the
  mark divided by the split. A larger dividend must be logged at least seven days before its
  `previous_close`.
- **Splits.** A split's ratio is at most `max_split_ratio` either way, which is 20 to 1 or 1 to 20
  at launch. A split must be logged at least seven days before its `previous_close`. A split may not
  go ex at or before the previous close of a dividend already pending on the market, so schedule the
  split first.
- **Votes.** While votes govern, as [Parameter votes](operator-candidate.md#parameter-votes)
  describes, the engine refuses the operator's instruction. A member proposes it on the vote
  contract instead, and it applies seven days after it passes. The trading days above must still be
  met at that time, so propose it well ahead.

## Schedule a corporate action

Use this procedure when a venue or an issuer announces a dividend or a split on a listed share, or a
dividend in points on a listed index. **Submit the instruction strictly before `previous_close`.**
An instruction submitted at the close or later is rejected, because the engine cannot infer
historical holdings. It must also leave the trading days that
[The bounds on an instruction](#the-bounds-on-an-instruction) requires. Aim to submit at least one
full session earlier than that, so that a rejection can be fixed in time.

1. Gather the inputs for each action from the venue's or the issuer's announcement:

   | Input | Meaning |
   | --- | --- |
   | `id` | A name that no earlier action used, such as `AAPL-2026-11-04-dividend`. It cannot be reused. |
   | `market` | The market's id, which `curl -sf $API/v1/markets \| jq -r '.[] \| "\(.id) \(.symbol)"'` lists. Only shares and indices take corporate actions. ETFs, bonds, currencies, commodities and crypto-assets take none, and an index does not split. |
   | `previous_close` | The venue's close immediately before the ex-date open, in UTC milliseconds since the Unix epoch. |
   | `ex_open` | The ex-date open, in UTC milliseconds since the Unix epoch. |
   | For a dividend, `gross_per_unit` | The amount in the market's quote currency, for each share or index unit held at `previous_close`. A dividend that goes ex after a split is given for each post-split share. |
   | For a dividend, `withholding_rate` | A decimal from `"0"` to `"1"`. The engine has no default, so the owner supplies the rate for every instruction. |
   | For a split, `new_units` and `old_units` | Two positive, unequal integers, such as 4 and 1 for a four-for-one split. |

2. Read the market's calendar from the trading server, not from the venue's website. Both times
   must be an actual close and open of the market's listed calendar, including holidays, early
   closes and daylight saving time. For the market whose id is `<id>`:

   ```shell
   curl -sf $API/v1/markets | jq '.[] | select(.id == <id>) | .schedule'
   ```

   In the output, `tz` is the venue's time zone, `windows` are the weekly sessions, `closed_dates`
   are whole days on which the market is shut, `closures` are dated periods shut inside the
   windows, and `special_sessions` are dated extra sessions. Find the session that opens on the
   ex-date, and the session that closes before it.

   **If the calendar does not list a close or an open that you expect, such as a half day or a
   holiday, stop.** The calendar is changed by an `upsert-market` instruction, which is a protocol
   change for the owner, and a pending action locks the calendar once it is scheduled. While votes
   govern, only a normal vote changes the calendar, and the change applies seven days after the
   engine logs it. Until then, schedule an action only at an open that the calendar already lists.

3. Convert each local time to milliseconds with the zone's rules. For example, for the close at
   16:00 New York time on Tuesday 3 November 2026:

   ```shell
   python3 -c 'from datetime import datetime; from zoneinfo import ZoneInfo; z = ZoneInfo("America/New_York"); print(int(datetime(2026, 11, 3, 16, 0, tzinfo=z).timestamp() * 1000))'
   ```

   The command prints `1793739600000`. The open at 09:30 on Wednesday 4 November gives
   `1793802600000`.

4. Write one file for each instruction. A dividend and a split at the same opening are two files.
   These values are examples, so use the real figures:

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

5. Submit the instruction, where `div.toml` is the file you wrote.

   **Changes the live system.** The owner must approve this administrative instruction with the
   inputs above, including the withholding rate, because it changes the balances of every holder of
   the market.

   ```shell
   $ENGINE admin --config $CONFIG --node $API --pin $PIN schedule-corporate-action div.toml
   ```

   The command checks the file locally first. An unknown field, or a number that it cannot read,
   stops it before it sends anything. It then prints the signed request and the events that the
   trading server produced.

6. Check the answer. It ends with
   `[{"type":"corporate_action_scheduled", …},{"type":"admin_applied"}]`. If it does not, find the
   error here:

   | Output | Meaning | What to do |
   | --- | --- | --- |
   | `the engine rejected the command: corporate boundaries must be an actual venue close and open` | `previous_close` or `ex_open` is not on the listed calendar, or the close is not the one immediately before the open. | Repeat steps 2 and 3. |
   | `the engine rejected the command: duplicate corporate action id` | The id was used before. | Choose another id. |
   | `the engine rejected the command: a corporate action needs at least 2 of its market's trading days after it is scheduled, …` | The instruction is too close to the ex-date. | Ask the owner. The action cannot be scheduled for that ex-date. |
   | `the engine rejected the command: dividends on short notice total at most 1000bps of their market's mark within seven days; …` | The dividend is less than seven days before `previous_close`, and together with the market's other short-notice dividends within seven days, it is above the cap. | Check the amount, and any split before it, against the announcement. A real dividend this large must be submitted seven days ahead. |
   | `the engine rejected the command: a split's ratio is at most 20 either way` | The split is wider than the bound. | Ask the owner. |
   | `the engine rejected the command: a split needs a notice of at least seven days before its previous close` | The split was logged less than seven days before `previous_close`. | Ask the owner. The split cannot go ex at that open. |
   | `the engine rejected the command: the market has no mark to cap a short-notice dividend against` | The market has never had a price, so a dividend less than seven days ahead has no cap to meet. | Submit the dividend at least seven days before `previous_close`. |
   | `the engine rejected the command: a split may not apply before a pending dividend's previous close` | A dividend already pending would be paid on the split shares. | Ask the owner. |
   | `the engine rejected the command: while rotation is on, only a passed vote changes this` | Votes govern. | Ask a member to propose the action on the vote contract. |
   | `the engine rejected the command: duplicate or inconsistent corporate action at this market boundary` | An action of the same kind is already scheduled for the market's opening, or the times disagree with one that is. | Check what exists on the log, as step 7 describes. |
   | Any other `admin_rejected` reason | The text names the cause, for example a market that is not a share or an index. | Fix the input. Nothing was recorded except the rejection. |

7. Confirm that the instruction is on the public log. Download the archive, as
   [Download the public log and verify it](reference.md#download-the-public-log-and-verify-it)
   describes, and list the corporate events in it:

   ```shell
   jq -c 'select(.entry=="command") | .events[]? | select(.type | startswith("corporate")) | {type, id: (.action.id // .id)}' archive/0*.jsonl
   ```

   The archive holds completed segments only, so a new instruction can take minutes to appear in
   it. Once the action is pending, the market's calendar, symbol, asset class and quote currency
   are locked. Margin and spread changes remain possible.

To rehearse this procedure on a local trading server, with a mock operator key and simulated time,
run `./scripts/dev-corporate-actions.sh`.

**How to tell it is resolved.** The instruction's `corporate_action_scheduled` event is on the log.
Then follow [Watch it happen](#watch-it-happen).

**When to escalate.** Ask the owner whenever the table in step 6 says so, and before you correct a
mistake in an accepted instruction.

## Watch it happen

Use this table to follow an action from the time it is scheduled until the ex-date open.

| When | Events in the log | What to check |
| --- | --- | --- |
| When the instruction is accepted | `corporate_action_scheduled` | It is the instruction you sent. |
| At `previous_close` | `corporate_entitlements_captured` | One event for each action. The holdings are recorded. |
| At `ex_open`, before any execution at the opening | For a dividend, `corporate_dividend` for each holder, with the USD amount and the FX mark used. For a split, `corporate_split` with the ratio, then `corporate_cash_in_lieu` for each holder, and `order_cancelled` and `position_levels_removed` where they apply. Then `corporate_action_applied`. | One `corporate_action_applied` event for each action. |

Replay verifies these events and the roots they produce. After the open,
`$ENGINE replay --log archive` prints a line that begins `verified:`. A boundary that falls while
the trading server is down is processed before the first instruction that the trading server
sequences after it returns. [What the engine does](#what-the-engine-does) describes the split's
effect on positions, working orders and quotes.

## When an action fails

Use this procedure when `corporate_action_failed` fires. The event of the same name is in the log,
and the alert carries `action` and `market` in its fields. The engine commits no partial change, and
other markets keep their normal rules.

- **A failed split** stops the market. The causes are a price or an entry price that rounds to zero,
  a value that overflows, a position that would exceed the input bound, or a rounded-off fraction
  with no mark or no usable FX valuation. The market then stops trading, including closing
  positions, refuses quotes and further actions, keeps its listing locked, and stops accruing
  financing. **Nothing in the protocol unblocks the market.**
- **A failed dividend** pays nothing, and the market keeps trading. The causes are no usable FX
  valuation mark, or a transfer that cannot be represented.

1. For a failed split, make the market reduce-only if its orders are not already refused, as
   [Make markets reduce-only](reduce-only.md) describes.
2. For a failed dividend caused by a missing FX mark, check the FX market's feeds, as
   [Feed and custody failures](feed-and-custody-failures.md) describes.
3. Call the owner.

**When to escalate.** Always. For a failed split, the remedy is a protocol change. For a failed
dividend, the owner decides whether the holders are made whole outside the protocol.

## What the engine does

`engine admin` signs the whole instruction, with its nonce and expiry, and sends it over the pinned
TLS connection. The public log records the signature, `admin_applied` or `admin_rejected`, and the
events at the close and at the open, which replay verifies.

**At `previous_close`,** the engine records each holder's quantity before any instruction stamped at
that time, and keeps the record across restarts.

**At `ex_open`,** before any execution at the opening and before any margin check:

- Corporate boundaries and missed financing rollovers run in time order. At a single instant, the
  captures come first and then the openings. Dividends pay on the holdings captured before any
  split, and then splits apply, each in order of market and id. An opening quote that arrives with
  the instruction is stored in the new units before anything executes or is checked for margin, and
  a rollover at that instant follows.
- A dividend credits a long position the gross amount less `withholding_rate`, and debits a short
  position the gross amount. Every step rounds against the account, a credit down and a debit up,
  and the house takes the exact opposite of each transfer. An amount not in USD converts to USD at
  the market's recorded FX valuation mark, which is the mark that financing uses, even if it is
  stale. With no usable mark, the dividend fails, as [When an action fails](#when-an-action-fails)
  describes. A dividend does not count towards an account's MONKEY emission weight.
- A split checks every scaled value and cash transfer before it changes anything. It then scales
  positions and their entry prices, working orders, stored quotes, marks and indices, aggregates,
  stress price references and pending markouts. Each scaled price rounds once, at nine decimals:
  - an entry price rounds against its holder, up for a long position and down for a short one;
  - a working limit or stop price rounds so that it never executes sooner than the client asked,
    down for a buy limit and a sell stop, and up for a sell limit and a buy stop;
  - marks, quotes, indices, stress references and markout prices round towards zero.

  Quantities round towards zero. A position's take profit scales as the limit order that would
  close the position, and its stop loss as the stop order, so that neither fires sooner. A level
  that rounds to zero, or that leaves the input bound, is removed, with the event
  `position_levels_removed` and the reason `split`.
- The fraction that rounding removes from a position is closed at the post-split mark. Its profit
  or loss against the rounded entry price, rounded down and converted at the FX valuation mark,
  moves between the account and the house as `corporate_cash_in_lieu`. No profit fee is charged on
  it, and it counts towards the account's MONKEY emission weight, as the realised profit or loss of
  an execution does.
- A working order whose quantity or price rounds to zero, or whose quantity would exceed the input
  bound of 1,000,000,000, is cancelled with the reason `split`.
- An account that the split leaves with no position writes off any negative balance against the
  house, as a close-out does.
- A split leaves unchanged the amounts of pending dividends, USD risk limits, the notional values of
  past executions, and the age of each stored quote.

**Quotes around a split.** While a split is pending, the market rejects every quote from its
previous close onwards, and any quote stamped at or after its opening, because a venue may publish
the new units during the closure. Once the split applies, the market rejects a quote stamped before
the opening, and takes the first quote at the opening in the new units. After a failed split, the
market rejects every quote, as [When an action fails](#when-an-action-fails) describes.
