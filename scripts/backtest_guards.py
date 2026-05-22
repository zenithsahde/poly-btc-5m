#!/usr/bin/env python3
"""
Backtest different chase guards against historical fv_snapshots.

Replays signal.rs chase + fill + merge + window-settle logic exactly,
then swaps in alternative guard rules to compare PnL.

Author: PUA-engine backtester (data-driven, no hand-waving).
"""
from __future__ import annotations

import sys
import glob
import argparse
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path

import pandas as pd
import numpy as np

# ------------------------------------------------------------------
# Constants — mirror signal.rs exactly
# ------------------------------------------------------------------
CHASE_GAP_MIN = 0.03
FV_HISTORY_LEN = 10
MAX_BUY_PRICE = 0.85
MIN_EXPIRY_MIN_FOR_CHASE = 0.5
MERGE_AVG_SUM_MAX = 1.0
FORCE_MERGE_EXPIRY_MIN = 1.0
MERGE_INTERVAL_MS = 1000
TAKER_BUY_INTERVAL_MS = 1000
MAKER_BUY_CHASE_MIN_QTY = 100.0
MERGE_TRIGGER_PAIR_QTY = 1.0
TAKER_FEE_RATE = 0.072  # qty * 0.072 * p * (1-p)


@dataclass
class Position:
    qty: float = 0.0
    avg: float = 0.0

    def buy(self, price: float, qty: float):
        new_qty = self.qty + qty
        if new_qty > 0:
            self.avg = (self.qty * self.avg + price * qty) / new_qty
        self.qty = new_qty


@dataclass
class State:
    pos_up: Position = field(default_factory=Position)
    pos_down: Position = field(default_factory=Position)
    cash_paid: float = 0.0
    cash_received: float = 0.0
    total_fee: float = 0.0
    merge_pnl: float = 0.0
    merged_pairs: float = 0.0
    last_taker_up_ts: int = 0
    last_taker_down_ts: int = 0
    last_merge_ts: int = 0
    fv_up_hist: deque = field(default_factory=lambda: deque(maxlen=FV_HISTORY_LEN))
    fv_down_hist: deque = field(default_factory=lambda: deque(maxlen=FV_HISTORY_LEN))

    # Stats
    chase_up_count: int = 0
    chase_down_count: int = 0
    rejected_up: int = 0
    rejected_down: int = 0
    max_qty_up: float = 0.0
    max_qty_down: float = 0.0

    def avg_sum(self) -> float:
        return self.pos_up.avg + self.pos_down.avg

    def mergeable(self) -> float:
        return min(self.pos_up.qty, self.pos_down.qty)

    def inventory_value(self, up_mid: float, down_mid: float) -> float:
        pairs = self.mergeable()
        extra_up = max(self.pos_up.qty - pairs, 0.0)
        extra_down = max(self.pos_down.qty - pairs, 0.0)
        return pairs * 1.00 + extra_up * up_mid + extra_down * down_mid

    def net_pnl(self, up_mid: float, down_mid: float) -> float:
        return self.cash_received + self.inventory_value(up_mid, down_mid) - self.cash_paid - self.total_fee

    def apply_fill(self, side: str, price: float, qty: float, ts_ms: int):
        p = max(0.0, min(1.0, price))
        fee = qty * TAKER_FEE_RATE * p * (1 - p)
        self.total_fee += fee
        self.cash_paid += price * qty
        if side == "UP":
            self.pos_up.buy(price, qty)
            self.last_taker_up_ts = ts_ms
            self.chase_up_count += 1
        else:
            self.pos_down.buy(price, qty)
            self.last_taker_down_ts = ts_ms
            self.chase_down_count += 1
        self.max_qty_up = max(self.max_qty_up, self.pos_up.qty)
        self.max_qty_down = max(self.max_qty_down, self.pos_down.qty)

    def apply_merge(self, pairs: float, ts_ms: int, force: bool = False):
        pq = min(pairs, self.mergeable())
        if pq <= 0:
            return
        avg_sum = self.avg_sum()
        if not force and avg_sum >= MERGE_AVG_SUM_MAX:
            return
        delta = pq * (1.00 - avg_sum)
        self.merge_pnl += delta
        self.cash_received += pq * 1.00
        self.merged_pairs += pq
        self.pos_up.qty -= pq
        self.pos_down.qty -= pq
        if self.pos_up.qty <= 0:
            self.pos_up.avg = 0
            self.pos_up.qty = 0
        if self.pos_down.qty <= 0:
            self.pos_down.avg = 0
            self.pos_down.qty = 0
        self.last_merge_ts = ts_ms

    def settle_window(self, binance_close: float, strike: float, ts_ms: int):
        # 1. Force-merge all pairs (math equivalent to redeem)
        pairs = self.mergeable()
        if pairs > 0:
            avg_sum = self.avg_sum()
            self.merge_pnl += pairs * (1.0 - avg_sum)
            self.cash_received += pairs * 1.0
            self.merged_pairs += pairs
            self.pos_up.qty -= pairs
            self.pos_down.qty -= pairs
            self.last_merge_ts = ts_ms
        # 2. Redeem winner side
        if strike > 0 and binance_close > 0:
            up_wins = binance_close >= strike
            if up_wins and self.pos_up.qty > 0:
                self.cash_received += self.pos_up.qty * 1.0
            elif not up_wins and self.pos_down.qty > 0:
                self.cash_received += self.pos_down.qty * 1.0
        # 3. Reset positions for new window
        self.pos_up = Position()
        self.pos_down = Position()


