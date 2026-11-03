"""Decode papertrade.xyz's Exchange logs into positions and derive the trader behaviour dataset.

The Exchange's source is not verified on the explorer, so the event layouts below are inferred
from the logs and checked: every close and liquidation must match an earlier open, a close's fee
must be 2% of its adjusted PnL (the documented win fee, and the LP-side loss fee), BTC entry
prices must sit on Binance's price of the same hour, and deposits less withdrawals give the
contract's USDC balance. The open-interest event is not used: its changes do not follow position
flows, and until block 48,217,087 it carried a fixed offset of about $5B a side.

Event layouts (topic 0 -> indexed topics; data words):
- OPEN: position id, user; asset (0 BTC, 1 ETH), is_long, flag, entry price, margin, leverage.
- CLOSE: position id, user; exit price, PnL after the deadband, PnL after the impact haircut,
  unused, fee. PnL words are signed.
- LIQUIDATION: position id, user; trigger price, bust price, fee, lost margin, mint basis.
- DEPOSIT: user, deposit proxy; amount (8 decimals). WITHDRAWAL: user; amount (18 decimals),
  amount (8 decimals); inferred from the 18-to-8-decimal conversion to HyperCore.
Prices are Hyperliquid's integer prices: 1 decimal for BTC, 2 for ETH. Money words have 18
decimals.

Outputs under `data/derived/`: `papertrade_flow.csv` (the flow profile the model samples),
`papertrade_sizes.csv` (per launch margin schedule, the margin-weighted distribution of each
position's notional, which sets the share of flow in orders small enough to fit a cap),
`papertrade_leverage.csv` (per launch margin schedule and leverage bin, the notional that margin
buys), `papertrade_daily.csv`, `papertrade_holding.csv`, and `papertrade_summary.json`. The flow,
size, and leverage profiles count the closed positions opened on the full days that set the daily
margin path.
"""

import csv
import gzip
import heapq
import json
from collections import defaultdict
from datetime import UTC, datetime
from pathlib import Path

import numpy as np

from catalogue import PLANNED_CRYPTO_TIERS, PREVIOUS_CRYPTO_TIERS, launch_markets

OPEN = "0x88451ea1189f73b554f0bf39c09868560ed149b3d788fcc933ff66ba926827f3"
CLOSE = "0x87902ed2451849f465fc4d36a01d7f5c67b53af12f63ee85d2814a2d582f89bf"
LIQUIDATION = "0x0c859bc5db091ba3b9dc10c00f8dfc78b1f0b7e2566d107253ae67c069ae5de8"
DEPOSIT = "0x02d61879cd72906e0951e3a96096407dd2b1f2cd2d063dae983bb20216cf3025"
WITHDRAWAL = "0x95d1a93901f24eeaa294588802f66fa008f9f3741060c9f79e5dab4709de5e75"
PRICE_SCALE = {0: 10.0, 1: 100.0}
WAD = 1e18
# Hold classes: upper bounds in hours, and the hold the model gives each persistent class.
HOLD_EDGES_H = [1.0, 2.0, 8.0, 32.0, np.inf]
HOLD_CLASS_H = [None, 1, 4, 16, 48]  # class 0 is churn: opened and closed within the hour
LEVERAGE_EDGES = [50, 300, 1001]  # papertrade leverage bins: <=50, 51-300, 301-1000
PAIR_WINDOW_BLOCKS = 2
SIZE_BOUNDS = np.geomspace(1.0, 1e9, 73)  # eight a decade
LOG_ORDER = 100_000  # orders logs as block * LOG_ORDER + log index


def signed(word: int) -> int:
    return word - (1 << 256) if word >> 255 else word


def words(data: str) -> list[int]:
    return [signed(int(data[i : i + 64], 16)) for i in range(2, len(data), 64)]


def read_logs(raw: Path):
    """Yields (block, tx_index, log_index, topics, data) in chain order, from the chunks inside
    the manifest's pulled range only."""
    manifest = json.loads((raw / "manifest.json").read_text())
    first, last = manifest["from_block"], manifest["last_block_pulled"]
    for path in sorted((raw / "logs").glob("*.jsonl.gz")):
        if not first <= int(path.name.split(".")[0]) <= last:
            continue
        with gzip.open(path, "rt") as fh:
            rows = [json.loads(line) for line in fh]
        rows.sort(key=lambda r: (r[0], r[1], r[2]))
        yield from rows


