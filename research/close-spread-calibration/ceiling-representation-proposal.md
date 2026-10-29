# Close-spread ceiling representation: a proposal for #344

A recommendation for each of #344's five open choices, with its evidence. It is a proposal: the owner
decides, and #344's own design records the result. Values are in
[results/report.md](results/report.md); every figure below traces to `results/ceilings.json`,
`reversal_blended_spread.csv` or `ceiling_effects.csv`.

| # | Choice | Recommendation |
|---|---|---|
| 1 | Registry or global | A per-market registry, kept apart from `MarketSpec` |
| 2 | Global `SetParams` | Clamp each close's charge to its market's ceiling; do not refuse against the strictest |
| 3 | Reversals | Refuse a reversal whose blended spread exceeds the ceiling; pure closes stay open |
| 4 | Rounding | Floor the adjustment to the exact bound; no one-atom allowance |
| 5 | Listing without a ceiling | Refuse it; no fallback |

Choices 1 and 2 depend on each other, as the second section says.

## 1. Per-market registry

The ceilings span 13 to 35 bps by class: FX and indices 13, commodities 13 to 20, shares 13 to 20, crypto
30 to 35. They differ because the genesis price rules do: the divergence limit is 5 bps for FX, 10 for
indices, commodities and shares, and 25 for crypto, and the ceiling covers that limit plus a one-second tail.
One global value fails whichever way it is set:

- At the largest value (35 bps), 14 of 37 markets (every FX, index and commodity market) exceed the 30%-of-margin
  burden limit; EUR-USD's 50-bp minimum margin would pay 70% of itself to close.
- At the strictest (13 bps), 23 of 37 markets may charge less than their reference-risk need.

Representation:

- A list of `{ market_id, max_close_spread_bps }` in genesis, stored in engine state apart from the market
  specification, so `UpsertMarket` cannot replace, erase or raise an entry. The calibrated values stay in
  `results/ceilings.csv`; the protocol carries only the number.
- Genesis refuses a duplicate ID, an ID with no listed market, a listed market with no entry, a value outside
  0 to 9,999 (an arithmetic limit, not an economic range), and a value below the genesis parameters' largest
  close charge (stress minimum plus top toxicity add-on, 13 bps here), which would make the launch
  configuration illegal. Zero is a legal ceiling and means free closes: no nonzero charge meets it.
- An entry follows its market ID. `UpsertMarket` must not change `asset_class` or `quote_currency` of an ID with
  an entry: the calibration is per instrument, so a different instrument is a new ID with its own entry.
- Routine calendar corrections, price-rule edits and margin changes leave an entry alone. A price-rule or
  margin change does call for re-running this pipeline before the next adoption.
- A ceiling that moves with volatility would be an operator-adjustable limit. This proposal gives none; an
  automatic adjustment needs its own explicit decision.

## 2. Clamp, not refuse

A pure close's charge is the stress minimum plus the toxicity add-on, so today it is a function of the global
parameters alone: at most 13 bps on every market. Refusing a `SetParams` whose maximum close charge exceeds the
strictest ceiling would hold that sum at 13 for every market, whatever the registry says: 23 of 37 markets
would have headroom they can never use (crypto 17 to 22 bps, shares and commodities up to 7). The registry would be
decoration above the floor.

Clamping each close fill to `min(charge, ceiling)` at the pricing site keeps the per-market values meaningful
and makes the guarantee structural: every close, whichever command changed the parameters, stress levels,
toxicity tiers or spread tiers, pays at most its ceiling. It needs no change to what `SetParams` validates.
Nothing about launch behaviour changes: no close pays above 13 bps at the launch parameters, and every
ceiling is at least 13.

The cost is a change to #344's dev scenario. A signed `SetParams` that would raise a close above its
ceiling is accepted and the close pays the ceiling, instead of being refused as `admin_rejected`; the fill
records the charge actually paid. If the owner wants the refusal, it is sound but pairs with one global
value of 13 bps and choice 1 collapses: the registry then adds nothing.

The cap protects what the house forgoes only weakly: in a 5% gap on EUR-USD with no house capital, an uncapped
103-bp close charge would cut the shortfall by a fifth to a third. That is the price of the protection, and
the ceiling is not a gap-risk instrument (`ceiling_effects.csv`).