def fv_rising(hist: deque, current: float) -> bool:
    if len(hist) < 3:
        return False
    return current > sum(hist) / len(hist)


# ------------------------------------------------------------------
# Guard schemes
# ------------------------------------------------------------------
def make_guard(scheme: str, **params):
    """Return guard(state, side, price, up_ask, down_ask) -> bool (allow?)"""

    def guard_baseline(s: State, side: str, price: float, up_ask: float, down_ask: float) -> bool:
        return 0 < price <= MAX_BUY_PRICE

    def guard_A_symmetric(s: State, side: str, price: float, up_ask: float, down_ask: float) -> bool:
        # 0.15 <= price <= 0.85
        lo = params.get("lo", 0.15)
        hi = params.get("hi", 0.85)
        return lo <= price <= hi

    def guard_B_arb_window(s: State, side: str, price: float, up_ask: float, down_ask: float) -> bool:
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        if up_ask <= 0 or down_ask <= 0:
            return False
        # Allow tolerance over 1.0 (Poly has ~1c spread, so ask_sum ≈ 1.01 normally)
        thresh = params.get("arb_thresh", 1.02)
        return (up_ask + down_ask) < thresh

    def guard_C_avg_sum(s: State, side: str, price: float, up_ask: float, down_ask: float) -> bool:
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        # avg_sum after this fill of MAKER_BUY_CHASE_MIN_QTY @ price
        qty = MAKER_BUY_CHASE_MIN_QTY
        if side == "UP":
            new_q = s.pos_up.qty + qty
            new_avg_up = (s.pos_up.qty * s.pos_up.avg + price * qty) / new_q if new_q > 0 else price
            new_sum = new_avg_up + s.pos_down.avg
        else:
            new_q = s.pos_down.qty + qty
            new_avg_down = (s.pos_down.qty * s.pos_down.avg + price * qty) / new_q if new_q > 0 else price
            new_sum = s.pos_up.avg + new_avg_down
        return new_sum < params.get("max_avg_sum", 1.0)

    def guard_C2_cost_aware(s: State, side: str, price: float, up_ask: float, down_ask: float) -> bool:
        """avg_sum after fill, using opposite-side ASK as proxy when no inventory.
        Closes the C-failure mode where empty side makes avg_sum trivially low."""
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        qty = MAKER_BUY_CHASE_MIN_QTY
        if side == "UP":
            new_q = s.pos_up.qty + qty
            new_avg_up = (s.pos_up.qty * s.pos_up.avg + price * qty) / new_q if new_q > 0 else price
            opp = s.pos_down.avg if s.pos_down.qty > 0 else (down_ask if down_ask > 0 else 0.5)
            new_sum = new_avg_up + opp
        else:
            new_q = s.pos_down.qty + qty
            new_avg_down = (s.pos_down.qty * s.pos_down.avg + price * qty) / new_q if new_q > 0 else price
            opp = s.pos_up.avg if s.pos_up.qty > 0 else (up_ask if up_ask > 0 else 0.5)
            new_sum = opp + new_avg_down
        return new_sum < params.get("max_avg_sum", 1.02)

    def guard_D_qty_cap(s: State, side: str, price: float, up_ask: float, down_ask: float) -> bool:
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        cap = params.get("cap", 500.0)
        if side == "UP":
            return s.pos_up.qty < cap
        else:
            return s.pos_down.qty < cap

    def guard_E_force_balance(s: State, side: str, price: float, up_ask: float, down_ask: float) -> bool:
        # Only allow buying the side with smaller qty (or equal)
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        if side == "UP":
            return s.pos_up.qty <= s.pos_down.qty + params.get("slack", 100.0)
        else:
            return s.pos_down.qty <= s.pos_up.qty + params.get("slack", 100.0)

    def guard_BD(s: State, side: str, price: float, up_ask: float, down_ask: float) -> bool:
        return guard_B_arb_window(s, side, price, up_ask, down_ask) and guard_D_qty_cap(s, side, price, up_ask, down_ask)

    def guard_BC(s: State, side: str, price: float, up_ask: float, down_ask: float) -> bool:
        return guard_B_arb_window(s, side, price, up_ask, down_ask) and guard_C_avg_sum(s, side, price, up_ask, down_ask)

    def guard_BE(s: State, side: str, price: float, up_ask: float, down_ask: float) -> bool:
        return guard_B_arb_window(s, side, price, up_ask, down_ask) and guard_E_force_balance(s, side, price, up_ask, down_ask)

    def guard_C2D(s, side, price, up_ask, down_ask):
        return guard_C2_cost_aware(s, side, price, up_ask, down_ask) and guard_D_qty_cap(s, side, price, up_ask, down_ask)

    def guard_C2E(s, side, price, up_ask, down_ask):
        return guard_C2_cost_aware(s, side, price, up_ask, down_ask) and guard_E_force_balance(s, side, price, up_ask, down_ask)

    schemes = {
        "baseline": guard_baseline,
        "A": guard_A_symmetric,
        "B": guard_B_arb_window,
        "C": guard_C_avg_sum,
        "C2": guard_C2_cost_aware,
        "D": guard_D_qty_cap,
        "E": guard_E_force_balance,
        "BD": guard_BD,
        "BC": guard_BC,
        "BE": guard_BE,
        "C2D": guard_C2D,
        "C2E": guard_C2E,
    }
    return schemes[scheme]


