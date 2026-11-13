"""Each launch market's markout-spread floor, and how often an honest account reaches a tier at it.

The score divides each fill's 5 s markout by its market's markout spread, the root mean square of
the index's 5 s changes over the trailing hour, never below the market's floor (#690). The owner set
the floor at the full year's markout spread (#614, 2026-11-13; #623 proposed half of it), which #623
measured on the trade grids of BTC-USD, ETH-USD and SOL-USD. The other markets have hourly bars
only, so their year's markout spread is scaled from the bars as `calib/volatility.py` scales the lag
move: a Brownian path with each open hour's Parkinson volatility moves `sigma * sqrt(5 / 3600)` in
root mean square over 5 s, and `RHO_5S` is the ratio the crypto markets give between their measured
year's markout spread and that scaling of their own bars. The ratio is crypto's; applying it to FX,
indices, commodities and shares is an assumption, which the owner accepted on the same date.

The floor matters in two cases:
- **trailing**: a fill's markout is scored in units of its hour's markout spread, or the floor when
  that is lower: a quiet hour cannot inflate scores past the floor;
- **at the floor**: every markout is scored in units of the floor, as when the host holds the index
  still for the trailing hour and releases it, so fills see ordinary moves against a denominator
  the held hour pushed down to the floor: an upper bound on what holding the index can do.

An honest account's markouts are the crypto hours' random fills in units of each hour's own markout
spread, a fat-tailed shape (kurtosis 9 to 17), rescaled to each case: by `sigma_h / max(sigma_h,
floor)` trailing and by `sigma_h / floor` at the floor, with `sigma_h` each open hour's scaled
markout spread and fills spread evenly over open hours. `toxicity.honest_reach` then gives the share
of edgeless accounts whose score reaches each tier within a window of fills.
"""

import numpy as np

from calib import toxicity

FLOOR_SHARE = 1.0  # of the year's markout spread (#614, 2026-11-13)
FLOOR_SHARES = (0.5, 0.75, 1.0, 1.5)  # the sweep: what each floor buys honest accounts and costs the stick
HORIZON_S = toxicity.HORIZON_S
FILL_WINDOWS = (50, 200, 1000)


def brownian_rms(sigma_hour, horizon_s: float = HORIZON_S):
    """The root mean square move over `horizon_s` of a Brownian path with hourly volatility `sigma_hour`."""
    return np.asarray(sigma_hour) * np.sqrt(horizon_s / 3600)


def year_rms(sigma_hours: np.ndarray) -> float:
    """The root mean square of the hours' volatilities: what a year's markout spread scales from."""
    return float(np.sqrt(np.mean(np.square(sigma_hours))))


def rho(measured_bps: dict[str, float], sigma: dict[str, np.ndarray]) -> dict[str, float]:
    """Per measured market, its year's markout spread over the Brownian scaling of its bars."""
    return {m: v / 1e4 / brownian_rms(year_rms(sigma[m])) for m, v in measured_bps.items()}


def year_spread_bps(sigma_hours: np.ndarray, ratio: float) -> float:
    """A market's year's markout spread, bps, scaled from its open hours' volatilities."""
    return float(ratio * brownian_rms(year_rms(sigma_hours)) * 1e4)


def floor_bps(year_bps: float, share: float = FLOOR_SHARE) -> float:
    """The floor, `share` of the year's markout spread rounded up to the hundredth of a bp."""
    return float(np.ceil(share * year_bps * 100 - 1e-9) / 100)


def scales(sigma_hours: np.ndarray, ratio: float, floor: float) -> dict[str, np.ndarray]:
    """Per open hour, the factor an honest markout in its own hour's units is multiplied by in each
    case: `sigma_h / max(sigma_h, floor)` trailing, `sigma_h / floor` at the floor (`floor` in bps)."""
    s = ratio * brownian_rms(sigma_hours) * 1e4
    return {"trailing": s / np.maximum(s, floor), "at_floor": s / floor}


def honest_reach(z0: np.ndarray, factor: np.ndarray, rng, accounts: int = 4000, draws: int = 200_000, windows=FILL_WINDOWS) -> dict:
    """The share of edgeless accounts reaching each tier within each of `windows` fills, their
    markouts `z0` (in their hour's units) drawn with an open hour each, rescaled by its `factor`."""
    z = rng.choice(z0, size=draws) * rng.choice(factor, size=draws)
    return {str(n): toxicity.honest_reach(z, n, rng, accounts=accounts) for n in windows}


