"""Monte Carlo of house NAV over the first months, hour by hour.

Each run carries `lanes`: sets of floors simulated on the same price paths and the same desired
flow, so floor levels compare without sampling noise between them.

Per hour and market:
1. Positions that reach their exit this hour settle: the planned exit after the class's hold, or
   a close-out once equity falls below 50% of margin, at the trigger in continuous trading or at
   the open when the market gaps through it. A loss beyond the balance is bad debt.
2. House NAV is house cash minus clients' unrealised PnL; each cap is the larger of its NAV share
   and its floor, per market and in total.
3. At a 22:00 rollover, every open position pays or receives financing on its notional at the
   mark.
4. New flow opens only in orders the engine would fill: one that grows the house's net exposure
   must fit under the market and total caps, one that shrinks it always fills. Of what fits, the
   side that reduces the house's net exposure fills in full and the crowded side up to the cap.
   Refused notional is counted.
5. Hedged flow opens offsetting pairs in two accounts, which leave the house's net exposure
   unchanged; each leg is an order the engine checks in turn, so a pair opens only while the
   returning leg lands the exposure back within the caps. A pair ends when either leg is closed
   out (the other leg closes with it) or at its hold.
6. Flow held under an hour (churn) pays the profit fee on its expected gain over its hold, and its
   mean open net notional takes the hour's move from its open.
7. Cash moves through the vault: exits return proceeds to idle balances, new margin and churn's
   dues are drawn from them first and the rest deposited, and a share of the proceeds left is
   withdrawn; idle balances leave at a daily rate, faster while NAV is below zero; a withdrawal
   the vault cannot pay waits in a first-in-first-out queue (#49).
"""

from dataclasses import dataclass

import numpy as np

from model import accounting
from model import prices as pricing
from model.flow import Flow, daily_margin_path, fit_share, schedule_key

KINDS = ("directional", "hedged")
ROLLOVER_HOUR_UTC = 22  # 22:00 London, which is UTC through the winter launch window
GAIN_PER_SD = 1 / np.sqrt(2 * np.pi)  # E[max(Z, 0)] for a standard normal Z


@dataclass
class Scenario:
    name: str = "baseline"
    paths: int = 1000
    seed: int = 1
    days: int = 90
    flow_scale: float = 1.0  # times papertrade's margin flow
    hedged_scale: float = 1.0  # times the measured hedged margin flow; directional flow is unchanged
    leverage_mapping: str = "capped"
    vol_scale: float = 1.0
    block_weeks: int = 4
    independent_markets: bool = False  # each market bootstraps its own weeks
    common_direction: bool = False  # one client long-share process for every market
    demean: bool = True  # remove each market's drift over the history
    hold_through_closures: bool = True  # a planned exit inside a closure waits for the open
    farm_half_life_days: float | None = None  # hedged flow decays with the MONKEY mint rate
    gap_pair_share: float = 0.0  # share of the past week's accepted margin in hedged pairs (held an hour or more) held through each closure, at max leverage
    withdraw_on_exit: float = 0.5  # share of exit proceeds withdrawn at once
    idle_withdraw_per_day: float = 0.05
    run_withdraw_per_day: float = 0.5  # while NAV is below zero
    batch_paths: int = 250
    utilisation_stats: bool = False  # record per-hour utilisation and next-hour book MTM (stress calibration)


@dataclass
class Markets:
    symbols: list[str]
    margin_rate: np.ndarray  # first margin tier
    schedule: list[str]  # margin band schedule (`flow.schedule_key`)
    cap_bps: np.ndarray
    total_cap_bps: int
    fin_long: np.ndarray  # financing per unit notional per rollover, paid by a long
    fin_short: np.ndarray
    mix: np.ndarray  # share of margin flow
    open_hours_per_week: np.ndarray
    columns: np.ndarray  # each market's column in the price data
    fx_columns: np.ndarray  # the column of the FX market a market converts at; -1 for USD
    fx_invert: np.ndarray  # yen: USD per unit is one over USD-JPY

    @property
    def max_leverage(self) -> np.ndarray:
        return 1.0 / self.margin_rate

    def subset(self, idx: list[int]) -> "Markets":
        return Markets(
            symbols=[self.symbols[i] for i in idx],
            margin_rate=self.margin_rate[idx],
            schedule=[self.schedule[i] for i in idx],
            cap_bps=self.cap_bps[idx],
            total_cap_bps=self.total_cap_bps,
            fin_long=self.fin_long[idx],
            fin_short=self.fin_short[idx],
            mix=self.mix[idx],
            open_hours_per_week=self.open_hours_per_week[idx],
            columns=self.columns[idx],
            fx_columns=self.fx_columns[idx],
            fx_invert=self.fx_invert[idx],
        )


