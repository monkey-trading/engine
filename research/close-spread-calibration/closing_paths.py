"""Evidence for the closing-path choices: reversal blending and raw-unit rounding.

Both use `execution.adjustment`, which agrees with `Engine::apply` on the shared vectors.
"""

import importlib.util
import random
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("execution", Path(__file__).resolve().parent / "execution.py")
EXEC = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(EXEC)

SCALE = EXEC.SCALE
# The genesis stress levels as (name, minimum spread bps, size-spread multiplier bps).
LEVELS = (("calm", 0, 10_000), ("elevated", 2, 20_000), ("top", 10, 40_000))
TOXICITY_BPS = 3
POSITIONS_USD = (10_000, 100_000, 500_000, 1_000_000, 2_000_000, 5_000_000)
OPEN_FRACTIONS = (0.1, 0.5, 1.0, 2.0)
MID = 100
SEED = 147


def tiers(spec):
    """Genesis spread tiers as (upper bound in raw USD, bps); the last tier is open."""
    return [(None if "up_to" not in tier else EXEC.raw(tier["up_to"]), tier["bps"])
            for tier in spec["spread_tiers"]]


def reversal(spec, level, position_usd, open_fraction):
    """Sell through a long position of `position_usd`, opening `open_fraction` of it on the other side."""
    _, minimum, multiplier = level
    quantity = position_usd // MID
    opening = round(quantity * open_fraction)
    result = EXEC.adjustment(mid=EXEC.raw(MID), current=EXEC.raw(quantity), quantity=EXEC.raw(quantity + opening),
                             buy=False, tiers=tiers(spec), minimum_bps=minimum, toxicity_bps=TOXICITY_BPS,
                             multiplier_bps=multiplier)
    return result["adjustment_raw"] * 10_000 / EXEC.raw(MID)


def reversal_rows(items, ceilings):
    """Blended per-unit spread of reversals against each market's ceiling and a pure close's charge."""
    rows = []
    for item in items:
        spec = item["market"]
        ceiling = ceilings[spec["symbol"]]
        for level in LEVELS:
            pure = level[1] + TOXICITY_BPS
            for position in POSITIONS_USD:
                for fraction in OPEN_FRACTIONS:
                    blended = reversal(spec, level, position, fraction)
                    rows.append({"symbol": spec["symbol"], "asset_class": spec["asset_class"],
                                 "stress_level": level[0], "position_usd": position,
                                 "opening_over_closing": fraction, "pure_close_bps": pure,
                                 "blended_reversal_bps": round(blended, 3), "ceiling_bps": ceiling,
                                 "exceeds_ceiling": blended > ceiling})
    return rows


def rounding_rows(charge_bps=13, mids=(1, 10, 777, 7_777, 77_777, 777_777, 77_777_777, 650_000_001,
                                       150_123_456_789)):
    """The charge the engine rounds up, against the exact nominal bound, for very small and typical indices."""
    rows = []
    for mid in mids:
        up = EXEC.divide(mid * charge_bps, 10_000, True)
        exact = mid * charge_bps // 10_000
        rows.append({"index_raw": mid, "index_units": mid / SCALE, "nominal_charge_bps": charge_bps,
                     "rounded_up_raw": up, "floor_bound_raw": exact, "excess_atoms": up - exact,
                     "excess_bps": round((up * 10_000 - mid * charge_bps) / mid, 8),
                     "floor_makes_close_free": exact == 0})
    return rows


def pure_close_excess_atoms(trials=20_000):
    """Largest raw-unit excess of a pure close's adjustment over its exact nominal bound, over seeded fills."""
    generator = random.Random(SEED)
    worst = 0
    for _ in range(trials):
        mid = generator.randrange(1, 10**12)
        minimum = generator.randrange(0, 11)
        toxicity = generator.randrange(0, 4)
        sell = bool(generator.getrandbits(1))
        if minimum + toxicity == 0 or (sell and mid == 1):
            continue
        result = EXEC.adjustment(mid=mid, current=10 * SCALE if sell else -10 * SCALE, quantity=SCALE, buy=not sell,
                                 tiers=[(None, 0)], minimum_bps=minimum, toxicity_bps=toxicity)
        bound = mid * (minimum + toxicity) // 10_000
        worst = max(worst, result["adjustment_raw"] - bound)
    return worst