## 3. Reversals: refuse, keep the pure close

A reversal fills at one blended price: the opening slice's size spread is divided over the whole quantity, so
it lands on the closing slice too. Over a grid of 6 position sizes and 4 opening fractions per market, the
share of reversals whose blended spread exceeds the market's ceiling is 0% at calm, 1.9% at the elevated stress
level and 16.4% at the top level (shares 28%, commodities 7%, indices 4%, FX and crypto 0%). The grid is not a
flow distribution, but it shows refusal costs nothing in normal conditions.

Recommended rule: clamp the stress and toxicity part (it applies to every fill), then refuse a reversal when
that plus the opening slice's size cost per unit exceeds the ceiling. A trader refused can send the reduce-only
close, which stays within the ceiling, and then open, which still pays its full size charge and passes the
admission checks; the protection never traps a position that a person is present to close.

A resting stop that triggers as a reversal has no such person: the engine cancels a triggered order as
`Unfillable` on any refusal but slippage, so a refused stop-and-reverse would cancel and leave the position
open. Recommended: for a triggered order, fill the closing slice and cancel the rest. That is a small addition to
the refusal, and where the owner would rather not add it, splitting is the alternative that handles it
naturally.

Splitting a reversal into a closing and an opening fill gives the same protection and keeps the flip, but
turns one order into two fills: a new event shape for the node's history index, stream and markouts, to serve
a case that arises mostly at the top stress level. Clamping the blended spread instead would silently discard
size spread the house priced to cover large openings. Refusal is the smallest exact change.

## 4. Rounding: floor to the exact bound

Charges round upward at nine decimals, so a close's adjustment can sit one raw unit above its nominal bps.
The exact check is `adjustment_raw * 10_000 <= index_raw * ceiling_bps` in checked wide arithmetic; flooring
the adjustment to `index_raw * ceiling_bps / 10_000` meets it exactly.

- A seeded search of 20,000 pure closes found at most 1 raw unit above the exact bound.
- It matters only at tiny prices: at a 0.65 index one raw unit is 0.000015 bps; at an index of 777 raw units, 12.7 bps.
- Flooring makes a close free only below 770 raw units (7.7e-7), which no launch market approaches.
- The house forgoes at most one raw unit of price per unit of quantity on a fill that sits exactly at the ceiling.

An allowance would need a tolerance in every test and proof, and an unscaled one-unit allowance is huge at a
one-unit index. The floor needs neither. The truncated `Fill.spread_bps` must not be the checked quantity.

## 5. A listing with no ceiling: refuse

#147 forbids hiding missing evidence behind a uniform fallback, and the table shows none is defensible: a
single value is above the burden limit for 14 markets at 35 bps and below the need of 23 at 13. A class
fallback would extend one asset class's evidence to an instrument that was never calibrated. So a post-genesis
`UpsertMarket` of an ID with no entry is refused.

That leaves the policy question this research cannot answer: how an entry for a new market is adopted. Letting
the ordinary operator assign any new ceiling would reopen the power #109 bounds. A candidate is a signed
command that carries the market ID and its ceiling after the notice period the other bounds use, with the
value coming from this pipeline run on the new instrument. The owner decides the signer, the notice and any
bound before #344 is designed.

## Decisions for the owner

- Adopt or amend the objective: the 13-bp floor, the 30%-of-margin burden limit, the 5-bp step and the tail statistic.
- FX and index ceilings sit at 26% of their 50-bp margin because the launch close charge itself does: consider
  a lower launch stress minimum for them.
- Adopt the per-market values, or ask for specific markets to wait for execution-horizon reference data
  (DE40 and UK100 are low confidence; BRENT's value uses a refit after its validation shock).
- The five choices above, and the path for a new market's entry.

## Development, migration and promotion

Development and Anvil configurations carry unequal, labelled test ceilings and exercise: a clamped close
under raised parameters, a refused reversal with a pure close still open, an `UpsertMarket` that tries to
change an entry, a refused unlisted market, and a tiny-price close. Those values are fixtures, never
calibrated estimates. The change ships as a new protocol version with its own reference log, assigned when
the change is built; research changes no protocol, log or configuration.