# ------------------------------------------------------------------
# Backtest engine
# ------------------------------------------------------------------
def backtest(df: pd.DataFrame, scheme: str, **params):
    s = State()
    guard = make_guard(scheme, **params)
    pnl_curve = []
    last_window = None
    last_binance = 0.0
    last_strike = 0.0

    for row in df.itertuples():
        ts = int(row.t_ms)
        win_end = int(row.window_end_ts) if hasattr(row, 'window_end_ts') else 0
        binance_mid = float(row.binance_mid)
        fv_up = float(row.fv_up)
        fv_down = float(row.fv_down)
        up_bid = float(row.poly_up_bid)
        up_ask = float(row.poly_up_ask)
        down_bid = float(row.poly_down_bid)
        down_ask = float(row.poly_down_ask)
        market_state = row.market_state  # 'S' or 'E'
        expiry_min = float(row.expiry_min)
        strike = float(row.strike)
        source = row.source  # 'B' or 'P'

        # Window switch: settle previous window
        if last_window is not None and win_end != last_window:
            s.settle_window(last_binance, last_strike, ts)
        last_window = win_end
        last_binance = binance_mid
        last_strike = strike

        in_excited = (market_state == "E")
        in_chase_window = expiry_min >= MIN_EXPIRY_MIN_FOR_CHASE

        # Update FV history (only on Binance source — same as signal.rs)
        if source == "B":
            up_mid_p = (up_bid + up_ask) / 2 if (up_bid > 0 and up_ask > 0) else 0.0
            down_mid_p = (down_bid + down_ask) / 2 if (down_bid > 0 and down_ask > 0) else 0.0

            up_chase = (in_excited and in_chase_window and up_mid_p > 0
                       and (fv_up - up_mid_p) >= CHASE_GAP_MIN
                       and fv_rising(s.fv_up_hist, fv_up))
            down_chase = (in_excited and in_chase_window and down_mid_p > 0
                         and (fv_down - down_mid_p) >= CHASE_GAP_MIN
                         and fv_rising(s.fv_down_hist, fv_down))

            taker_price_up = up_ask
            taker_price_down = down_ask

            if up_chase and ts - s.last_taker_up_ts >= TAKER_BUY_INTERVAL_MS:
                if guard(s, "UP", taker_price_up, up_ask, down_ask):
                    s.apply_fill("UP", taker_price_up, MAKER_BUY_CHASE_MIN_QTY, ts)
                else:
                    s.rejected_up += 1
            if down_chase and ts - s.last_taker_down_ts >= TAKER_BUY_INTERVAL_MS:
                if guard(s, "DOWN", taker_price_down, up_ask, down_ask):
                    s.apply_fill("DOWN", taker_price_down, MAKER_BUY_CHASE_MIN_QTY, ts)
                else:
                    s.rejected_down += 1

            s.fv_up_hist.append(fv_up)
            s.fv_down_hist.append(fv_down)

        # On Poly source: try merge
        if source == "P":
            mergeable = s.mergeable()
            throttle_ok = ts - s.last_merge_ts >= MERGE_INTERVAL_MS
            avg_sum_now = s.avg_sum()
            force_merge = expiry_min < FORCE_MERGE_EXPIRY_MIN
            avg_ok = force_merge or avg_sum_now < MERGE_AVG_SUM_MAX
            if mergeable >= MERGE_TRIGGER_PAIR_QTY and throttle_ok and avg_ok:
                pq = max(np.floor(mergeable), MERGE_TRIGGER_PAIR_QTY)
                s.apply_merge(pq, ts, force=force_merge)

        # Track PnL curve every 1000 rows
        if len(pnl_curve) == 0 or ts - pnl_curve[-1][0] > 5000:
            up_mid = (up_bid + up_ask) / 2 if (up_bid > 0 and up_ask > 0) else 0.0
            down_mid = (down_bid + down_ask) / 2 if (down_bid > 0 and down_ask > 0) else 0.0
            pnl_curve.append((ts, s.net_pnl(up_mid, down_mid)))

    # Final settle
    if last_window is not None:
        s.settle_window(last_binance, last_strike, last_window * 1000)

    final_pnl = s.cash_received - s.cash_paid - s.total_fee  # All positions settled
    pnl_curve.append((last_window * 1000 if last_window else 0, final_pnl))

    return {
        "scheme": scheme,
        "params": params,
        "final_pnl": final_pnl,
        "merge_pnl": s.merge_pnl,
        "merged_pairs": s.merged_pairs,
        "cash_paid": s.cash_paid,
        "cash_received": s.cash_received,
        "total_fee": s.total_fee,
        "chase_up": s.chase_up_count,
        "chase_down": s.chase_down_count,
        "rejected_up": s.rejected_up,
        "rejected_down": s.rejected_down,
        "max_qty_up": s.max_qty_up,
        "max_qty_down": s.max_qty_down,
        "min_pnl": min(p for _, p in pnl_curve) if pnl_curve else 0,
    }