def block_clock(raw: Path):
    """Block -> UTC seconds, interpolated between the fetched anchors; a block outside them is
    an error rather than a clamped time."""
    with (raw / "block_times.csv").open() as fh:
        anchors = np.array([(int(r["block"]), int(r["timestamp"])) for r in csv.DictReader(fh)], dtype=float)

    def clock(blocks):
        blocks = np.asarray(blocks, dtype=float)
        if blocks.size and (blocks.min() < anchors[0, 0] or blocks.max() > anchors[-1, 0]):
            raise ValueError("block outside the timestamp anchors; re-run fetch-papertrade")
        return np.interp(blocks, anchors[:, 0], anchors[:, 1])

    return clock


def decode(raw: Path) -> dict:
    """Positions as column arrays, plus deposit and withdrawal events."""
    opens, ends = {}, {}
    deposits, withdrawals = [], []
    users: dict[str, int] = {}
    for block, _, log_index, topics, data in read_logs(raw):
        sig = topics[0]
        order = block * LOG_ORDER + log_index
        if sig == OPEN:
            asset, is_long, _, price, margin, leverage = words(data)
            user = users.setdefault(topics[2], len(users))
            opens[int(topics[1], 16)] = (block, user, asset, 1 if is_long else -1, price / PRICE_SCALE[asset], margin / WAD, leverage, order)
        elif sig == CLOSE:
            price, pnl_deadband, pnl_adjusted, _, fee = words(data)
            ends[int(topics[1], 16)] = (block, 0, price, pnl_deadband / WAD, pnl_adjusted / WAD, fee / WAD, order)
        elif sig == LIQUIDATION:
            trigger, _, fee, lost, _ = words(data)
            ends[int(topics[1], 16)] = (block, 1, trigger, -lost / WAD, -lost / WAD, fee / WAD, order)
        elif sig == DEPOSIT:
            deposits.append((block, words(data)[0] / 1e8))
        elif sig == WITHDRAWAL:
            withdrawals.append((block, words(data)[0] / WAD))

    ids = np.array(sorted(opens), dtype=np.int64)
    cols = list(zip(*(opens[i] for i in ids)))
    pos = {
        "id": ids,
        "open_block": np.array(cols[0], dtype=np.int64),
        "user": np.array(cols[1], dtype=np.int64),
        "asset": np.array(cols[2], dtype=np.int64),
        "side": np.array(cols[3], dtype=np.int64),
        "entry": np.array(cols[4]),
        "margin": np.array(cols[5]),
        "leverage": np.array(cols[6], dtype=np.int64),
        "open_order": np.array(cols[7], dtype=np.int64),
    }
    pos["notional"] = pos["margin"] * pos["leverage"]
    n = len(ids)
    end_cols = {k: np.full(n, np.nan) for k in ("close_block", "liquidated", "exit_raw", "pnl_deadband", "pnl_adjusted", "fee", "close_order")}
    index = {pid: i for i, pid in enumerate(ids)}
    orphans = 0
    for pid, end in ends.items():
        i = index.get(pid)
        if i is None:
            orphans += 1
            continue
        for k, v in zip(end_cols, end):
            end_cols[k][i] = v
    pos.update(end_cols)
    pos["exit"] = pos["exit_raw"] / np.vectorize(PRICE_SCALE.get)(pos["asset"])
    return {
        "positions": pos,
        "orphan_ends": orphans,
        "deposits": np.array(deposits),
        "withdrawals": np.array(withdrawals),
        "users": len(users),
    }