def load_markets(genesis: dict, symbols: list[str], mix: np.ndarray, bars: np.ndarray, benchmark_bps: dict) -> Markets:
    by_symbol = {m["symbol"]: m for m in genesis["markets"]}
    specs = [by_symbol[s] for s in symbols]
    fin = [m["financing"] for m in specs]
    bench = np.array([benchmark_bps[f["benchmark"]] for f in fin])
    markup = np.array([f["markup_bps"] for f in fin])
    basis = np.array([f["day_basis"] for f in fin])
    open_hours = (bars[:, :, 0] != pricing.CLOSED).sum(axis=0) / (bars.shape[0] / pricing.WEEK)
    by_id = {m["id"]: m["symbol"] for m in genesis["markets"]}
    quote = [m.get("quote_currency", {"currency": "usd"}) for m in specs]
    return Markets(
        symbols=list(symbols),
        margin_rate=np.array([m["margin_tiers"][0]["bps"] / 1e4 for m in specs]),
        schedule=[schedule_key(m["margin_tiers"]) for m in specs],
        cap_bps=np.array([m["house_cap_bps"] for m in specs]),
        total_cap_bps=genesis["params"]["house_total_cap_bps"],
        fin_long=accounting.daily_financing(1, 1.0, bench, markup, basis),
        fin_short=accounting.daily_financing(-1, 1.0, bench, markup, basis),
        mix=mix,
        open_hours_per_week=open_hours,
        columns=np.arange(len(symbols)),
        fx_columns=np.array([symbols.index(by_id[q["fx_market"]]) if q["currency"] != "usd" else -1 for q in quote]),
        fx_invert=np.array([q["currency"] == "jpy" for q in quote]),
    )


@dataclass
class Prepared:
    """Everything one batch of paths needs that does not depend on the floors."""

    paths: pricing.Paths
    next_open: np.ndarray
    prev_open: np.ndarray
    rollovers: np.ndarray  # (paths, hours + 1, markets) sum of USD marks at the rollovers up to each hour
    margin_rate_hour: np.ndarray  # (paths, hours, markets) margin flow per hour
    leverage: np.ndarray  # (kinds, buckets, markets) notional over margin under the market's bands
    first_cross: np.ndarray  # (2 sides, kinds, buckets, paths, hours, markets) hours to the trigger; 0 = none
    horizon: int  # hours from an entry to its latest possible settlement


def is_rollover(hour) -> np.ndarray:
    """Whether a rollover (22:00) falls at the start of `hour`, between the previous bar's close
    and this bar's."""
    return (np.asarray(hour) % 24 == ROLLOVER_HOUR_UTC) & (np.asarray(hour) >= 1)


def rollover_marks(paths: pricing.Paths, hours: int) -> np.ndarray:
    """(paths, hours + 1, markets): the sum, over the rollovers up to and including each hour, of
    the USD mark (close times FX) at that rollover. A position opened at hour t and settled at
    hour x pays financing at the rollovers in (t, x], on its notional at each one's mark."""
    mark = paths.close * paths.fx
    at_rollover = np.zeros(mark.shape[:1] + (hours + 1,) + mark.shape[2:])
    hour = np.arange(1, hours + 1)
    take = is_rollover(hour)
    at_rollover[:, 1:][:, take] = mark[:, hour[take] - 1]
    return np.cumsum(at_rollover, axis=1)


def prev_open_hour(is_open: np.ndarray) -> np.ndarray:
    """(paths, hours + 1, markets): the last open hour at or before each hour; -1 when none."""
    n_paths, hours, n_markets = is_open.shape
    out = np.full((n_paths, hours + 1, n_markets), -1, dtype=np.int32)
    last = np.full((n_paths, n_markets), -1, dtype=np.int32)
    for t in range(hours):
        last = np.where(is_open[:, t], t, last)
        out[:, t] = last
    out[:, hours] = last
    return out


def first_crossings(paths: pricing.Paths, balance: np.ndarray, margin_rate: np.ndarray, horizon: int) -> np.ndarray:
    """Hours from each entry (at an hour's close) until the first bar in which a one-position
    account with `balance` (buckets, markets) per unit of notional falls below the close-out
    level, long at the bar's low and short at its high, valued in USD at the hour's FX, in an
    hour the market is open; 0 when it does not within `horizon` hours."""
    close = paths.close.astype(np.float32)
    low = paths.low.astype(np.float32)
    high = paths.high.astype(np.float32)
    fx = paths.fx.astype(np.float32)
    half_margin = (0.5 * margin_rate).astype(np.float32)
    n_buckets = balance.shape[0]
    n_paths, hours, n_markets = close.shape
    out = np.zeros((2, n_buckets, n_paths, hours, n_markets), dtype=np.int16)
    for k in range(1, min(horizon, hours - 1) + 1):
        g = fx[:, k:] / fx[:, :-k]
        x_low = low[:, k:] / close[:, :-k]
        x_high = high[:, k:] / close[:, :-k]
        # Equity less the close-out level, before the balance, per unit of entry notional.
        long_room = (x_low - 1) * g - half_margin * x_low * g
        short_room = -(x_high - 1) * g - half_margin * x_high * g
        tradeable = paths.is_open[:, k:]
        for j in range(n_buckets):
            for side, room in ((0, long_room), (1, short_room)):
                view = out[side, j, :, : hours - k]
                view[(room < -balance[j]) & tradeable & (view == 0)] = k
    return out


