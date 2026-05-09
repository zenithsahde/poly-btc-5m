#!/usr/bin/env python3
"""Sweep CHASE_GAP_MIN to find fee-optimal threshold."""
import sys
sys.path.insert(0, '/Users/zenith/btc-poly/market-maker-5m/scripts')
from collections import deque
import numpy as np
import pandas as pd
from backtest_guards import (State, MAKER_BUY_CHASE_MIN_QTY, MAX_BUY_PRICE,
                              MIN_EXPIRY_MIN_FOR_CHASE, MERGE_AVG_SUM_MAX,
                              FORCE_MERGE_EXPIRY_MIN, MERGE_INTERVAL_MS,
                              TAKER_BUY_INTERVAL_MS, MERGE_TRIGGER_PAIR_QTY,
                              FV_HISTORY_LEN, load_data)

DATA_DIR = sys.argv[1] if len(sys.argv) > 1 else "/tmp/aerlan_live"
df = load_data(DATA_DIR)
print(f"Loaded {len(df):,} snapshots\n")


def fv_rising(hist, current):
    if len(hist) < 3: return False
    return current > sum(hist) / len(hist)


def run(df, gap_min, throttle_ms=1000, e0=True):
    s = State()
    last_window = None
    last_binance = 0.0
    last_strike = 0.0

    def guard(side, price):
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        if e0:
            if side == "UP" and s.pos_up.qty > s.pos_down.qty:
                return False
            if side == "DOWN" and s.pos_down.qty > s.pos_up.qty:
                return False
        return True

    for row in df.itertuples():
        ts = int(row.t_ms)
        win_end = int(row.window_end_ts)
        binance_mid = float(row.binance_mid)
        fv_up = float(row.fv_up); fv_down = float(row.fv_down)
        up_bid = float(row.poly_up_bid); up_ask = float(row.poly_up_ask)
        down_bid = float(row.poly_down_bid); down_ask = float(row.poly_down_ask)
        market_state = row.market_state
        expiry_min = float(row.expiry_min)
        strike = float(row.strike)
        source = row.source

        if last_window is not None and win_end != last_window:
            s.settle_window(last_binance, last_strike, ts)
        last_window = win_end; last_binance = binance_mid; last_strike = strike

        in_excited = (market_state == "E")
        in_chase = expiry_min >= MIN_EXPIRY_MIN_FOR_CHASE

        if source == "B":
            up_mid = (up_bid+up_ask)/2 if (up_bid>0 and up_ask>0) else 0.0
            down_mid = (down_bid+down_ask)/2 if (down_bid>0 and down_ask>0) else 0.0
            up_chase = (in_excited and in_chase and up_mid>0
                       and (fv_up - up_mid) >= gap_min
                       and fv_rising(s.fv_up_hist, fv_up))
            down_chase = (in_excited and in_chase and down_mid>0
                         and (fv_down - down_mid) >= gap_min
                         and fv_rising(s.fv_down_hist, fv_down))
            if up_chase and ts - s.last_taker_up_ts >= throttle_ms:
                if guard("UP", up_ask):
                    s.apply_fill("UP", up_ask, MAKER_BUY_CHASE_MIN_QTY, ts)
            if down_chase and ts - s.last_taker_down_ts >= throttle_ms:
                if guard("DOWN", down_ask):
                    s.apply_fill("DOWN", down_ask, MAKER_BUY_CHASE_MIN_QTY, ts)
            s.fv_up_hist.append(fv_up); s.fv_down_hist.append(fv_down)

        if source == "P":
            mergeable = s.mergeable()
            throttle_ok = ts - s.last_merge_ts >= MERGE_INTERVAL_MS
            avg_sum_now = s.avg_sum()
            force_merge = expiry_min < FORCE_MERGE_EXPIRY_MIN
            avg_ok = force_merge or avg_sum_now < MERGE_AVG_SUM_MAX
            if mergeable >= MERGE_TRIGGER_PAIR_QTY and throttle_ok and avg_ok:
                pq = max(np.floor(mergeable), MERGE_TRIGGER_PAIR_QTY)
                s.apply_merge(pq, ts, force=force_merge)

    if last_window is not None:
        s.settle_window(last_binance, last_strike, last_window*1000)
    pnl = s.cash_received - s.cash_paid - s.total_fee
    return pnl, s


print(f"{'Method':<28} {'PnL':>9} {'merge':>7} {'fee':>7} {'pairs':>6} {'chases':>7} {'max_dd':>9}")
print("-" * 80)

# Different gap thresholds
for gap in [0.03, 0.04, 0.05, 0.06, 0.07, 0.08, 0.10]:
    pnl, st = run(df, gap, throttle_ms=1000, e0=True)
    chase = st.chase_up_count + st.chase_down_count
    print(f"E0 + gap={gap:.2f}".ljust(28) +
          f" ${pnl:+8.2f}  {st.merge_pnl:+6.1f}  {st.total_fee:6.1f}  "
          f"{st.merged_pairs:6.0f}  {chase:7d}  ?")

# Different throttle
print()
for thr in [1000, 2000, 3000, 5000, 10000]:
    pnl, st = run(df, 0.03, throttle_ms=thr, e0=True)
    chase = st.chase_up_count + st.chase_down_count
    print(f"E0 + throttle={thr}ms".ljust(28) +
          f" ${pnl:+8.2f}  {st.merge_pnl:+6.1f}  {st.total_fee:6.1f}  "
          f"{st.merged_pairs:6.0f}  {chase:7d}  ?")

# Combined: high gap + high throttle
print()
for gap in [0.05, 0.07]:
    for thr in [2000, 5000]:
        pnl, st = run(df, gap, throttle_ms=thr, e0=True)
        chase = st.chase_up_count + st.chase_down_count
        print(f"E0 gap={gap:.2f} thr={thr}ms".ljust(28) +
              f" ${pnl:+8.2f}  {st.merge_pnl:+6.1f}  {st.total_fee:6.1f}  "
              f"{st.merged_pairs:6.0f}  {chase:7d}  ?")