def latency_edge(hours: list[tuple], floor: float) -> dict:
    """The latency strategy's mean edge a fill in score units with the hour's markout spread floored
    at `floor` (log terms), weighted as `toxicity` weighs its fills, and the fills it takes to tier 1:
    what a higher floor costs the stick in quiet hours."""
    z = np.concatenate([a / max(s, floor) for _, s, a in hours])
    w = np.concatenate([np.full(len(a), wt) for wt, _, a in hours])
    edge = float((z * w).sum() / w.sum())
    return {"edge_z": edge, "fills_to_tier_1": toxicity.fills_to_tier(edge, toxicity.Z_TIERS[0][0])}


def toml_lines(rows: list[dict]) -> str:
    """The floor each market would carry in genesis: a decimal of bps, as genesis writes its `Fixed`
    values, under a field name #690 settles."""
    return "\n".join(f'# {r["symbol"]} ({r["source"]}, year {r["year_bps"]:.2f} bps)\nmarkout_floor_bps = "{r["floor_bps"]:.2f}"'
                     for r in rows) + "\n"


def report(out: dict) -> str:
    """`results/markout_floors.md` from the stage's results."""
    from calib.report import pct, table

    rows = out["markets"]
    lines = ["# Markout-spread floors for every launch market (#699)", "",
             "Generated by `python -I run.py floors`. The floor is the year's markout spread (#614, 2026-11-13), the root mean "
             "square of the index's 5 s changes over the open hours: measured on the trade grids of BTC-USD, ETH-USD and "
             f"SOL-USD, and scaled from the hourly bars elsewhere by the crypto markets' ratio of measured to Brownian-scaled "
             f"markout spread, {out['rho_5s']:.3f} ("
             + ", ".join(f"{m} {v:.3f}" for m, v in out["rho_by_market"].items()) + "). Honest accounts' markouts are the "
             f"crypto hours' random fills in their hour's units (standard deviation {out['honest_z_sd']:.2f}, kurtosis "
             f"{out['honest_z_kurtosis']:.0f}). Reach is the share of 4,000 edgeless accounts whose score reaches tier 1 (a score "
             "of 4) or tier 2 (8) within 1,000 fills: trailing, each markout in its hour's units or the floor's when the hour "
             "is quieter; at the floor, every markout in the floor's units, the most holding the index still can do.", ""]
    body = [[r["symbol"], r["asset_class"], r["source"], f"{r['year_bps']:.2f}", f"{r['floor_bps']:.2f}",
             pct(r["reach"]["trailing"]["1000"]["4.0"], 2), pct(r["reach"]["trailing"]["1000"]["8.0"], 2),
             pct(r["reach"]["at_floor"]["1000"]["4.0"], 1), pct(r["reach"]["at_floor"]["1000"]["8.0"], 2)] for r in rows]
    lines += table(["Market", "Class", "Spread", "Year's markout spread, bps", "Floor, bps", "Tier 1, trailing", "Tier 2, trailing",
                    "Tier 1, at the floor", "Tier 2, at the floor"], body)
    lines += ["## Other floors", "",
              "The same measures with the floor at other shares of the year's markout spread (#623 proposed half): honest "
              f"accounts reaching tier 1 within 1,000 fills, across the {len(rows)} markets (median and worst), and on the "
              "crypto markets the latency trader's edge a fill in score units (an honest node's 0.25 s lead, moves past twice "
              "the first tier) and the fills it takes to tier 1.", ""]
    body = []
    for share in FLOOR_SHARES:
        k = str(share)
        trailing = [r["sweep"][k]["reach_1000"]["trailing"]["4.0"] for r in rows]
        at = [r["sweep"][k]["reach_1000"]["at_floor"]["4.0"] for r in rows]
        lat = [r["sweep"][k]["latency"] for r in rows if "latency" in r["sweep"][k]]
        body.append([f"{share:g}", pct(float(np.median(trailing)), 2), pct(max(trailing), 2), pct(float(np.median(at)), 1), pct(max(at), 1),
                     ", ".join(f"{x['edge_z']:.2f}" for x in lat), ", ".join(f"{x['fills_to_tier_1']:.0f}" for x in lat)])
    lines += table(["Share of the year", "Tier 1 trailing, median", "worst", "Tier 1 at the floor, median", "worst",
                    "Latency edge a fill (BTC, ETH, SOL)", "Fills to tier 1"], body)
    return "\n".join(lines).rstrip() + "\n"