def prepare(scenario: Scenario, markets: Markets, bars: np.ndarray, flow: Flow, rng: np.random.Generator, n_paths: int) -> Prepared:
    hours = scenario.days * 24
    n_weeks = -(-hours // pricing.WEEK)
    history = bars.shape[0] // pricing.WEEK
    # The markets' own price columns, and those of the FX markets they convert at.
    needed = np.unique(np.concatenate([markets.columns, markets.fx_columns[markets.fx_columns >= 0]]))
    position = {c: i for i, c in enumerate(needed)}
    if scenario.independent_markets:
        weeks = np.stack([pricing.sample_weeks(rng, n_paths, n_weeks, history, scenario.block_weeks) for _ in needed], axis=-1)
    else:
        weeks = pricing.sample_weeks(rng, n_paths, n_weeks, history, scenario.block_weeks)
    sub = bars[:, needed]
    drift = pricing.drift_per_open_hour(sub) if scenario.demean else None
    paths = pricing.select(
        pricing.build(sub, weeks, hours, scenario.vol_scale, drift),
        np.array([position[c] for c in markets.columns]),
        np.array([position[c] if c >= 0 else -1 for c in markets.fx_columns]),
        markets.fx_invert,
    )
    next_open = pricing.next_open_hour(paths.is_open)
    prev_open = prev_open_hour(paths.is_open)

    day = np.arange(hours) // 24
    daily = daily_margin_path(flow, scenario.days, scenario.flow_scale)
    per_open_hour = daily[day][None, :, None] * markets.mix[None, None, :] * 7 / markets.open_hours_per_week[None, None, :]
    margin_rate_hour = np.where(paths.is_open, per_open_hour, 0.0)

    n_buckets = max(c.bucket for c in flow.classes) + 1
    leverage = np.array([[[flow.leverage.get((kind, scenario.leverage_mapping, s), np.ones(n_buckets))[b] for s in markets.schedule] for b in range(n_buckets)] for kind in KINDS])
    reopen = next_open[:, 1:] - np.arange(hours)[None, :, None]
    longest_wait = int(reopen[next_open[:, 1:] < hours].max(initial=1))
    horizon = max(c.hold_h for c in flow.classes) + longest_wait
    first_cross = first_crossings(paths, 1.0 / leverage.reshape(-1, len(markets.symbols)), markets.margin_rate, horizon)
    first_cross = first_cross.reshape((2, len(KINDS), n_buckets) + first_cross.shape[2:])

    return Prepared(paths, next_open, prev_open, rollover_marks(paths, hours), margin_rate_hour, leverage, first_cross, horizon)


def farm_decay(scenario: Scenario, hours: int) -> np.ndarray:
    if scenario.farm_half_life_days is None:
        return np.ones(hours)
    return 0.5 ** (np.arange(hours) / 24 / scenario.farm_half_life_days)


CHUNK_H = 24
# Ring channels: change in clients' long and short quantity and their cost, house cash, client
# proceeds. Quantity and cost are divided by the FX index at entry (see `run_batch`).
LONG, SHORT, COST, HOUSE, PROCEEDS = range(5)
# Total channels: fee, financing, bad debt, mint basis.
FEE, FINANCING, BAD_DEBT, MINT = range(4)


def planned_exits(prep: Prepared, scenario: Scenario, hold_h: int, t0: int, t1: int, hours: int) -> np.ndarray:
    """(paths, n, markets) planned exit hour of positions entering in [t0, t1) with `hold_h`."""
    t = np.arange(t0, t1)[None, :, None]
    target = np.minimum(np.arange(t0, t1) + hold_h, hours)
    planned = prep.next_open[:, target, :]
    if not scenario.hold_through_closures:
        last = prep.prev_open[:, target, :]
        planned = np.where(last > t, last, planned)
    return planned


def settle_at(prep: Prepared, markets: Markets, side, closed_out, exit_hour, balance, t0: int, t1: int, hours: int):
    """Exit price relative to entry and FX ratio of positions entering in [t0, t1): the close-out
    fill (the trigger, or the open when the market gaps through it) or the exit bar's close."""
    paths = prep.paths
    p = np.arange(exit_hour.shape[0])[:, None, None]
    m = np.arange(exit_hour.shape[2])[None, None, :]
    at = np.minimum(exit_hour, hours - 1)
    entry = paths.close[:, t0:t1]
    g = paths.fx[p, at, m] / paths.fx[:, t0:t1]
    trigger = accounting.closeout_trigger(side, balance / g, markets.margin_rate[None, None, :])
    fill = accounting.closeout_fill(side, trigger, paths.open[p, at, m] / entry, trigger)
    return np.where(closed_out, fill, paths.close[p, at, m] / entry), g


def rollover_units(prep: Prepared, exit_hour, t0: int, t1: int, hours: int):
    """Per unit of entry notional, the sum of the USD marks at the rollovers in (entry, exit]:
    times a rate per rollover, a position's financing."""
    paths = prep.paths
    p = np.arange(exit_hour.shape[0])[:, None, None]
    m = np.arange(exit_hour.shape[2])[None, None, :]
    marks = prep.rollovers[p, np.minimum(exit_hour, hours), m] - prep.rollovers[:, t0:t1]
    return marks / (paths.close[:, t0:t1] * paths.fx[:, t0:t1])


def account_exit(pnl, balance, financing):
    """One account's exit, per unit of notional: house cash, client proceeds, fee, bad debt, and
    the loss the account itself pays (the mint basis)."""
    fee, bad, proceeds = accounting.settle(pnl, balance, financing)
    return -pnl + fee - bad, proceeds, fee, bad, np.maximum(np.maximum(-pnl, 0.0) - bad, 0.0)


def pair_exit(markets: Markets, x, g, balance, units):
    """A pair's two accounts, a long and a short at the same notional and balance, closed at the
    same relative price `x`; each pays its own financing and loses at most its own balance."""
    long_leg = account_exit(accounting.realised_pnl(1, x, g), balance, markets.fin_long[None, None, :] * units)
    short_leg = account_exit(accounting.realised_pnl(-1, x, g), balance, markets.fin_short[None, None, :] * units)
    return tuple(a + b for a, b in zip(long_leg, short_leg))


def cohort_units(prep: Prepared, scenario: Scenario, markets: Markets, cls, side: int, t0: int, t1: int, hours: int):
    """Per unit of notional, the settlement of a directional cohort entering in each hour of
    [t0, t1): exit hour (paths, n, markets), ring channels (paths, n, markets, 5), and total
    channels (paths, n, markets, 4). A cohort settling past the horizon has zero channels; its
    financing is charged at each rollover from the books, as for every open position."""
    s_i = 0 if side > 0 else 1
    k, j = KINDS.index(cls.kind), cls.bucket
    balance = 1.0 / prep.leverage[k, j][None, None, :]
    t = np.arange(t0, t1)[None, :, None]
    planned = planned_exits(prep, scenario, cls.hold_h, t0, t1, hours)
    cross = prep.first_cross[s_i, k, j][:, t0:t1].astype(np.int64)
    closed_out = (cross > 0) & (t + cross <= planned)
    exit_hour = np.where(closed_out, t + cross, planned)
    x, g = settle_at(prep, markets, side, closed_out, exit_hour, balance, t0, t1, hours)
    rate = (markets.fin_long if side > 0 else markets.fin_short)[None, None, :]
    house, proceeds, fee, bad, mint = account_exit(accounting.realised_pnl(side, x, g), balance, rate * rollover_units(prep, exit_hour, t0, t1, hours))
    valid = exit_hour < hours
    entry_usd = prep.paths.close[:, t0:t1] * prep.paths.fx[:, t0:t1]
    quantity = -1.0 / entry_usd
    ring = np.stack([quantity * (side > 0), quantity * (side < 0), -side / prep.paths.fx[:, t0:t1], house, proceeds], axis=-1)
    totals = np.stack([fee, np.zeros_like(fee), bad, mint], axis=-1)
    return np.where(valid, exit_hour, t), ring * valid[..., None], totals * valid[..., None]


def pair_units(prep: Prepared, scenario: Scenario, markets: Markets, cls, t0: int, t1: int, hours: int):
    """Per unit of each leg's notional, the settlement of hedged pairs entering in [t0, t1). A pair
    ends when either leg is closed out, the other closing with it, or at its planned exit."""
    k, j = KINDS.index(cls.kind), cls.bucket
    balance = 1.0 / prep.leverage[k, j][None, None, :]
    t = np.arange(t0, t1)[None, :, None]
    planned = planned_exits(prep, scenario, cls.hold_h, t0, t1, hours)
    cross = [prep.first_cross[s, k, j][:, t0:t1].astype(np.int64) for s in (0, 1)]
    cross_hour = [np.where(c > 0, t + c, np.iinfo(np.int64).max) for c in cross]
    first = np.minimum(cross_hour[0], cross_hour[1])
    closed_out = first <= planned
    exit_hour = np.where(closed_out, first, planned)
    loser = np.where(cross_hour[0] <= cross_hour[1], 1.0, -1.0)
    x, g = settle_at(prep, markets, loser, closed_out, exit_hour, balance, t0, t1, hours)
    house, proceeds, fee, bad, mint = pair_exit(markets, x, g, balance, rollover_units(prep, exit_hour, t0, t1, hours))
    valid = exit_hour < hours
    quantity = -1.0 / (prep.paths.close[:, t0:t1] * prep.paths.fx[:, t0:t1])
    ring = np.stack([quantity, quantity, np.zeros_like(quantity), house, proceeds], axis=-1)
    totals = np.stack([fee, np.zeros_like(fee), bad, mint], axis=-1)
    return np.where(valid, exit_hour, t), ring * valid[..., None], totals * valid[..., None]


def gap_pair_units(prep: Prepared, markets: Markets, t0: int, t1: int, hours: int):
    """Per unit of each leg's notional, pairs at the maximum leverage opened at an hour in
    [t0, t1) that is the last open hour before a closure of 3 hours or more, and closed at the
    reopening open (the `gap_pair_share` stress). Zero where the hour starts no such closure."""
    paths = prep.paths
    t = np.arange(t0, t1)[None, :, None]
    reopen = prep.next_open[:, t0 + 1 : t1 + 1, :]
    starts = paths.is_open[:, t0:t1] & (reopen - t >= 3) & (reopen < hours)
    exit_hour = np.where(starts, reopen, t)
    p = np.arange(exit_hour.shape[0])[:, None, None]
    m = np.arange(exit_hour.shape[2])[None, None, :]
    at = np.minimum(exit_hour, hours - 1)
    balance = markets.margin_rate[None, None, :]
    g = paths.fx[p, at, m] / paths.fx[:, t0:t1]
    x = paths.open[p, at, m] / paths.close[:, t0:t1]
    house, proceeds, fee, bad, mint = pair_exit(markets, x, g, balance, rollover_units(prep, exit_hour, t0, t1, hours))
    quantity = -1.0 / (paths.close[:, t0:t1] * paths.fx[:, t0:t1])
    ring = np.stack([quantity, quantity, np.zeros_like(quantity), house, proceeds], axis=-1)
    totals = np.stack([fee, np.zeros_like(fee), bad, mint], axis=-1)
    return exit_hour, ring * starts[..., None], totals * starts[..., None], starts


def order_fits(exposure, cap, total_cap, flow: Flow, mapping: str, schedules: list[str]) -> dict:
    """Each side's share of margin in orders the engine fills, (paths, markets, lanes): directional
    longs and shorts, and hedged pairs. An order that grows the house's net exposure fills up to
    the room under the market and total caps, one that shrinks it always
    fills (`accounting.order_room`); a pair's legs must both fill, the returning leg checked with
    the first on the books (`accounting.pair_room`). A kind with no profile rows opens nothing."""
    other_gross = np.abs(exposure).sum(axis=1, keepdims=True) - np.abs(exposure)
    total_room = total_cap[:, None, :] - other_gross
    room_long, room_short = accounting.order_room(exposure, cap, total_room)
    leg_room = accounting.pair_room(exposure, cap, total_room)
    fits = {}
    for name, kind, room in (("long", "directional", room_long), ("short", "directional", room_short), ("pair", "hedged", leg_room)):
        fits[name] = np.zeros(exposure.shape)
        for key in dict.fromkeys(schedules):
            cols = [i for i, s in enumerate(schedules) if s == key]
            sizes = flow.sizes.get((kind, mapping, key))
            fits[name][:, cols] = 0.0 if sizes is None else fit_share(room[:, cols], sizes)
    return fits


def run_batch(scenario: Scenario, markets: Markets, bars: np.ndarray, flow: Flow, floors: np.ndarray, total_floors: np.ndarray, seed: int, n_paths: int) -> dict:
    """Simulates `n_paths` paths for every lane: `floors` is (lanes, markets) and `total_floors`
    (lanes,). Returns per-(path, lane) results. State keeps lanes on the last axis, so a
    settlement scheduled for one (path, market) writes one contiguous block of lanes."""
    rng = np.random.default_rng(seed)
    prep = prepare(scenario, markets, bars, flow, rng, n_paths)
    hours = scenario.days * 24
    n_lanes = floors.shape[0]
    n_markets = len(markets.symbols)
    paths = prep.paths
    shape = (n_paths, n_markets, n_lanes)
    lanes_shape = (n_paths, n_lanes)
    ring_len = prep.horizon + 2
    market_floors = floors.T[None]  # (1, markets, lanes)
    cap_bps = markets.cap_bps[None, :, None]

    # Clients' long and short quantity and their net cost basis, each divided by the FX index at
    # entry, so that a market's USD notional is price * FX * quantity and clients' unrealised PnL
    # FX * (price * (long - short) - cost).
    longs = np.zeros(shape)
    shorts = np.zeros(shape)
    cost = np.zeros(shape)
    house = np.zeros(shape)  # house cash by market
    ring = np.zeros((n_paths, ring_len, n_markets, 5, n_lanes))
    totals = np.zeros((n_paths, n_markets, 4, n_lanes))
    vault, idle, in_positions, queue = (np.zeros(lanes_shape) for _ in range(4))

    # `desired` and `refused_hedged` cover hedged flow too; `refused` and the week-one figures are
    # directional (churn included), as `desired_directional` is.
    out = {k: np.zeros(lanes_shape) for k in ("desired", "refused", "refused_hedged", "desired_directional", "desired_week1", "refused_week1", "deposits", "withdrawn", "max_queue", "max_gross_exposure")}
    out["min_nav"] = np.full(lanes_shape, np.inf)
    out["min_cash"] = np.full(lanes_shape, np.inf)
    out["ever_negative"] = np.zeros(lanes_shape, dtype=bool)
    out["queued"] = np.zeros(lanes_shape, dtype=bool)
    out["first_negative_h"] = np.full(lanes_shape, -1)
    started = np.zeros(lanes_shape, dtype=bool)  # the house has cash or exposure
    out["daily_nav"] = np.zeros((n_paths, n_lanes, scenario.days), dtype=np.float32)
    refused_by_market = np.zeros(shape)
    desired_by_market = np.zeros((n_paths, n_markets))
    identity_gap = 0.0

    # Stress-calibration instrumentation (`scenario.utilisation_stats`): per whole-percent
    # utilisation bucket, the open-hour count and the house's absolute exposure in USD; bucket 101
    # holds hours whose cap is not positive, which the engine reads as full utilisation.
    n_util_buckets = 102
    util_hist = np.zeros((1, n_util_buckets, n_markets, n_lanes))
    util_expo = np.zeros((1, n_util_buckets, n_markets, n_lanes))

    h_scale = scenario.hedged_scale
    decay = farm_decay(scenario, hours)
    directional = [c for c in flow.classes if c.kind == "directional" and c.hold_h > 0]
    hedged = [c for c in flow.classes if c.kind == "hedged" and c.hold_h > 0]
    cohorts = [(cls, side) for cls in directional for side in (1, -1)]
    churn = [c for c in flow.classes if c.hold_h == 0]
    deviation = np.zeros((n_paths, 1 if scenario.common_direction else n_markets))
    log_close = np.log(paths.close)
    usd_mark = paths.close * paths.fx
    p_idx = np.arange(n_paths)[:, None]
    m_idx = np.arange(n_markets)[None, :]
    directional_leverage, hedged_leverage = prep.leverage[0][:, None, :, None], prep.leverage[1][:, None, :, None]  # (buckets, 1, markets, 1)
    hedged_week = np.zeros(shape)  # margin accepted in hedged pairs over about the past week
    week_decay = np.exp(-1 / pricing.WEEK)
    run_rate = 1 - (1 - scenario.idle_withdraw_per_day - scenario.run_withdraw_per_day) ** (1 / 24)
    calm_rate = 1 - (1 - scenario.idle_withdraw_per_day) ** (1 / 24)

    def book(units, k, amount, need_rate):
        """Adds `amount` (paths, markets, lanes) of a cohort's notional to the books and schedules
        its settlement; returns the margin it posts, summed by lane."""
        exit_hour, ring_unit, total_unit = units
        nonlocal totals
        ring[p_idx, exit_hour[:, k] % ring_len, m_idx] += amount[:, :, None, :] * ring_unit[:, k][..., None]
        totals += amount[:, :, None, :] * total_unit[:, k][..., None]
        return (amount * need_rate[None, :, None]).sum(axis=1)

    for t0 in range(0, hours, CHUNK_H):
        t1 = min(t0 + CHUNK_H, hours)
        units = [cohort_units(prep, scenario, markets, cls, side, t0, t1, hours) for cls, side in cohorts]
        pairs = [pair_units(prep, scenario, markets, cls, t0, t1, hours) for cls in hedged]
        gaps = gap_pair_units(prep, markets, t0, t1, hours) if scenario.gap_pair_share > 0 else None
        for t in range(t0, t1):
            k = t - t0
            # Financing at a rollover, on every position open across it, at the mark before it.
            if is_rollover(t):
                mark = usd_mark[:, t - 1][..., None]
                charge = mark * (longs * markets.fin_long[None, :, None] + shorts * markets.fin_short[None, :, None])
                house += charge
                in_positions -= charge.sum(axis=1)
                totals[:, :, FINANCING] += charge

            slot = t % ring_len
            due = ring[:, slot]  # (paths, markets, channel, lanes)
            longs += due[:, :, LONG]
            shorts += due[:, :, SHORT]
            cost += due[:, :, COST]
            exit_cash = due[:, :, HOUSE].copy()  # the slot is zeroed below
            house += exit_cash
            proceeds = due[:, :, PROCEEDS].sum(axis=1)
            due[...] = 0.0

            entry = paths.close[:, t]
            fx = paths.fx[:, t]
            exposure = (entry * fx)[..., None] * (longs - shorts)  # USD notional at the mark
            nav = (house - (exposure - fx[..., None] * cost)).sum(axis=1)
            cap = accounting.exposure_cap(nav[:, None, :], cap_bps, market_floors)
            total_cap = accounting.exposure_cap(nav, markets.total_cap_bps, total_floors[None])

            if scenario.utilisation_stats:
                # The stress calibration reads each market's utilisation and absolute exposure at
                # the hour's mark.
                util_bucket = np.where(
                    cap > 0,
                    np.clip((np.abs(exposure) / np.where(cap > 0, cap, 1.0) * 1e4 // 100).astype(int), 0, 100),
                    101,
                )
                u_market = np.arange(n_markets)[None, :, None]
                u_lane = np.arange(n_lanes)[None, None, :]
                u_batch = np.zeros(util_bucket.shape, dtype=int)
                open_now = np.broadcast_to(paths.is_open[:, t][:, :, None], util_bucket.shape)
                np.add.at(util_hist, (u_batch, util_bucket, u_market, u_lane), open_now.astype(float))
                np.add.at(util_expo, (u_batch, util_bucket, u_market, u_lane), np.abs(exposure))

            # Client long share: AR(1) around each class's measured share, leaning on the last return.
            prev_ret = log_close[:, t - 1] - log_close[:, t - 2] if t >= 2 else np.zeros((n_paths, n_markets))
            if scenario.common_direction:
                prev_ret = prev_ret.mean(axis=1, keepdims=True)
            deviation = flow.long_share_ar1 * deviation + flow.long_share_slope * prev_ret + flow.long_share_resid_sd * rng.standard_normal(deviation.shape)

            fits = order_fits(exposure, cap, total_cap, flow, scenario.leverage_mapping, markets.schedule)

            margin_hour = prep.margin_rate_hour[:, t][..., None]
            wants = []
            zero = np.zeros(shape)
            desired_full = zero.copy()
            for cls in directional:
                notional = margin_hour * cls.margin_share * directional_leverage[cls.bucket]
                share = np.clip(cls.long_share + deviation, 0.0, 1.0)[..., None]
                wants += [notional * share * fits["long"], notional * (1 - share) * fits["short"]]
                desired_full += notional
            long_total = sum(wants[0::2], zero)
            short_total = sum(wants[1::2], zero)
            churn_long, churn_short, churn_hedged, hedged_full = zero.copy(), zero.copy(), zero.copy(), zero.copy()
            open_long, open_short = zero.copy(), zero.copy()  # churn's mean open notional over the hour
            gain_long, gain_short, gain_hedged = zero.copy(), zero.copy(), zero.copy()
            for cls in churn:
                gain = (GAIN_PER_SD * paths.parkinson[:, t] * cls.mean_sqrt_hold_h)[..., None]
                if cls.kind == "hedged":
                    full = margin_hour * cls.margin_share * hedged_leverage[cls.bucket] * h_scale * decay[t]
                    hedged_full += full
                    churn_hedged += full * fits["pair"]
                    gain_hedged += full * fits["pair"] * gain
                else:
                    full = margin_hour * cls.margin_share * directional_leverage[cls.bucket]
                    share = np.clip(cls.long_share + deviation, 0.0, 1.0)[..., None]
                    longs_in, shorts_in = full * share * fits["long"], full * (1 - share) * fits["short"]
                    desired_full += full
                    churn_long += longs_in
                    churn_short += shorts_in
                    open_long += longs_in * min(cls.mean_hold_h, 1.0)
                    open_short += shorts_in * min(cls.mean_hold_h, 1.0)
                    gain_long += longs_in * gain
                    gain_short += shorts_in * gain

            # The crowded side fills up to the cap, the side that reduces exposure in full; churn
            # competes for room with its mean open notional over the hour.
            want_long, want_short = long_total + open_long, short_total + open_short
            long_ok, short_ok = accounting.accepted_share(exposure, cap, want_long, want_short)
            change = long_ok * want_long - short_ok * want_short
            growth = np.maximum(np.abs(exposure + change) - np.abs(exposure), 0.0)
            room_total = np.maximum(total_cap - np.abs(exposure).sum(axis=1), 0.0)
            wanted = growth.sum(axis=1)
            cut = np.where(wanted > room_total, 1.0 - room_total / np.where(wanted > 0, wanted, 1.0), 0.0)[:, None, :] * growth
            with np.errstate(divide="ignore", invalid="ignore"):
                long_ok = np.where((change > 0) & (want_long > 0), np.maximum(long_ok - cut / want_long, 0.0), long_ok)
                short_ok = np.where((change < 0) & (want_short > 0), np.maximum(short_ok - cut / want_short, 0.0), short_ok)

            # Desired and refused opening notional; what does not fit counts as refused.
            accepted = long_ok * (long_total + churn_long) + short_ok * (short_total + churn_short)
            refused = desired_full - accepted
            desired_by_market += desired_full[..., 0]
            refused_by_market += refused
            pair_legs = [margin_hour * c.margin_share * h_scale * decay[t] / 2 * hedged_leverage[c.bucket] for c in hedged]
            hedged_full = hedged_full + sum((2 * leg for leg in pair_legs), zero)
            hedged_open = churn_hedged + sum((2 * leg * fits["pair"] for leg in pair_legs), zero)
            out["desired"] += (desired_full + hedged_full).sum(axis=1)
            out["refused_hedged"] += (hedged_full - hedged_open).sum(axis=1)
            out["desired_directional"] += desired_full.sum(axis=1)
            out["refused"] += refused.sum(axis=1)
            if t < pricing.WEEK:
                out["desired_week1"] += desired_full.sum(axis=1)
                out["refused_week1"] += refused.sum(axis=1)

            # Book accepted cohorts and pairs and schedule their settlement.
            entry_usd = (entry * fx)[..., None]
            need = np.zeros(lanes_shape)
            for (cls, side), want, unit in zip(cohorts, wants, units):
                amount = (long_ok if side > 0 else short_ok) * want
                if side > 0:
                    longs += amount / entry_usd
                else:
                    shorts += amount / entry_usd
                cost += side * amount / fx[..., None]
                need += book(unit, k, amount, 1.0 / prep.leverage[0, cls.bucket])
            pair_margin = zero.copy()
            for cls, unit, leg in zip(hedged, pairs, pair_legs):
                leg = leg * fits["pair"]
                longs += leg / entry_usd
                shorts += leg / entry_usd
                need += book(unit, k, leg, 2.0 / prep.leverage[1, cls.bucket])
                pair_margin += 2 * leg / hedged_leverage[cls.bucket]
            if gaps is not None:
                # The stress: a share of the past week's margin accepted in hedged pairs, at the
                # maximum leverage, held through the closure that starts now, in orders that fit.
                start = gaps[3][:, k][..., None]
                wanted_leg = np.where(start, scenario.gap_pair_share * hedged_week / 2 * markets.max_leverage[None, :, None], 0.0)
                leg = wanted_leg * fits["pair"]
                longs += leg / entry_usd
                shorts += leg / entry_usd
                need += book(gaps[:3], k, leg, 2.0 * markets.margin_rate)
                out["desired"] += 2 * wanted_leg.sum(axis=1)
                out["refused_hedged"] += 2 * (wanted_leg - leg).sum(axis=1)
            hedged_week = hedged_week * week_decay + pair_margin

            # Churn pays the profit fee on its expected gain over its hold, and its mean open net
            # notional takes the hour's move from the open, a reopening gap excluded, converted to
            # USD at the FX index; churners fund what they owe. A hedged churn class is a pair in
            # two accounts (as `pair_units` settles it): each account pays the fee on its own
            # positive realised PnL, so per pair the expectation is 10% of both legs' expected
            # gains — 0.2 * N * gain for leg N, symmetric R — the same identity `pair_exit`
            # charges ex post (`execution.rs`, `profit_fee`).
            churn_gain = gain_long * long_ok + gain_short * short_ok + gain_hedged
            churn_fee = accounting.profit_fee(churn_gain)
            fx_ratio = paths.fx[:, t] / paths.fx[:, t - 1] if t >= 1 else 1.0
            hour_ret = accounting.realised_pnl(1, paths.close[:, t] / paths.open[:, t], fx_ratio)
            churn_pnl = (open_long * long_ok - open_short * short_ok) * hour_ret[..., None]
            churn_cash = churn_fee - churn_pnl  # to the house
            house += churn_cash
            totals[:, :, FEE] += churn_fee
            totals[:, :, MINT] += churn_gain

            # Vault: exit proceeds to idle balances, new margin and churn's dues drawn from them
            # first and the rest deposited, then withdrawals of what is left, and the queue.
            owed = churn_cash.sum(axis=1)
            idle += proceeds - np.minimum(owed, 0.0)
            need += np.maximum(owed, 0.0)
            draw = np.minimum(idle, need)
            idle -= draw
            vault += need - draw
            in_positions += need - np.maximum(owed, 0.0) - proceeds - exit_cash.sum(axis=1)
            out["deposits"] += need - draw
            from_exits = scenario.withdraw_on_exit * np.minimum(np.maximum(proceeds, 0.0), idle)
            rate = np.where(nav < 0, run_rate, calm_rate)
            request = from_exits + rate * (idle - from_exits)
            idle -= request
            paid = np.where(queue > 0, 0.0, np.minimum(vault, request))
            queue += request - paid
            vault -= paid
            settle_queue = np.minimum(queue, vault)
            queue -= settle_queue
            vault -= settle_queue
            out["withdrawn"] += paid + settle_queue
            out["queued"] |= queue > 1e-6
            out["max_queue"] = np.maximum(out["max_queue"], queue)
            identity_gap = max(identity_gap, float(np.abs(vault - (idle + in_positions + queue + house.sum(axis=1))).max()))

            exposure = (entry * fx)[..., None] * (longs - shorts)
            nav = (house - (exposure - fx[..., None] * cost)).sum(axis=1)
            newly = (nav < 0) & ~out["ever_negative"]
            out["first_negative_h"] = np.where(newly, t, out["first_negative_h"])
            out["ever_negative"] |= nav < 0
            started |= (np.abs(house).sum(axis=1) > 0) | (np.abs(exposure).sum(axis=1) > 0)
            out["min_nav"] = np.minimum(out["min_nav"], np.where(started, nav, np.inf))
            out["min_cash"] = np.minimum(out["min_cash"], np.where(started, house.sum(axis=1), np.inf))
            out["max_gross_exposure"] = np.maximum(out["max_gross_exposure"], np.abs(exposure).sum(axis=1))
            if t % 24 == 23:
                out["daily_nav"][:, :, t // 24] = nav

    for name, channel in (("fee", FEE), ("financing", FINANCING), ("bad_debt", BAD_DEBT), ("mint_basis", MINT)):
        out[name] = totals[:, :, channel].sum(axis=1)
    out["nav_end"] = nav
    out["pnl_by_market"] = (house - (exposure - fx[..., None] * cost)).transpose(0, 2, 1)
    out["refused_by_market"] = refused_by_market.transpose(0, 2, 1)
    out["desired_by_market"] = np.broadcast_to(desired_by_market[:, None, :], (n_paths, n_lanes, n_markets)).copy()
    out["in_positions_end"] = in_positions
    out["identity_gap"] = np.array(identity_gap)
    if scenario.utilisation_stats:
        out["util_hist"] = util_hist
        out["util_expo"] = util_expo
    return out


def run_many(tasks: list[tuple], workers: int = 1) -> list[dict]:
    """Runs several (scenario, markets, bars, flow, floors, total_floors) tasks, splitting each
    into batches of paths and running every batch in one pool of `workers` processes."""
    jobs, owner = [], []
    for n, (scenario, markets, bars, flow, floors, total_floors) in enumerate(tasks):
        for i, start in enumerate(range(0, scenario.paths, scenario.batch_paths)):
            size = min(scenario.batch_paths, scenario.paths - start)
            jobs.append((scenario, markets, bars, flow, floors, total_floors, scenario.seed * 100_003 + i, size))
            owner.append(n)
    if workers > 1:
        from concurrent.futures import ProcessPoolExecutor

        with ProcessPoolExecutor(max_workers=workers) as pool:
            parts = list(pool.map(_run_job, jobs))
    else:
        parts = [_run_job(job) for job in jobs]
    results = []
    for n in range(len(tasks)):
        mine = [part for part, o in zip(parts, owner) if o == n]
        merged = {}
        for key in mine[0]:
            if key == "identity_gap":
                merged[key] = max(float(p[key]) for p in mine)
            else:
                merged[key] = np.concatenate([p[key] for p in mine], axis=0)
        results.append(merged)
    return results


def _run_job(job):
    return run_batch(*job)
