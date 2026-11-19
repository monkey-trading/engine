# Trading rules

This document sets out the rules that apply to every account, with the values the engine launches
with. The values come from [`config/production-genesis.toml`](../config/production-genesis.toml),
and the engine that enforces them is documented in [`crates/core`](../crates/core/README.md).

Amounts are in US dollars. Rates are given in basis points, abbreviated to bps in the tables. One
basis point is one hundredth of one percent.

## Prices

Each market has a reference price, which the engine calculates from the quotes of independent
venues. The reference price is the median of the mid prices that the market's venues have quoted
within the last second. A market can be traded only while at least two of its venues have quoted
within that second and their quotes agree to within the market's divergence limit. When either
condition fails, the market has no price and no order in it can be executed.

| Markets | Venues | Trading hours | Divergence limit |
| --- | --- | --- | --- |
| BTC-USD, ETH-USD, SOL-USD | Coinbase, Kraken | Always open | 25 bps |
| EUR-USD, GBP-USD, USD-JPY, AUD-USD | Massive, TraderMade | Sunday 17:00 to Friday 17:00, New York time | 5 bps |
| US500, US100, US30, XAU-USD, XAG-USD, WTI | Massive, CME Group | CME Globex sessions | 10 bps |
| JP225 | Massive, CME Group | Osaka Exchange day and night sessions | 10 bps |
| 20 US shares, from AAPL to ORCL | Massive, Alpaca | 09:30 to 16:00 New York time, excluding NYSE holidays | 10 bps |

A market can be traded only during its exchange's trading hours, and a quote received outside those
hours never sets its price. Index and commodity markets are priced from their front-month futures
contract. When the contract rolls to the next month, the engine adds the price gap between the two
contracts, so that the roll does not move the market's price.

Accounts are held in US dollars. JP225 is quoted in yen, and its profits and losses are converted at
the engine's own USD-JPY price. JP225 therefore has no price whenever USD-JPY has none.

## Orders

Every order is executed against the house, which is the engine's own account and takes the other
side of every trade. A buy order is executed at the reference price plus a spread, and a sell order
at the reference price less a spread.

In a fast-moving market, the execution price of a market order may be worse than the price you were
quoted. A market order is rejected if its execution price would be worse than the quoted price by
more than your maximum slippage. You may set your own maximum slippage, but it cannot exceed the
market's limit. That limit is 25 bps on crypto, 5 bps on FX, 10 bps on indices and commodities, and
15 bps on shares.

A limit order is executed once its execution price, including the spread, reaches its limit or
better. A stop order is executed at the prevailing price once the reference price reaches its stop.
A working order whose market is closed is executed at the first price after the market opens, even
if that price has gapped. If a triggered order cannot be executed for any reason other than its
price, for example because the account has insufficient margin, the order is cancelled.

Any order can be marked reduce-only, in which case it can only reduce an existing position. An order
that opens or increases a position in a share must be worth at least $10.

### Take profit and stop loss

You can attach a take profit and a stop loss to a position. Either level can be set on the order
that opens or adds to the position, or later by a separate request. An order that names only one
level leaves the other unchanged. A reduce-only order cannot set levels.

When the price at which the whole position would close, including the spread, reaches a take profit
or stop loss level, the whole position is closed. A take profit executes at its level or better. A
stop loss executes at the prevailing price, which may be worse than its level if the market gaps.
Levels are checked on every price update in their market, and never while the market is closed.