def load_data(snapshot_dir: str) -> pd.DataFrame:
    """Load all fv_*.csv files, sort by t_ms, attach window_end_ts from filename."""
    files = sorted(glob.glob(f"{snapshot_dir}/fv_*.csv"))
    dfs = []
    for f in files:
        win = int(Path(f).stem.replace("fv_", ""))
        df = pd.read_csv(f)
        df["window_end_ts"] = win
        dfs.append(df)
    full = pd.concat(dfs, ignore_index=True).sort_values("t_ms").reset_index(drop=True)
    return full


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--data", default="/Users/zenith/btc-poly/market-maker-5m/fv_snapshots")
    parser.add_argument("--scheme", default="all")
    args = parser.parse_args()

    df = load_data(args.data)
    print(f"Loaded {len(df):,} snapshots across {df['window_end_ts'].nunique()} windows")
    print(f"Time range: {df['t_ms'].min()} → {df['t_ms'].max()}")
    print(f"BTC range: {df['binance_mid'].min():.2f} → {df['binance_mid'].max():.2f}")
    print()

    schemes_to_run = [
        ("baseline", {}),
        ("A", {"lo": 0.15, "hi": 0.85}),
        ("B", {"arb_thresh": 1.02}),  # 1c spread tolerance
        ("C", {"max_avg_sum": 1.0}),
        ("C2", {"max_avg_sum": 1.02}),
        ("D", {"cap": 500.0}),
        ("E", {"slack": 100.0}),
        ("BD", {"arb_thresh": 1.02, "cap": 500.0}),
        ("BC", {"arb_thresh": 1.02, "max_avg_sum": 1.0}),
        ("BE", {"arb_thresh": 1.02, "slack": 100.0}),
        ("C2D", {"max_avg_sum": 1.02, "cap": 500.0}),
        ("C2E", {"max_avg_sum": 1.02, "slack": 100.0}),
    ]

    results = []
    for scheme, params in schemes_to_run:
        r = backtest(df, scheme, **params)
        results.append(r)
        print(f"[{scheme:10s}] PnL=${r['final_pnl']:+8.2f}  "
              f"merge={r['merge_pnl']:+7.1f}  pairs={r['merged_pairs']:6.0f}  "
              f"chase_up={r['chase_up']:4d}  chase_down={r['chase_down']:4d}  "
              f"max_up={r['max_qty_up']:6.0f}  max_dn={r['max_qty_down']:6.0f}  "
              f"min_pnl=${r['min_pnl']:+7.0f}")

    print()
    best = max(results, key=lambda r: r["final_pnl"])
    print(f"🏆 Best: {best['scheme']} PnL=${best['final_pnl']:+.2f} params={best['params']}")


if __name__ == "__main__":
    main()