def price_check(pos: dict, open_t: np.ndarray, raw_markets: Path) -> dict:
    """Median gap between BTC entry prices and Binance's BTCUSDT close of the same hour."""
    rows = json.loads(gzip.decompress((raw_markets / "binance" / "BTCUSDT_1h_papertrade.json.gz").read_bytes()))
    close = {r[0] // 3_600_000: float(r[4]) for r in rows}
    btc = pos["asset"] == 0
    hours = (open_t[btc] // 3600).astype(np.int64)
    ref = np.array([close.get(h, np.nan) for h in hours])
    gap = np.abs(pos["entry"][btc] / ref - 1)
    return {"btc_entry_vs_binance_hour_close_median_gap": round(float(np.nanmedian(gap)), 5)}


def fee_check(pos: dict) -> dict:
    closes = (pos["liquidated"] == 0) & (pos["pnl_adjusted"] != 0)
    ratio = pos["fee"][closes] / np.abs(pos["pnl_adjusted"][closes])
    return {"closes_checked": int(closes.sum()), "fee_over_adjusted_pnl_median": round(float(np.median(ratio)), 5)}


def hedges(pos: dict) -> np.ndarray:
    """Each position's hedged share of its notional, both legs of each hedge counted:
    - within an account: the part of a new position that offsets the account's open opposite
      notional not already offset, and the same amount of those opposite positions, earliest
      first; when a position closes, what it offset is free to be offset again;
    - across accounts: opposite positions of two accounts at the same notional (to the cent) that
      opened and closed within `PAIR_WINDOW_BLOCKS` blocks of each other, neither hedged within its
      own account, are hedged in full."""
    n = len(pos["id"])
    matched = np.zeros(n)
    closed_at = np.where(np.isnan(pos["close_block"]), np.inf, pos["close_block"])

    # Same account: sweep each (user, asset) in log order. `free` holds each open position's
    # notional not offset by an open opposite position, by side in open order.
    order = np.lexsort((pos["open_order"], pos["asset"], pos["user"]))
    users, assets, sides = pos["user"].tolist(), pos["asset"].tolist(), pos["side"].tolist()
    close_order = np.where(np.isnan(pos["close_order"]), np.inf, pos["close_order"])
    opens, notionals, ends = pos["open_order"].tolist(), pos["notional"].tolist(), close_order.tolist()
    key, live, free, partners = None, [], {1: {}, -1: {}}, {}
    for i in order.tolist():
        if (users[i], assets[i]) != key:
            key, live, free, partners = (users[i], assets[i]), [], {1: {}, -1: {}}, {}
        while live and live[0][0] <= opens[i]:
            _, j = heapq.heappop(live)
            del free[sides[j]][j]
            for p, amount in partners.pop(j):
                if p in free[sides[p]]:  # still open
                    free[sides[p]][p] += amount
        left, partners[i] = notionals[i], []
        for j, available in free[-sides[i]].items():
            if left <= 0:
                break
            amount = min(left, available)
            if amount <= 0:
                continue
            free[-sides[i]][j] -= amount
            matched[i] += amount
            matched[j] += amount
            partners[i].append((j, amount))
            partners[j].append((i, amount))
            left -= amount
        free[sides[i]][i] = left
        heapq.heappush(live, (ends[i], i))
    share = np.minimum(matched / np.maximum(pos["notional"], 1e-12), 1.0)

    # Across accounts: same asset and notional (to the cent), opposite sides, matching open and close.
    groups: dict = defaultdict(list)
    for i in range(n):
        groups[(pos["asset"][i], round(pos["notional"][i], 2))].append(i)
    for members in groups.values():
        if len(members) < 2:
            continue
        longs = sorted((pos["open_block"][i], i) for i in members if pos["side"][i] > 0)
        shorts = sorted((pos["open_block"][i], i) for i in members if pos["side"][i] < 0)
        used = set()
        k = 0
        for ob, i in longs:
            while k < len(shorts) and shorts[k][0] < ob - PAIR_WINDOW_BLOCKS:
                k += 1
            for sb, j in shorts[k:]:
                if sb > ob + PAIR_WINDOW_BLOCKS:
                    break
                if j in used or pos["user"][j] == pos["user"][i] or matched[i] > 0 or matched[j] > 0:
                    continue
                if np.isfinite(closed_at[i]) and abs(closed_at[j] - closed_at[i]) <= PAIR_WINDOW_BLOCKS:
                    share[i] = share[j] = 1.0
                    used.add(j)
                    break
    return share


def netting(pos: dict, open_t: np.ndarray, close_t: np.ndarray) -> dict:
    """Notional-time netting: how much of the gross open notional is offset within an account,
    and across the whole market."""
    end = np.where(np.isnan(close_t), np.nanmax(close_t), close_t)

    def offset(keys: np.ndarray) -> tuple[float, float]:
        t = np.concatenate([open_t, end])
        key = np.concatenate([keys, keys])
        signed_notional = np.concatenate([pos["notional"], -pos["notional"]])
        is_long = np.concatenate([pos["side"], pos["side"]]) > 0
        order = np.lexsort((t, key))
        t, key, signed_notional, is_long = t[order], key[order], signed_notional[order], is_long[order]
        start = np.r_[True, key[1:] != key[:-1]]
        group = np.cumsum(start) - 1

        def running(x: np.ndarray) -> np.ndarray:
            total = np.cumsum(x)
            return total - (total - x)[start][group]

        longs = running(np.where(is_long, signed_notional, 0.0))
        shorts = running(np.where(is_long, 0.0, signed_notional))
        dt = np.r_[t[1:] - t[:-1], 0.0]
        dt[np.r_[start[1:], True]] = 0.0  # no time runs past a key's last event
        return float((2 * np.minimum(longs, shorts) * dt).sum()), float(((longs + shorts) * dt).sum())

    account_hedged, gross = offset(pos["user"] * 2 + pos["asset"])
    market_hedged, _ = offset(pos["asset"].copy())
    return {
        "gross_notional_time_usd_days": round(gross / 86400, 0),
        "offset_within_accounts_share": round(account_hedged / gross, 4),
        "offset_market_wide_share": round(market_hedged / gross, 4),
    }


def long_share_dynamics(pos: dict, open_t: np.ndarray, hedged: np.ndarray, sample: np.ndarray, hourly_close: dict) -> dict:
    """Hourly long share of new directional notional in BTC, over the `sample` positions: its
    mean, AR(1) coefficient, the residual spread, and its slope on the previous hour's return
    (negative = buying dips)."""
    sel = sample & (pos["asset"] == 0)
    hour = (open_t[sel] // 3600).astype(np.int64)
    h0 = hour.min()
    directional = pos["notional"][sel] * (1 - hedged[sel])
    longs = np.bincount(hour - h0, weights=directional * (pos["side"][sel] > 0))
    total = np.bincount(hour - h0, weights=directional)
    ok = total > 0
    share = np.where(ok, longs / np.where(ok, total, 1), np.nan)
    hours = np.arange(len(share)) + h0
    prev_ret = np.array([hourly_close.get(h - 1, np.nan) for h in hours])
    x, y, r = share[:-1], share[1:], prev_ret[1:]
    good = np.isfinite(x) & np.isfinite(y) & np.isfinite(r)
    design = np.column_stack([np.ones(good.sum()), x[good] - np.nanmean(share), r[good]])
    coef, *_ = np.linalg.lstsq(design, y[good], rcond=None)
    resid = y[good] - design @ coef
    return {
        "long_share_mean": round(float(np.average(share[ok], weights=total[ok])), 4),
        "long_share_ar1": round(float(coef[1]), 4),
        "long_share_resid_sd": round(float(resid.std()), 4),
        "long_share_slope_on_prev_hour_return": round(float(coef[2]), 3),
        "hours": int(good.sum()),
    }


def leverage_bin_label(b: int) -> str:
    return f"{([1] + [e + 1 for e in LEVERAGE_EDGES])[b]}-{LEVERAGE_EDGES[b] - (b == len(LEVERAGE_EDGES) - 1)}"


def kinds(hedged: np.ndarray):
    """Each kind with the share of every position's notional it takes."""
    return (("directional", 1.0 - hedged), ("hedged", hedged))


def flow_profile(pos: dict, hold_h: np.ndarray, hedged: np.ndarray, sample: np.ndarray) -> list[dict]:
    """Opened notional and margin of the `sample` positions by kind (directional or hedged), hold
    class, and leverage bin; a partly hedged position counts in both kinds by share. Means are
    notional-weighted."""
    hold_class = np.searchsorted(HOLD_EDGES_H, hold_h, side="right")
    lev_bin = np.searchsorted(LEVERAGE_EDGES, pos["leverage"], side="left")
    rows = []
    for kind, weight in kinds(hedged):
        for c, hold in enumerate(HOLD_CLASS_H):
            for b in range(len(LEVERAGE_EDGES)):
                sel = sample & (hold_class == c) & (lev_bin == b) & (weight > 0)
                if not sel.any():
                    continue
                w = pos["notional"][sel] * weight[sel]
                rows.append(
                    {
                        "kind": kind,
                        "hold_class_h": hold if hold is not None else 0,
                        "leverage_bin": leverage_bin_label(b),
                        "positions": round(float(weight[sel].sum()), 1),
                        "notional_usd": round(float(w.sum()), 2),
                        "margin_usd": round(float((pos["margin"][sel] * weight[sel]).sum()), 2),
                        "mean_leverage": round(float(np.average(pos["leverage"][sel], weights=w)), 1),
                        "mean_sqrt_hold_h": round(float(np.average(np.sqrt(hold_h[sel]), weights=w)), 5),
                        "mean_hold_h": round(float(np.average(hold_h[sel], weights=w)), 4),
                        "long_share": round(float(np.average(pos["side"][sel] > 0, weights=w)), 4),
                    }
                )
    return rows


def launch_schedules() -> list[list[dict]]:
    """The launch markets' distinct margin band schedules, and the planned crypto ones."""
    from model.flow import schedule_key

    markets = launch_markets()
    tiers = [m["margin_tiers"] for m in markets] + [PLANNED_CRYPTO_TIERS, PREVIOUS_CRYPTO_TIERS]
    return list({schedule_key(t): t for t in tiers}.values())


def mapped_notional(margin: np.ndarray, leverage: np.ndarray, tiers: list[dict], mapping: str) -> np.ndarray:
    """Each position's notional on a launch market: its leverage mapped onto the first tier's
    maximum, and no more than its margin buys under the market's bands."""
    from model.flow import banded_notional, mapped_leverage

    first = np.array(1e4 / tiers[0]["bps"])
    return np.minimum(margin * mapped_leverage(leverage, first, mapping), banded_notional(margin, tiers))


def size_profile(pos: dict, hedged: np.ndarray, sample: np.ndarray) -> list[dict]:
    """Per kind, leverage mapping, and launch margin schedule, the share of the `sample`
    positions' margin in positions whose notional on that schedule (`mapped_notional`) is at or
    below each bound."""
    from model.flow import schedule_key

    rows = []
    for kind, weight in kinds(hedged):
        margin, leverage = pos["margin"][sample], pos["leverage"][sample].astype(float)
        w = margin * weight[sample]
        if w.sum() <= 0:
            continue  # the kind has no weight in the sample: no profile rows to emit
        for mapping in ("capped", "relative"):
            for tiers in launch_schedules():
                notional = mapped_notional(margin, leverage, tiers, mapping)
                order = np.argsort(notional)
                cum = np.cumsum(w[order]) / w.sum()
                for bound in SIZE_BOUNDS:
                    share = cum[np.searchsorted(notional[order], bound, side="right") - 1] if notional.min() <= bound else 0.0
                    rows.append({"kind": kind, "mapping": mapping, "schedule": schedule_key(tiers), "notional_upto_usd": round(float(bound), 2), "margin_share": round(float(share), 6)})
    return rows


def leverage_profile(pos: dict, hedged: np.ndarray, sample: np.ndarray) -> list[dict]:
    """Per kind, leverage mapping, launch margin schedule, and papertrade leverage bin, the
    `sample` positions' margin and their notional on that schedule (`mapped_notional`)."""
    from model.flow import schedule_key

    lev_bin = np.searchsorted(LEVERAGE_EDGES, pos["leverage"], side="left")
    rows = []
    for kind, weight in kinds(hedged):
        for mapping in ("capped", "relative"):
            for tiers in launch_schedules():
                for b in range(len(LEVERAGE_EDGES)):
                    sel = sample & (lev_bin == b) & (weight > 0)
                    if not sel.any():
                        continue
                    margin = pos["margin"][sel]
                    notional = mapped_notional(margin, pos["leverage"][sel].astype(float), tiers, mapping)
                    rows.append(
                        {
                            "kind": kind,
                            "mapping": mapping,
                            "schedule": schedule_key(tiers),
                            "leverage_bin": leverage_bin_label(b),
                            "margin_usd": round(float((margin * weight[sel]).sum()), 2),
                            "notional_usd": round(float((notional * weight[sel]).sum()), 2),
                        }
                    )
    return rows


def weighted_quantiles(values: np.ndarray, weights: np.ndarray, qs: list[float]) -> list[float]:
    order = np.argsort(values)
    cum = np.cumsum(weights[order]) / weights.sum()
    return [float(values[order][np.searchsorted(cum, q)]) for q in qs]


def write_csv(path: Path, rows: list[dict]) -> None:
    with path.open("w", newline="") as fh:
        writer = csv.DictWriter(fh, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


def binance_hourly_returns(raw_markets: Path) -> dict:
    rows = json.loads(gzip.decompress((raw_markets / "binance" / "BTCUSDT_1h_papertrade.json.gz").read_bytes()))
    close = {r[0] // 3_600_000: float(r[4]) for r in rows}
    return {h: np.log(close[h] / close[h - 1]) for h in close if h - 1 in close}


def derive(raw: Path, raw_markets: Path, out: Path) -> None:
    d = decode(raw)
    pos = d["positions"]
    clock = block_clock(raw)
    open_t = clock(pos["open_block"])
    closed = ~np.isnan(pos["close_block"])
    close_t = np.full(len(open_t), np.nan)
    close_t[closed] = clock(pos["close_block"][closed])
    hold_s = close_t - open_t
    hold_h = np.where(closed, hold_s / 3600, np.nan)
    hedged = hedges(pos)
    # The flow profiles come from the full days that also set the daily margin path.
    first_t, last_t = open_t.min(), np.nanmax(close_t)
    day = (open_t // 86400).astype(np.int64)
    full_day = (day * 86400 >= first_t) & ((day + 1) * 86400 <= last_t)
    sample = closed & full_day

    exit_ok = closed & (pos["liquidated"] == 0)
    raw_pnl = np.where(closed, pos["side"] * (pos["exit"] / pos["entry"] - 1) * pos["notional"], 0.0)
    raw_pnl = np.where(pos["liquidated"] == 1, -pos["margin"], raw_pnl)
    kept = np.where(exit_ok & (pos["pnl_adjusted"] > 0), pos["pnl_adjusted"] - pos["fee"], 0.0)
    gains = np.where(exit_ok & (raw_pnl > 0), raw_pnl, 0.0)
    losses = np.where(closed & (raw_pnl < 0), -raw_pnl, 0.0)

    w = pos["notional"][closed]
    exposure_w = w * hold_h[closed]
    summary = {
        "source": "papertrade.xyz Exchange 0x6cd5661646289fb6e65ea5c032310fded797d0a2 on HyperEVM (chain 999)",
        "blocks": [int(pos["open_block"].min()), int(np.nanmax(pos["close_block"]))],
        "from": datetime.fromtimestamp(open_t.min(), UTC).isoformat(timespec="seconds"),
        "to": datetime.fromtimestamp(np.nanmax(close_t), UTC).isoformat(timespec="seconds"),
        "days": round(float((np.nanmax(close_t) - open_t.min()) / 86400), 2),
        "accounts": d["users"],
        "positions": int(len(pos["id"])),
        "closed": int(closed.sum()),
        "liquidated_share": round(float((pos["liquidated"] == 1).sum() / closed.sum()), 4),
        "unmatched_closes": d["orphan_ends"],
        "opened_notional_usd": round(float(pos["notional"].sum()), 0),
        "btc_share_of_notional": round(float(pos["notional"][pos["asset"] == 0].sum() / pos["notional"].sum()), 4),
        "hedged_share_of_notional": round(float((pos["notional"] * hedged).sum() / pos["notional"].sum()), 4),
        "hedged_share_of_exposure_time": round(float((exposure_w * hedged[closed]).sum() / exposure_w.sum()), 4),
        "full_days_positions": int(sample.sum()),
        "full_days_hedged_share_of_notional": round(float((pos["notional"] * hedged)[sample].sum() / pos["notional"][sample].sum()), 4),
        "full_days_hedged_share_of_margin": round(float((pos["margin"] * hedged)[sample].sum() / pos["margin"][sample].sum()), 4),
        "hold_hours_quantiles_by_count": dict(zip(["p10", "p50", "p90", "p99"], np.round(np.quantile(hold_h[closed], [0.1, 0.5, 0.9, 0.99]), 5).tolist())),
        "exposure_time_share_held_over_1h": round(float(exposure_w[hold_h[closed] >= 1].sum() / exposure_w.sum()), 4),
        "exposure_time_share_held_over_24h": round(float(exposure_w[hold_h[closed] >= 24].sum() / exposure_w.sum()), 4),
        "leverage_quantiles_by_notional": dict(zip(["p10", "p50", "p90"], weighted_quantiles(pos["leverage"], pos["notional"], [0.1, 0.5, 0.9]))),
        "notional_quantiles_usd": dict(zip(["p50", "p90", "p99", "p999"], np.round(np.quantile(pos["notional"], [0.5, 0.9, 0.99, 0.999]), 0).tolist())),
        "raw_pnl_over_notional_bps": round(float(raw_pnl[closed].sum() / w.sum() * 1e4), 4),
        "raw_pnl_over_notional_bps_directional": round(float((raw_pnl * (1 - hedged))[closed].sum() / (pos["notional"] * (1 - hedged))[closed].sum() * 1e4), 4),
        "winner_keeps_share_of_raw_gain": round(float(kept.sum() / gains.sum()), 4),
        "hedged_cost_per_usd_of_loss": round(float(((gains - kept) * hedged).sum() / (losses * hedged).sum()), 4) if (losses * hedged).sum() > 0 else 0.0,
        "deposits_usd": round(float(d["deposits"][:, 1].sum()), 0) if len(d["deposits"]) else 0.0,
        "withdrawals_usd": round(float(d["withdrawals"][:, 1].sum()), 0) if len(d["withdrawals"]) else 0.0,
    }
    summary.update(netting(pos, open_t, close_t))
    summary.update(long_share_dynamics(pos, open_t, hedged, full_day, binance_hourly_returns(raw_markets)))
    summary.update(price_check(pos, open_t, raw_markets))
    summary["net_deposits_usd"] = round(summary["deposits_usd"] - summary["withdrawals_usd"], 0)
    summary.update(fee_check(pos))

    out.mkdir(parents=True, exist_ok=True)
    write_csv(out / "papertrade_flow.csv", flow_profile(pos, hold_h, hedged, sample))
    write_csv(out / "papertrade_sizes.csv", size_profile(pos, hedged, sample))
    write_csv(out / "papertrade_leverage.csv", leverage_profile(pos, hedged, sample))

    dep_day = (clock(d["deposits"][:, 0]) // 86400).astype(np.int64) if len(d["deposits"]) else np.array([], dtype=np.int64)
    wd_day = (clock(d["withdrawals"][:, 0]) // 86400).astype(np.int64) if len(d["withdrawals"]) else np.array([], dtype=np.int64)
    first_seen = {}
    for u, dd in zip(pos["user"][np.argsort(open_t)], np.sort(day)):
        first_seen.setdefault(u, dd)
    new_by_day = np.bincount(np.array(list(first_seen.values())) - day.min())
    daily = []
    for k, dd in enumerate(range(day.min(), day.max() + 1)):
        sel = day == dd
        daily.append(
            {
                "date": datetime.fromtimestamp(dd * 86400, UTC).date().isoformat(),
                "full_day": bool(dd * 86400 >= first_t and (dd + 1) * 86400 <= last_t),
                "positions": int(sel.sum()),
                "opened_notional_usd": round(float(pos["notional"][sel].sum()), 0),
                "opened_margin_usd": round(float(pos["margin"][sel].sum()), 0),
                "active_accounts": int(len(np.unique(pos["user"][sel]))),
                "new_accounts": int(new_by_day[k]) if k < len(new_by_day) else 0,
                "hedged_share": round(float((pos["notional"] * hedged)[sel].sum() / max(pos["notional"][sel].sum(), 1)), 4),
                "long_share_directional": round(float((pos["notional"] * (1 - hedged))[sel & (pos["side"] > 0)].sum() / max((pos["notional"] * (1 - hedged))[sel].sum(), 1)), 4),
                "liquidations": int((sel & (pos["liquidated"] == 1)).sum()),
                "raw_trader_pnl_usd": round(float(raw_pnl[sel].sum()), 0),
                "deposits_usd": round(float(d["deposits"][dep_day == dd, 1].sum()), 0) if len(dep_day) else 0.0,
                "withdrawals_usd": round(float(d["withdrawals"][wd_day == dd, 1].sum()), 0) if len(wd_day) else 0.0,
            }
        )
    write_csv(out / "papertrade_daily.csv", daily)

    edges = np.array([0, 10, 30, 60, 300, 900, 3600, 4 * 3600, 16 * 3600, 86400, 4 * 86400, np.inf])
    bins = np.searchsorted(edges, hold_s[closed], side="right") - 1
    holding = []
    for b in range(len(edges) - 1):
        sel = bins == b
        holding.append(
            {
                "hold_from_s": edges[b],
                "hold_to_s": edges[b + 1],
                "count_share": round(float(sel.mean()), 5),
                "notional_share": round(float(w[sel].sum() / w.sum()), 5),
                "exposure_time_share": round(float(exposure_w[sel].sum() / exposure_w.sum()), 5),
            }
        )
    write_csv(out / "papertrade_holding.csv", holding)
    (out / "papertrade_summary.json").write_text(json.dumps(summary, indent=1))
    print(json.dumps(summary, indent=1))
