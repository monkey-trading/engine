"""Price-adjustment model for the pinned V17 execution path, in nine-decimal raw units.

This models price formation, not admission, margin, profit fees or liquidation selection.
Agreement with Engine::apply must be established before using it to recommend a ceiling.
"""

from decimal import Decimal

SCALE = 1_000_000_000


def raw(value):
    scaled = Decimal(str(value)) * SCALE
    if not scaled.is_finite() or scaled != scaled.to_integral_value():
        raise ValueError("expected finite value with at most nine decimal places")
    return int(scaled)


def divide(numerator, denominator, up):
    if numerator < 0 or denominator <= 0:
        raise ValueError("charge arithmetic requires non-negative numerator and positive denominator")
    return (numerator + denominator - 1) // denominator if up else numerator // denominator


def growth(current, delta):
    new = current + delta
    if current == 0 or (current > 0) == (delta > 0):
        return abs(current), abs(new)
    if new and (new > 0) == (delta > 0):
        return 0, abs(new)
    return 0, 0


def staircase(tiers, start, stop, up):
    total, low = 0, 0
    for high, bps in tiers:
        end = stop if high is None else min(stop, high)
        begin = max(start, low)
        if end > begin:
            total += divide((end - begin) * bps, 10_000, up)
        if high is not None:
            low = high
    return total


def convert(amount, currency, fx, to_usd, up):
    if currency == "USD":
        return amount
    if currency not in ("EUR", "GBP", "JPY") or fx is None or fx <= 0:
        raise ValueError("non-USD amount needs a supported positive FX index")
    multiply = (currency in ("EUR", "GBP")) == to_usd
    return divide(amount * fx, SCALE, up) if multiply else divide(amount * SCALE, fx, up)


def adjustment(*, mid, current, quantity, buy, tiers, minimum_bps, toxicity_bps,
               multiplier_bps=10_000, liquidation=False, reduce_only=False, currency="USD", fx=None):
    """Return the actual raw price adjustment and reported whole bps for an admitted order."""
    if mid <= 0 or quantity <= 0 or min(minimum_bps, toxicity_bps, multiplier_bps) < 0:
        raise ValueError("invalid price, quantity or spread")
    if reduce_only:
        if not current or (current > 0) == buy:
            raise ValueError("reduce-only order does not reduce")
        quantity = min(quantity, abs(current))
    delta = quantity if buy else -quantity
    begin, end = growth(current, delta)
    add_on = 0 if liquidation else toxicity_bps

    def charge(up):
        start_usd = convert(divide(begin * mid, SCALE, False), currency, fx, True, False)
        end_usd = convert(divide(end * mid, SCALE, up), currency, fx, True, True)
        size_usd = divide(staircase(tiers, start_usd, end_usd, up) * multiplier_bps, 10_000, up)
        size_quote = convert(size_usd, currency, fx, False, True)
        return divide(mid * (minimum_bps + add_on), 10_000, up) + divide(size_quote * SCALE, quantity, up)

    if charge(False) >= mid:
        raise ValueError("spread at this size reaches the price")
    value = charge(True)
    if not buy:
        value = min(value, mid - 1)
    return {"adjustment_raw": value, "reported_spread_bps": value * 10_000 // mid,
            "price_raw": mid + value if buy else mid - value, "quantity_raw": quantity}