A new or moved level must lie at least the market's minimum distance beyond the price at which the
position would close. For a level set on a working order, the distance is measured from the order's
price instead. The minimum distance for each market equals its closing spread cap, listed
under [Spreads](#spreads). The take profit and stop loss on a position count together as one working
order towards your limit. Closing or reversing the whole position removes both levels, but adding to
it or reducing it does not.

## Spreads

The spread is set in bands of position size, measured as the position's value in US dollars. A trade
pays each band's rate on the part of the position that falls inside that band. The spread is charged
both on the part of a trade that opens or increases a position and on the part that reduces or
closes one. A round trip therefore pays the spread twice.

The bounds of the first band below are those of an account at volume tier 2. An account at another
tier has a different first band, as [Volume tiers](#volume-tiers) describes.

| Markets | First band | Second band | Above the second band |
| --- | --- | --- | --- |
| BTC-USD | 2.0 bps to $2M | 4.0 bps to $10M | 8.0 bps |
| ETH-USD | 3.0 bps to $1M | 6.0 bps to $5M | 12.0 bps |
| SOL-USD | 2.0 bps to $1M | 4.0 bps to $5M | 8.0 bps |
| EUR-USD, GBP-USD | 0.4 bps to $10M | 0.8 bps to $50M | 1.6 bps |
| USD-JPY, AUD-USD | 0.5 bps to $10M | 1.0 bps to $50M | 2.0 bps |
| US500, US30 | 0.7 bps to $5M | 1.4 bps to $25M | 2.8 bps |
| US100 | 0.9 bps to $5M | 1.8 bps to $25M | 3.6 bps |
| JP225 | 1.0 bps to $5M | 2.0 bps to $25M | 4.0 bps |
| XAU-USD, XAG-USD, WTI | 2.0 bps to $2M | 4.0 bps to $10M | 8.0 bps |
| AMD, AVGO, LLY, NVDA, ORCL, TSLA | 3.0 bps to $500k | 6.0 bps to $2M | 12.0 bps |
| The other 14 shares | 2.0 bps to $500k | 4.0 bps to $2M | 8.0 bps |

The toxicity add-on and the stress level, described below, can widen the spread. However, the spread
charged on reducing or closing a position never exceeds the market's closing spread cap.

| Markets | Closing spread cap |
| --- | --- |
| BTC-USD | 30 bps |
| ETH-USD, SOL-USD | 35 bps |
| FX | 17 bps |
| US500, US30 | 18 bps |
| US100, JP225 | 19 bps |
| XAU-USD, XAG-USD, WTI | 23 bps |
| AMD, AVGO, LLY, NVDA, ORCL, TSLA | 27 bps |
| The other 14 shares | 23 bps |

### Toxicity add-on

Every account starts at the best price. The engine measures how far the reference price moves in
your favour in the five seconds after each of your trades, in units of the market's typical
five-second move over the last hour. From these measurements the engine calculates a toxicity score,
which is the number of standard errors by which your recent advantage exceeds zero. Each trade is
weighted by its size, up to four times your average trade size. The weights halve every seven days.

Once the score rests on at least 20 trades, a score of 4 adds 2 bps to the spread of every trade,
and a score of 8 adds 5 bps. A trade reversed at the same price within five seconds does not count
towards the score, and neither does a close-out.

### Stress levels

Each market has a stress level, which widens its spreads during turbulent conditions. The level
rises as soon as one of its triggers fires, and it steps down five minutes after the last trigger
clears. The operator can hold a market at level 1 for at most 72 hours.

| Level | Size spread | Added spread | Triggers |
| --- | --- | --- | --- |
| 0 | Unchanged | None | None |
| 1 | Doubled | 2 bps | The venues' quotes diverge by more than 60% of the market's divergence limit. The reference price moves 0.75% within a minute. The house's net exposure in the market reaches 80% of its limit. |
| 2 | Quadrupled | 10 bps | The venues' quotes diverge by 95% of the limit. The reference price moves 2% within a minute. The house's net exposure reaches 95% of its limit. In every market at once, the house has lost money to its clients over the last 24 hours. |

## Profit fee

No fee is charged on a losing trade. On a winning trade, a share of the realised profit is charged
as a fee, after the spreads paid on opening and closing. The share falls the longer the position has
been held:

| Time held | Up to 10 s | 16 s | 25 s | 39 s | 1 min | 1.6 min | 2.5 min | 3.9 min | 6.1 min | 9.6 min | 15 min or more |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Fee | 100% | 91% | 82% | 73% | 64% | 55% | 46% | 37% | 28% | 19% | 10% |

Between the points in the table, the fee falls in a straight line and is rounded up to the nearest
basis point. For a position held 15 minutes or more, the fee is your volume tier's base rate, which
is 10% at tiers 0 and 1, 9% at tier 2 and 8% at tier 3.

When you add to a position, its opening time becomes the size-weighted average of the times at which
each part was added. Any part held for longer than 15 minutes counts as 15 minutes old, so adding to
a position neither restarts its holding time nor lets a large new trade benefit from the age of a
small old one. Reducing a position leaves its opening time unchanged. Reversing a position closes
all of it at its holding time and opens the remainder as a new position.

## Financing

A daily rollover takes place at 22:00 London time, on weekends and holidays as on any other day. At
each rollover, every open position pays or receives one day's financing on its value. A long
position pays the benchmark interest rate plus the market's markup. A short position receives the
benchmark rate less the markup, and pays the difference when the markup is the larger.

| Markets | Benchmark rate | Markup | Day count |
| --- | --- | --- | --- |
| BTC-USD, ETH-USD, SOL-USD | SOFR | 5% | 365 |
| EUR-USD | €STR | 1% | 360 |
| GBP-USD | SONIA | 1% | 365 |
| USD-JPY | SOFR | 1% | 360 |
| AUD-USD | AONIA | 1% | 365 |
| US500, US100, US30, XAU-USD, XAG-USD, WTI and the shares | SOFR | 2.5% | 360 |
| JP225 | TONA | 2.5% | 365 |

Benchmark rates come from each rate's official publisher. A market whose benchmark has no published
rate for the day accrues no financing. Accounts at volume tiers 2 and 3 pay a markup reduced by 10%
and 20% respectively.

### Dividends and share splits

Dividends and splits take effect at the market's open on the ex-date. A long position in a share or
index receives the dividend less withholding tax, and a short position pays the full dividend. A
share split scales the positions, working orders and prices in that share together. Any fraction of
a unit that the split leaves is settled in cash.

## Volume tiers

Your volume tier depends on the trading volume you have built up over the last 30 days, counted in
UTC days. Every account starts at tier 0. The higher tiers require both a total volume and trading
on a minimum number of separate days, because no single day counts for more than the tier's volume
divided by that number of days.

| Tier | Volume over 30 days | Days of trading | First spread band | Maximum leverage | Working orders | Requests per second | Profit fee base | Markup reduction | MONKEY weighting |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | None | None | 25% of the tier 2 band | 20 times | 25 | 2 | 10% | None | Standard |
| 1 | $1M | 2 | 50% of the tier 2 band | 50 times | 50 | 5 | 10% | None | 1.1 times |
| 2 | $25M | 5 | The tier 2 band | The market's own | 100 | 10 | 9% | 10% | 1.25 times |
| 3 | $250M | 10 | 250% of the tier 2 band, up to the second band's bound | The market's own | 250 | 20 | 8% | 20% | 1.5 times |

The volume from opening a position counts once you have held the position for 60 seconds, so a
position opened and closed within a minute adds nothing. The volume from closing a position counts
five seconds after the trade, unless you reopen the position at the same price within those five
seconds. A close-out counts at once.

If your tier falls, no working order is cancelled and no position is closed. Only new orders beyond
the lower tier's limits are refused.

## Margin and close-out

Margin is shared across all the positions in your account. Each market sets its margin as a
percentage of position value, in bands that rise as the position grows.

| Markets | Margin |
| --- | --- |
| BTC-USD, ETH-USD | 1% to $2M, 2% to $10M, 5% above |
| SOL-USD | 10% to $1M, 20% above |
| FX | 0.5% to $5M, 1% to $25M, 3% above |
| Indices | 0.5% to $5M, 1% to $25M, 5% above |
| Commodities | 1% to $2M, 5% above |
| Shares | 10% to $500k, 20% to $2M, 50% above |

Opening or increasing a position, and making a withdrawal, must leave your equity at or above the
initial margin requirement. The initial requirement uses the bands above, with each band raised to
at least 5% at volume tier 0 and 2% at tier 1. These floors limit leverage to 20 times and 50 times
respectively.

The close-out level is half of the margin requirement at the market's own bands. If your equity
falls below the close-out level, the engine closes your largest losing position that can be traded.
The engine continues with the next largest losing position until your equity is back above the
close-out level. A position in a market that is closed or has no price waits until the market can be
traded again. Any loss beyond your balance is borne by the house, not by you.

The house also limits its own exposure. An order is rejected if it would take the house's net
exposure past its limit, which is the larger of a share of the house's net asset value and a fixed
floor. The floors range from $3.16 million for each share to $31.6 million for BTC-USD, with $61
million across all markets.

### Market closures

Margins do not change with the time of day or week. No market charges a higher margin before or
during a weekend or holiday closure. No limit on new positions or withdrawals applies ahead of a
closure, and no position is closed out because its market is about to close.

A price can move while its market is closed. When the market reopens, its first price may leave an
account below its close-out level, or with a loss larger than its balance. The account is then
closed out at the prices available after the reopening, as described above, and the house bears any
loss beyond the account's balance. The house manages this risk through its spreads and its exposure
limits.

The engine can apply a weekend margin, which is a higher margin over a market's closures of 40 hours
or more. No market carries one. A weekend margin can be added to a market only after seven days'
public notice.

## Deposits, withdrawals and the exit

Collateral is USDC, held in the vault contract on Arbitrum One. A deposit must be between 10 and
1,000,000,000 USDC. A deposit can be used as margin once a majority of the server's RPC providers
report that Arbitrum has confirmed it. The deposit can be withdrawn once its block is final on
Ethereum.

Before an account's first deposit is credited or its first order is accepted, the account must sign
an eligibility declaration. The server screens both the account and the address of the client that
sends each declaration, order and withdrawal.

A withdrawal must be for at least 10 USDC. Withdrawals join a public first-in, first-out queue. If
the vault holds too little USDC, the request at the head of the queue is paid in part. Once a
checkpoint that the vault has accepted includes the approval, the enclave signs an authorisation for
it. Anyone can submit that authorisation to the vault within 24 hours. An authorisation that expires
unpaid is signed again when its account asks.

If the enclave stops posting checkpoints, the vault stops accepting deposits after one day. After 14
days without a checkpoint, anyone can start the vault's exit against the last checkpoint. Each
account then has 30 days to register the amount that the checkpoint shows it is owed, including its
equity at the checkpoint's prices and any unpaid withdrawals. The vault then pays every claim in
proportion to the USDC it holds. `engine exit-proof` builds an account's claim from the public log.

## MONKEY and staking

MONKEY is a token minted by the vault. Every ten minutes, a fixed amount of MONKEY is distributed
among the traders who realised a loss in that period, in proportion to the size of their losses. The
amount is 100,000 MONKEY at launch, and it halves every 182.5 days.

Staked MONKEY earns a share of the house's net asset value above a reserve of $30.5 million. The
share is paid in USDC at the end of each day, to the extent that the house's cash covers it.
Unstaking takes 28 days. Both MONKEY and USDC distributions are claimed from the vault with a proof
against a checkpoint.

## Changes to the rules

The operator lists markets, publishes benchmark interest rates, schedules dividends and share
splits, and can suspend new positions in one market or in all of them. No operator action can
prevent you from reducing or closing a position in a market that is open and has a price. Every
operator action is a signed instruction in the public log.

The profit fee, spreads, margins, financing markups, stress settings, volume tiers and weekend
margins can change only after seven days' public notice. Some of them can also change only within
limits set at launch:

- the base profit fee can rise to at most one and a half times its launch rate, and never fall below
  it;
- the fee at each holding time can never fall below its launch value;
- each spread stays between its launch rate and twice that rate;
- each margin stays at or above its launch rate;
- each volume tier's benefits stay at or below their launch values.

When operator rotation is enabled, MONKEY stakers elect the operators, and one of them is drawn each
day to run the engine. The operator in office then receives a fifth of each staking distribution.
While rotation is enabled, the settings above can change only through a vote that carries more than
two thirds of the elected operators' stake.

Each change to the rules takes effect as a new protocol version, which the log records. An old log
therefore replays under the rules that applied when it was written. The
[version list](../crates/core/src/protocol/versions.rs) describes what each version changed.
