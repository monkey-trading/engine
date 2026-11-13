# Launch caps at the launch pricing

## Summary

This study measures the launch exposure-cap floors and the level-1 utilisation trigger at the
pricing that ships: #623's spread tiers in tenths of a bp, each fill through its bands at its tier's
best-price size, the owner's 10 s fee curve and D5's tier leverage, at tier 0 and at tier 3, the
ends of the volume-tier range, on the genesis's 34 launch markets. It changes no genesis value; the
owner decides (#614, decision D8) and F11 (#696) applies.

**Proposed for the owner:**

| Genesis value | Today | Proposed |
|---|---|---|
| `house_cap_floor`, per market | none (every cap is zero at zero NAV) | BTC-USD $31.6M; ETH-USD, US500, US100 $17.8M; the other FX, index and commodity markets $10M; SOL-USD $5.62M; each share $3.16M |
| Their sum | $0 | $244M |
| `house_total_cap_floor` | none | $61M |
| Level-1 `utilisation_bps` | 7000 | 8000 |
| Level-2 `utilisation_bps` | 9500 | 9500 |

- **On the cap study's flow no floor set from $100k to $30M a market puts house NAV or cash below
  zero in any of 2,000 paths at either end**, since every opening fill pays its spread to the house
  as it fills; without spreads $1M a market measured 0.85% (#662). Every target, 1% to 0.01%,
  chooses the same set, and the floors trade refusals against the exposure the house may carry
  unbacked.
- **The proposed floors are the least that serve every market's flow:** each market at the floor
  where its own refusals end at tier 3, refusing 0.1% to 0.3% of directional notional over 90 days
  (no market over 0.4%) and 0.2% to 2.4% in launch week. $1M a market refuses 29% to 38% of
  launch-week notional.
- **The total floor does not bind** down to 25% of the market floors' sum, $61M, the smallest share
  tested.
- **Level 1 at 8000** is the lowest utilisation level that fires in under 2% of open hours at both
  ends at those floors (1.47% at tier 3); the genesis's 7000 fires in 2.65%.
- **On the target client's flow** the spread income is a third, and at the proposed floors NAV
  falls below zero in 3% of paths within the first hours (lowest NAV at its 1st percentile -$115k
  to -$462k) while house cash stays within 1% (0.95% at worst). Small floors ($100k to $3M a market)
  keep NAV within 1% there but refuse 21% to 87% of launch-week notional; seed capital of about
  $0.5M to $1M removes the dip at the proposed floors.

**Paired positions across unpriced moves** at 1% margin on BTC-USD and ETH-USD, through tier-2
accounts, at the engine's level spreads (V52's size multiplier and the level the engine sets at the
repricing): pairs reopened before each spell, as #549's operator runs them, lose on 2025-10-10's
four stress hours. Pairs opened ahead at level 0 take $12.9M of house loss there at $50M a side
(136 pairs, about $95k each), and $80k a day on the year before about $14k a day of financing per
standing pair (95% interval -$121k to $397k). This reconciles with #734: the same code reproduces
its -$0.58M and $18.0M, and #697's $30.6M, under their conventions.

**Paired positions across session gaps** lose on every FX market at every tier and on every
market at tier 0's 20x, but pay across weekends at the market's own leverage (tiers 2 and 3) on
WTI, Japan 225, silver and US Tech 100: a pair held over every WTI weekend at 100x returns about a
third of its legs' notional a year, sixteen times the margin it posts, and the house's bad debt on
it is 45% of the legs' notional a year. A pair's two legs leave the house's net exposure unchanged, so no cap
floor limits how much of it an attacker holds; only margin over closures, or a fee or spread that
the pair pays at the gap, does. At tier 1's 50x only WTI's weekends pay.

## Decisions for the owner

1. **The floors and the measure (D8).** The proposed $244M set keeps house cash within 1% of paths
   on both flows and serves every market. If the measure is NAV below zero on the target client's
   flow, it measures 3%, and only floors of $3M a market or less meet 1%, refusing 21% to 41% of
   launch-week notional.
2. **Whether to seed the house.** About $0.5M to $1M of opening capital removes the target flow's
   early dip at the proposed floors; floors cannot.
3. **Level-1 utilisation at 8000**, from today's 7000, at those floors.
4. **Weekend margin on gap-prone markets.** Pairs held over weekends at the market's own leverage
   pay on WTI, Japan 225, silver and US Tech 100, and no floor limits them. They stop paying at a
   weekend margin of 7.5% on WTI, 1.5% on Japan 225 and silver, and 1% on US Tech 100; tier 0's 20x
   already stops them.
5. **Unpriced pairs at 1% on BTC-USD and ETH-USD (D4).** Pairs opened ahead take $12.9M of house
   loss on 2025-10-10's stress hours through tier-2 accounts (136 pairs, $1M of margin each), and
   pairs reopened before each spell lose. #734 measures what its loss-rate trigger leaves of them.
6. **How fast accounts earn tiers.** At tier 3 the honest share passes the attack classes on both
   flows; if every account stayed a fresh wallet, the share would be $151k a day on target flow
   against $395k of attack classes. The tier distribution is bracketed here, not measured.

## The question

#500 (E1) asks for every launch market's exposure-cap floor, the total floor and the level-1
utilisation trigger, at the pricing that ships, for the owner to decide the launch caps on (#614,
decision D8). #662 re-ran the cap study at 100x on BTC-USD and ETH-USD without spreads or the
holding-time fee; #623 and #697 priced spreads and the fee but ran neither the joint floors nor the
utilisation stage. This study:

- re-runs `research/cap-calibration`'s per-market, joint, total-floor and utilisation stages for the
  genesis's 34 launch markets (#685 deferred DE40, UK100 and BRENT) at the launch pricing bundle;
- restates the retained NAV, house income and the honest-share comparison at that bundle;
- re-runs #549's paired positions across unpriced moves at 1% on BTC-USD and ETH-USD through tier-2
  accounts, on the year and on 2025-10-10's stress hours (#614, decision D4);
- measures paired positions across session gaps on the FX, index and commodity markets at each
  tier's leverage, which #623 measured only across unpriced spells on crypto.

It changes no genesis value: F11 (#696) applies what the owner decides.

## The launch pricing bundle

`caps/pricing.py` builds what each volume tier pays, from `config/production-genesis.toml` and the
owner's decisions on #614:

| Tier | Best-price size (F5) | Initial margin floor (D5) | Fee past the curve (D7, F7) |
|---|---|---|---|
| 0 | a quarter of the first spread bound | 5% (20x) | 10% |
| 1 | half of it | 2% (50x) | 10% |
| 2 | the first bound | none | 9% |
| 3 | two and a half times it, at most the second bound | none | 8% |

- **Spreads** are the genesis's bands (#623's tiers, D6's tenths on FX and indices, V52). Every fill
  pays the staircase on the slice it opens or closes (D2), so a position opened and closed in one
  order pays its staircase twice. The cap model prices each order on its own notional
  (`research/cap-calibration`'s `Scenario.spread_bands`): in each hour the orders that fit under a
  side's room pay their own mean, so a small floor fills only small orders, at the first band. Each
  cell of the size profile pays the rate at its lower bound. A position pays its closing spread at
  its exit, from what it leaves its client, so the house's early cash holds only the opening
  spreads of the positions still open.
- **The closing clamp** (D10, V40) never binds at stress level 0: every market's widest band is
  below its ceiling (BTC-USD 8 bps against 30, the widest share 12 against 27,
  `pricing.check_ceilings`). The model prices every fill at level 0, which, clamp included, is the
  least a fill pays at any level.
- **The fee** is `max(base x (1 - discount), C(hold))` on the engine's table of the owner's curve,
  100% to 10 s and 10% at 15 minutes (#614, 2026-11-13; `calib.fee.curve_points`), charged on
  realised PnL after both fills' spreads (#697's corrected income model). The genesis's
  `profit_fee_curve` (V55) is the same table to the millisecond.
- **Leverage.** D5's floor sets the initial requirement only; close-outs keep the market's bands. At
  tier 0 every market that allows more than 20x (BTC-USD, ETH-USD, the FX, index and commodity
  markets) takes a flat 5% for new risk, while its close-out level stays at half its first band, so
  a tier-0 position on BTC-USD loses 4.5% before its close-out.

**The tier distribution.** The derived flow has no account volumes, so how the flow spreads across
tiers is not known. Every stage runs at both ends of the range: every account at tier 0, a fresh
wallet, which pays the most spread and fee per notional and takes at most 20x; and every account at
tier 3, which pays the least and takes the market's own leverage. Every account starts at tier 0,
and tier 2 takes $25M of counted volume over five days or more, so the house's first days, when the
floors carry it, are tier 0's. A floor set meets a target only if it meets it at both ends.

**Tier 0's flow profile is approximated.** The derived data has no client-flow profile for a flat
5% schedule (`pricing.with_flat_schedule`). Papertrade's leverage bins above 50x buy exactly 20x per
margin there. The 1x to 50x bin buys `E[min(L, 20)]`, between bounds from what it buys at 10x (SOL-
USD's schedule) and its papertrade mean: 13.3 to 16.5 for directional flow and 17.1 to 18.5 for
hedged, of which the model takes the midpoints, 14.9 and 17.8. Order sizes up to the 10x schedule's
$1M bound scale by the kind's change in notional per margin (x1.96); larger orders were cut by that
schedule's bands, and their margin buys 20x.

## Floors

The cap study's per-market sweep and joint run (`research/cap-calibration`, #111) run at each end of
the tier range on 2,000 paths of 90 days from a house with no capital, seed 1, at the genesis's 34
launch markets. The candidates are each measure's risk-level floors from each end's per-market sweep and
the same floor on every market from $100k to $30M, each run on both ends' paths
(`results/tier0/joint.csv`, `results/tier3/joint.csv`).

**On the cap study's flow no floor set puts the house below zero.** At both ends, from $100k on every market ($3.4M in
total) to $30M on every market ($1.02B), no path has house NAV or house cash below zero at any hour
(one-sided 95% bound under 0.15%), and the 1st percentile of the lowest NAV is positive: $25k to
$119k at tier 0, $46k to $384k at tier 3. Without spreads or the holding-time fee the same model put
NAV below zero in 0.85% of paths at $1M a market and 5.85% at $10M (#662). Every opening fill pays
its spread to the house as it fills, and over 90 days spreads bring $143M to $155M at tier 0 and
$457M to $490M at tier 3 against $22M to $58M of profit fees, so the house is ahead from its first
hour. The floors therefore trade refusals against the exposure the house may carry unbacked, and
the targets of 1%, 0.1% and 0.01% choose the same set.

**The proposal** is the smallest set that also serves every market's flow, refusing at most 1% of
each market's directional notional over the 90 days at both ends (`house.propose`): each market at
the floor where its own refusals end at tier 3, $243.8M in total.

| Floors | Total | Refused, launch week, tier 0 / tier 3 | Refused, 90 days | Worst market, 90 days | NAV day 90 median, tier 0 / tier 3 |
|---|---|---|---|---|---|
| $100k on every market | $3.4M | 48.5% / 55.5% | 7.0% / 6.7% | 7.8% / 7.3% (BTC-USD) | $166M / $513M |
| $1M on every market | $34M | 29.1% / 37.7% | 4.4% / 4.5% | 5.2% / 5.2% (BTC-USD) | $171M / $525M |
| Each market's floor where it alone never dips, at most where its refusals end (tier 3) | $147M | 2.6% / 3.2% | 0.3% / 0.4% | 2.6% / 1.5% (WTI) | $179M / $548M |
| **Each market's floor where its refusals end, tier 3 (proposed)** | **$244M** | **0.2% / 2.4%** | **0.1% / 0.3%** | **0.4% / 0.4% (WTI, BTC-USD)** | **$180M / $549M** |
| $10M on every market | $340M | 5.6% / 12.3% | 1.1% / 1.5% | 1.6% / 2.3% (BTC-USD) | $178M / $543M |
| $30M on every market | $1.02B | 0.0% / 2.2% | 0.0% / 0.3% | 0.0% / 0.5% (BTC-USD) | $180M / $549M |

Proposed `house_cap_floor` per market (`results/proposal.toml`, in the genesis's fields), the cap
study's floor grid's values, which the owner may round:

| Markets | Floor |
|---|---|
| BTC-USD | $31.6M |
| ETH-USD, US500, US100 | $17.8M |
| EUR-USD, GBP-USD, USD-JPY, AUD-USD, US30, JP225, XAU-USD, XAG-USD, WTI | $10M |
| SOL-USD | $5.62M |
| Each of the 20 shares | $3.16M |

- **The house's exposure stays far below the floors.** The flow nets: the median over paths of the
  house's largest gross net exposure is $32M at tier 0 and $66M at tier 3 at the proposed floors,
  and $24M and $59M at $100k a market. A floor binds mostly through the size of the largest
  order it admits, so most of the $244M is never carried at once.
- **Smaller sets refuse more in launch week and on single markets.** The $147M set gives 12 share
  markets no floor at all, so they refuse every order until house NAV builds their share cap, and
  leaves WTI refusing 2.6% over 90 days. $1M a market, the cap study's choice without spreads
  (#662), refuses 29% to 38% of launch-week notional.
- **Today's genesis** sets no floor, so every cap is zero at zero NAV, no opening trade fills, and
  utilisation reads full on every market.

**The total floor does not bind.** At the proposed market floors, a total floor of 25% of their
sum ($61M) to 100% ($244M) leaves every measure where it was at tier 0, and at tier 3 raises
launch-week refusals from 2.40% to 2.57% at 25%; every market still refuses under 0.5% over the 90
days (`results/tier*/total.csv`). The house's largest gross net exposure has a median of $32M at
tier 0 and $64M to $66M at tier 3, so a total floor much below 25% would start to bind. The proposal
takes the smallest share tested that meets the target and serves every market at both ends: a
`house_total_cap_floor` of $61M.

### On the target client's flow

The joint run is repeated on the target client's flow (#623's reshaping: the flow held an hour or more,
the within-hour flow held 15 to 60 minutes, and a tenth of the rest; `results/tier*/joint_target.csv`),
the yardstick the owner set for honest cost (#614). That flow brings a third of the spread income,
and the early dip returns:

| Floors | NAV<0 ever, tier 0 / tier 3 | Cash<0 | NAV<-$100k | NAV<-$1M | Lowest NAV, 1st percentile | Refused, launch week |
|---|---|---|---|---|---|---|
| $1M on every market | 0.20% / 0.40% | 0.20% / 0.40% | 0 / 0 | 0 / 0 | $10k / $15k | 38.9% / 63.0% |
| $3M on every market | 0.25% / 0.85% | 0.25% / 0.65% | 0 / 0.20% | 0 / 0 | $13k / $10k | 21.0% / 40.8% |
| **Proposed ($244M)** | **3.25% / 3.00%** | **0.20% / 0.95%** | **1.10% / 1.80%** | **0 / 0.50%** | **-$115k / -$462k** | **0.3% / 2.6%** |
| $10M on every market | 1.70% / 1.15% | 0.20% / 0.90% | 0.20% / 0.35% | 0 / 0 | -$17k / -$14k | 5.8% / 14.3% |

- **The proposal keeps house cash within 1% on target flow** (0.95% at tier 3), but NAV falls below
  zero in 3% of paths, in the first hours: the median first dip is 2 to 6 hours after launch, when
  the house holds exposure but few spreads. The dip is shallow: at its 1st percentile the lowest NAV
  is -$115k at tier 0 and -$462k at tier 3, and it passes -$1M in 0.5% of tier 3's paths.
- **Only small floors keep NAV within 1% on target flow:** every set from $100k to $3M a market
  does (0.2% to 0.85%), refusing 21% to 87% of launch-week notional. Seed capital removes the dip at
  the proposed floors where floors cannot: about $0.5M brings it to about 1% of paths, and $1M to
  0.5% at tier 3.
- `propose` on target flow (`results/proposal.json`, the `@target` keys): no set meets 0.1% on either
  measure; at 1% the house-cash measure keeps every market under 1% refused only at $30M a market,
  and the NAV measure's least-refusing set is $3M a market.

### Under the main sensitivities

The proposed floors, and 0.3 times them, run under the cap study's main sensitivities on 1,000 paths
(`results/tier*/sensitivity.csv`):

| Scenario, at the proposed floors | NAV<0 ever, tier 0 / tier 3 | Cash<0 | Bad debt mean, 90 days |
|---|---|---|---|
| Baseline | 0 / 0 | 0 / 0 | $74k / $418k |
| Target flow | 3.2% / 3.0% | 0.3% / 1.0% | $74k / $418k |
| Flow x0.25 | 0 / 0 | 0 / 0 | $18k / $104k |
| No farming (no hedged flow) | 0.3% / 0.2% | 0 / 0 | $15k / $120k |
| Volatility x2 | 0.3% / 0.1% | 0 / 0 | $528k / $1.47M |
| One client direction for every market | 0 / 0 | 0 / 0 | $74k / $417k |

At 0.3 times the floors target flow measures 1.4% and 1.2% (cash 0.4% and 1.1%), and every other
scenario stays at or below 0.2%. A quarter of the flow is safe because it keeps churn, whose spreads
come with little exposure; target flow removes most of it.

## Utilisation levels

The cap study's utilisation stage (`model/stress.py`, #133) runs at each end on 2,000 paths: per
market and candidate level, it counts the share of open hours in which the book an hour leaves
behind sits at or above the level of its cap. Its rule takes, for level 1, the lowest candidate whose fire rate, weighted by the
markets' flow, stays under 2%, with every market carrying 1% of the flow or more under 5%; level 2
stays at 9500, the last widening before refusals start. A level must pass at both ends.

| Level 1, bps | Tier 0: flow-weighted (BTC-USD) | Tier 3: flow-weighted (BTC-USD) |
|---|---|---|
| 5000 | 0.36% (0.7%) | 6.93% (11.7%) |
| 6000 | 0.08% (0.1%) | 4.38% (7.5%) |
| 7000, genesis | 0.01% (0.0%) | 2.65% (4.7%) |
| 7500 | 0.00% (0.0%) | 2.00% (3.6%) |
| **8000, proposed** | **0.00% (0.0%)** | **1.47% (2.7%)** |
| 8500 | 0.00% (0.0%) | 1.02% (1.9%) |
| 9000 | 0.00% (0.0%) | 0.60% (1.1%) |
| 9500, level 2 | 0.00% (0.0%) | 0.22% (0.4%) |

- **Level 1 at 8000.** At the proposed floors tier 0's lower leverage leaves every cap nearly empty,
  and tier 3 passes from 8000; the genesis's 7000 fires in 2.65% of tier 3's open hours. BTC-USD,
  which carries most of the flow, is the market that fills its cap. Without spreads no level below
  9500 passed at 100x (#662); larger floors, not pricing, make the difference.
- **Smaller floors fire constantly.** At $1M a market level 1 at 8000 fires in 6.1% of tier 0's
  open hours and 4.1% of tier 3's, and at $10M in 3.1% of each; the rule's choice at $10M is 9000.
- **The other triggers do not depend on the floors.** The fast-move thresholds (75 and 200 bps over
  60 s) come from each market's volatility and the divergence shares (6001 and 9500) from the
  genesis's price rules; #725 re-ran both on the 34 launch markets and kept them.

`results/tier*/stress_utilisation.csv` has every market's rates, and `stress_choice.json` the rule's
choice for each floor family.

## Retained NAV and the honest share

In the retained-NAV run (`model/retained.py`, #134; `house.income`) the house opens at each floor
of the retained grid as its capital, sweeps everything above it every hour, and trades under the
genesis's NAV-share caps, on 1,000 paths, on the current flow and on the target client's (#623's
reshaping, `calib/target.py`). The honest share is 20% of what the house distributes (#623). The
attack classes are #697's latency takes under the 10 s curve (`owner_latency.json`,
`owner_fx.json`), its held-sync and skimming figures, and this study's pairs, each as the operator's
take (80% of its accounts' profit, #549).

| Tier, flow | Retained NAV | Spread income, 90 days | Profit fees | Distributed | Honest share a day |
|---|---|---|---|---|---|
| Tier 0, current | $67.1M | $154M | $23.4M | $179M | $397k |
| Tier 3, current | $107M | $489M | $57.7M | $548M | $1.22M |
| Tier 0, target | $67.1M | $49.2M | $17.7M | $68.0M | $151k |
| Tier 3, target | $107M | $145M | $36.3M | $183M | $407k |

| The operator's take a day | Current flow | Target flow |
|---|---|---|
| Latency arbitrage, crypto, 3 s view (10 s curve) | $160k | $160k |
| Latency arbitrage, FX and indices at D6's tenths | $63k | $63k |
| Unpriced pairs at 1%, tier 2, opened ahead, before financing (upper bound) | $64k | $64k |
| Session-gap pairs, $50M a side on each market where they pay, tier 2 | $55k | $55k |
| Held sync's kill option | $2k | $2k |
| Skimming client fills (the loss-rate trigger cannot see it) | $159k | $47k to $51k |
| **All together** | **$504k** | **$391k to $395k** |
| Honest share, every account at tier 3 | $1.22M | $407k |
| Honest share, every account at tier 0 | $397k | $151k |

- **At tier 3**, #697's assumption, the criterion holds on both flows before the loss-rate trigger:
  on target flow by $16k a day, with both pair classes at their upper bounds. #697 found $385k of
  share on target flow at first-band spreads and the 1 s curve; the bands and the 10 s curve raise it
  to $407k.
- **If every account were a fresh wallet**, the share falls to $397k on current flow and $151k on
  target flow, since 20x buys a fifth of the notional on most of the flow, and the classes take
  more than either. The share rises as accounts earn tiers; how fast is the tier distribution this
  study brackets but does not measure.
- **The retained NAV** is $107M at tier 3, as #623 and #697 found, and $67.1M at tier 0, whose
  lower leverage needs less NAV behind the same NAV-share caps.
- The fastest honest client still takes $102k a day under the 10 s curve (#697; #734 finds the
  trigger leaves $101k).

## Paired positions across unpriced moves

#549's operator holds a long in one account and a short in another on BTC-USD or ETH-USD, unprices
the market as a fast move starts and prices it again once the move has run, so the losing leg's
close-out runs past its balance and the loss beyond it is the house's. The trigger rule is #549's:
unprice once the venues have moved 40 bps in 5 s, and price again once the move pays the margin over
one less the fee, plus 0.3%, or after 300 s. `caps/unprice.py` runs it at the launch rules:

- **Accounts.** 1% margin needs tier 2 or above (D5) and holds only on each account's first $2M,
  and each leg stays within its tier's best-price size ($2M on BTC-USD, $1M on ETH-USD at tier 2),
  so every fill pays the first band (2 bps on BTC-USD, 3 on ETH-USD) and a pair of $50M a side spans
  25 tier-2 accounts a side on BTC-USD and 50 on ETH-USD.
  The winning leg pays the fee past the curve, as if the pair had stood 15 minutes (#697's choice).
- **Balances.** The engine admits a fill only if the account's equity after it, at the mid, covers
  the requirement (`check_risk`), so each account holds 1% plus its opening spread, and that is the
  most its leg loses. The fee is on the realised PnL at the spread-adjusted fills.
- **Stress spreads.** From V52 a stress level multiplies the size spread: a fill pays the level's
  minimum plus the band times its multiplier (2 bps plus 2x at level 1, 10 bps plus 4x at level 2).
- **The move.** The engine keeps a source's quote only if it is no older than the one it holds
  (`marks.rs`, `store_quotes`), so a spell's move counts from no earlier than the market's last
  repricing, as #734 found; #549's rule counted it from 3 s before every spell.
- **The opening.** As #549 runs the rule, each spell opens a new pair just before it, at the level
  the engine's index sets then (`trigger`). A pair that has stood 15 minutes opened in a calm market,
  at level 0 (`standing`, #734's "opened ahead"): one per spell, each $50M a side.
- **The close.** Both legs close at one level, the loser's close-out included (D2). `engine` is the
  level the engine sets at the repricing (`stress.rs`): its move window keeps only the trailing
  60 s, so a market unpriced longer reads no move when it prices again, and a raised level steps
  down only once 300 s have passed since it last changed, so in a crash a short move can still
  close at level 2. `move` is the level the move reads as (#697's and #734's convention).

House loss at $50M a side over BTC-USD and ETH-USD, for the 300 s, 40 bps rule
(`results/unpriced_pairs.json` has every rule; this one is the attacker's best on the year in every
variant):

| Accounts | Opening | Closes at | A day, the year | 95% interval | 2025-10-10's four stress hours |
|---|---|---|---|---|---|
| Tier 2 (9% fee) | at the trigger | the engine's level | -$20.6k | -$187k to $227k | -$4.67M |
| Tier 2 | at the trigger | the move's level | -$34.7k | -$200k to $210k | -$0.40M |
| Tier 2 | standing, at level 0 | the engine's level | $80.3k | -$121k to $397k | $12.9M |
| Tier 2 | standing, at level 0 | the move's level | $66.2k | -$135k to $380k | $17.2M |
| Tier 3 (8% fee) | at the trigger | the engine's level | -$11.3k | -$181k to $245k | -$3.91M |
| Tier 3 | standing, at level 0 | the engine's level | $90.0k | -$116k to $416k | $13.9M |
| Tier 3 | standing, at level 0 | the move's level | $75.3k | -$131k to $399k | $18.2M |

- **Only pairs opened ahead pay.** Reopened before each spell at the crash's raised level, every
  variant loses on the stress hours. Opened ahead, tier 2's pairs take $12.9M on the stress hours,
  136 pairs of $50M a side, about $95k each, with $1M of margin and 50 to 100 tier-2 accounts
  behind each.
- **Reconciled with #697 and #734.** On the same hours this code gives #697's $30.6M under its
  rules (the band unmultiplied, every move from 3 s before its spell, each close at the move's
  level; `check_697`), and #734's -$0.58M reopened and $18.0M opened ahead under its (`check_734`).
  Both charge the fee on the gain less the closing spread; the engine charges it net of the opening
  spread too, so tier 3's $18.2M at the move's level is $0.14M above #734's $18.0M. Tier 2's $12.9M
  differs from it by tier 2's 9% fee and the engine's closing level, which holds level 2 through its
  cooldown more often than the move alone reads it (83 of 136 closes, against 8).
- **On the year** pairs opened ahead take $66k to $90k a day before financing. A pair standing
  ready across each day's 22:00 rollover pays about $13.7k a day at $50M a side (both legs' 5%
  markup), which these figures leave out. The intervals span zero.
- The operator keeps 80% of its accounts' take (#549): $10.3M of tier 2's $12.9M stress day.

## Paired positions across session gaps

A pair opened at the last hour's close before a market closes and closed at its reopening's open
leaves the house's net exposure unchanged, so no cap limits it except through the size of each
order, and the size of an order repeats across as many accounts as the attacker holds. Each leg's
loss stops at its balance, so a gap past the balance is the house's and the other leg keeps its gain
less the fee. `caps/gaps.py` prices each closure in the cap study's hourly bars (102 weeks to
2026-10-12) per unit of each leg's notional, for each tier: the balance is the tier's initial
requirement plus the opening fill's spread, which the account's equity must cover after the fill
(`check_risk`), both fills of both legs pay the first band, the winner pays the fee past the curve
on its realised PnL, and each leg pays financing at every rollover it crosses. A pair held across
every closure:

The pair's payoff a year at each tier, and the house's bad debt on it at tier 2, in bps of each leg's notional (the attacker's gain is the house's loss):

| Market | Closures a year | Mean gap | Tier 0 (20x) | Tier 1 (50x) | Tier 2 | Tier 3 | Past a tier-2 leg's balance | House's bad debt, tier 2 |
|---|---|---|---|---|---|---|---|---|
| EUR-USD | 57 | 13 bps | -237 | -237 | -176 | -169 | 2.7% | 53 |
| GBP-USD | 56 | 11 bps | -225 | -225 | -176 | -170 | 3.6% | 43 |
| USD-JPY | 56 | 17 bps | -284 | -284 | -184 | -175 | 4.6% | 91 |
| AUD-USD | 54 | 16 bps | -263 | -263 | -140 | -132 | 5.7% | 115 |
| US500 | 259 | 10 bps | -1,462 | -1,462 | -920 | -897 | 5.1% | 519 |
| US100 | 258 | 14 bps | -1,750 | -1,729 | -797 | -765 | 7.5% | 922 |
| US30 | 260 | 8 bps | -1,422 | -1,406 | -1,044 | -1,025 | 3.7% | 359 |
| JP225 | 259 | 17 bps | -1,937 | -1,631 | -298 | -258 | 7.7% | 1,599 |
| XAU-USD | 256 | 12 bps | -2,795 | -2,780 | -2,487 | -2,462 | 2.4% | 284 |
| XAG-USD | 256 | 23 bps | -3,023 | -2,201 | -1,015 | -963 | 7.2% | 2,008 |
| WTI | 267 | 38 bps | -2,349 | -357 | 1,271 | 1,364 | 9.0% | 4,752 |

A selective attacker holds pairs only across the closures that pay. Across weekends:

| Market | Closures a year | Mean gap | Tier 0 (20x) | Tier 1 (50x) | Tier 2 | Tier 3 | Past a tier-2 leg's balance | House's bad debt, tier 2 |
|---|---|---|---|---|---|---|---|---|
| EUR-USD | 52 | 14 bps | -228 | -228 | -168 | -161 | 2.9% | 53 |
| GBP-USD | 52 | 12 bps | -215 | -215 | -166 | -161 | 3.9% | 43 |
| USD-JPY | 52 | 18 bps | -274 | -274 | -174 | -166 | 4.9% | 91 |
| AUD-USD | 52 | 17 bps | -260 | -260 | -137 | -128 | 5.9% | 115 |
| US500 | 52 | 30 bps | -513 | -513 | -51 | -36 | 20.6% | 447 |
| US100 | 52 | 38 bps | -599 | -578 | 146 | 165 | 27.5% | 725 |
| US30 | 52 | 26 bps | -492 | -476 | -145 | -132 | 15.7% | 334 |
| JP225 | 52 | 51 bps | -682 | -376 | 717 | 743 | 28.4% | 1,374 |
| XAU-USD | 52 | 36 bps | -809 | -794 | -524 | -507 | 8.8% | 268 |
| XAG-USD | 52 | 73 bps | -946 | -356 | 545 | 582 | 28.4% | 1,508 |
| WTI | 52 | 139 bps | -117 | 1,815 | 3,225 | 3,295 | 33.3% | 4,497 |

- **WTI's weekend gaps are oil-wide.** The public front-month series switches contracts on its own
  calendar, which the engine's roll-adjusted index does not, but WTI's 102 weekend gaps correlate
  0.94 with Brent's over the same weekends (whose contract rolls on another calendar) and have the
  same mean size (1.36% and 1.35% of log move). The largest is 2026-03-06's +21%.
- **The take scales with capital.** It is a return on the margin posted, not a figure a cap bounds:
  at $50M a side on each market, across each kind of closure that pays at tier 2, the house loses
  about $69k a day.
- **Tier 0 never pays**, since 20x needs a 5% gap past the spreads, and tier 1's 50x pays only on
  WTI's weekends.
- **The margin that ends it.** At tier 3's fee, the least any tier pays, weekend pairs stop paying
  once each leg holds 1% on US Tech 100, 1.5% on Japan 225 and silver (where silver is about
  even), and 7.5% on WTI (5% still leaves WTI's weekends 0.2% of a leg's notional a year); at their
  own first bands every other market's already lose (`results/session_gaps.json`,
  `weekend_breakeven`).

## Limits

- **The flow is papertrade's.** The client flow, its sizes, holds and leverage, is the cap study's
  (ten days of a crypto venue at 1000x, carried over by margin); its limits apply here. BTC-USD's
  and ETH-USD's 100x profiles are approximated from the 100x oil schedule, and tier 0's from the 10x
  schedule (above).
- **The tier distribution is bracketed, not measured.** Tiers 1 and 2 lie between the two ends in
  every input, but the floors' risk need not be monotone between them.
- **Spread income is a lower bound, given the flow.** Fills price at stress level 0 and pay no
  toxicity add-on, and each size cell pays its lower bound's rate. The flow trades as it does at no
  spread; target flow and a quarter of the flow are the sensitivities that bound it.
- **The fee at tier 3** is the least any tier pays past the curve; churn's fee takes the least its
  hold moments allow (#697's bound).
- **Unpriced pairs** keep #549's trigger rule and its year estimates from 156 sampled hours; the
  interval on the year spans zero. Pairs opened ahead need one standing pair per spell and pay
  financing while they stand, which the figures leave out. Neither closing-level convention models
  the divergence trigger at the repricing, which can only raise the level.
- **Session gaps** are the hourly bars' reopening gaps: the first trade of the reopening hour, the
  stress level at the reopening taken as 0, and Yahoo's public series. Rollovers are counted at
  22:00 UTC throughout, as the cap model counts them.

## Re-running

From a virtualenv outside the repo with `requirements.txt` (Python 3.12 or newer), in this
directory. The house-model stages take hours; run them under `nice`:

```sh
python -I run.py sweep --tier 0                  # and --tier 3: about an hour each
python -I run.py joint --tier 0                  # and --tier 3, once both sweeps are done: about 50 minutes each
python -I run.py joint --tier 0 --flow target    # and --tier 3: the target client's flow, about 80 minutes each on two workers
python -I run.py propose
python -I run.py total --tier 0                  # and --tier 3: about 20 minutes each
python -I run.py stress --tier 0                 # and --tier 3: about 20 minutes each
python -I run.py sensitivity --tier 0            # and --tier 3: about 70 minutes each
python -I run.py income --tier 0 --flow current  # and target, and both at --tier 3: about 17 minutes each
python -I run.py gaps
python -I run.py unprice
python -I run.py report
python -I -m pytest tests
```

Each stage is seeded and deterministic; `results/manifest_*.json` records each house-model stage's
scenario, the hashes of the derived data it read and its run time. The times above are with three
workers under `nice -n 10` on a machine shared with other builds, as the committed results ran.

## Files

- `run.py`: the stages.
- `caps/`: `common.py` (the genesis and the sibling studies), `pricing.py` (each tier's pricing in
  the cap model's terms, and tier 0's flow profile), `house.py` (the cap study's stages at a tier,
  and the proposal across the two ends), `unprice.py` (unpriced pairs at the launch rules), `gaps.py`
  (session-gap pairs), `report.py` (`results/report.md` and `results/proposal.toml`).
- `tests/`: the pricing, the flat profile's bounds, the proposal rule, the stress levels and pair
  payoffs on hand cases.
- `results/`: `tier0/` and `tier3/` (each end's per-market sweep, joint runs on both flows, total-floor
  sweep, utilisation rates and choice, and sensitivities), `proposal.json` (each measure's and
  target's choice on each flow), `proposal.toml` (the proposed values in the genesis's fields),
  `income.json`, `unpriced_pairs.json`, `session_gaps.csv` and `.json`, `report.md`, and the manifests.
