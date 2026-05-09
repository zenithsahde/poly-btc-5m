#!/usr/bin/env python3
"""Combo sweep: C with various secondary guards."""
import sys
sys.path.insert(0, '/Users/zenith/btc-poly/market-maker-5m/scripts')
from backtest_guards import backtest, load_data, make_guard, MAKER_BUY_CHASE_MIN_QTY, MAX_BUY_PRICE

df = load_data("/Users/zenith/btc-poly/market-maker-5m/fv_snapshots")
print(f"Loaded {len(df):,} snapshots\n")

# Manually wire C + cap combo
def run_C_plus_D(df, max_avg_sum, cap):
    """C + D combo via composing guards inline."""
    # Re-implement here inline to combine
    import sys
    sys.path.insert(0, '/Users/zenith/btc-poly/market-maker-5m/scripts')
    from backtest_guards import State, fv_rising, MAKER_BUY_CHASE_MIN_QTY, MAX_BUY_PRICE
    from backtest_guards import (CHASE_GAP_MIN, FV_HISTORY_LEN, MIN_EXPIRY_MIN_FOR_CHASE,
                                  MERGE_AVG_SUM_MAX, FORCE_MERGE_EXPIRY_MIN, MERGE_INTERVAL_MS,
                                  TAKER_BUY_INTERVAL_MS, MERGE_TRIGGER_PAIR_QTY)
    import numpy as np
    s = State()

    def guard(side, price):
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        # C check
        qty = MAKER_BUY_CHASE_MIN_QTY
        if side == "UP":
            new_q = s.pos_up.qty + qty
            new_avg_up = (s.pos_up.qty * s.pos_up.avg + price * qty) / new_q if new_q > 0 else price
            new_sum = new_avg_up + s.pos_down.avg
            if s.pos_up.qty + qty > cap:
                return False
        else:
            new_q = s.pos_down.qty + qty
            new_avg_down = (s.pos_down.qty * s.pos_down.avg + price * qty) / new_q if new_q > 0 else price
            new_sum = s.pos_up.avg + new_avg_down
            if s.pos_down.qty + qty > cap:
                return False
        return new_sum < max_avg_sum

    last_window = None
    last_binance = 0.0
    last_strike = 0.0
    pnl_curve = []
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
        in_chase_window = expiry_min >= MIN_EXPIRY_MIN_FOR_CHASE

        if source == "B":
            up_mid_p = (up_bid+up_ask)/2 if (up_bid>0 and up_ask>0) else 0.0
            down_mid_p = (down_bid+down_ask)/2 if (down_bid>0 and down_ask>0) else 0.0
            up_chase = (in_excited and in_chase_window and up_mid_p>0
                       and (fv_up - up_mid_p) >= CHASE_GAP_MIN
                       and fv_rising(s.fv_up_hist, fv_up))
            down_chase = (in_excited and in_chase_window and down_mid_p>0
                         and (fv_down - down_mid_p) >= CHASE_GAP_MIN
                         and fv_rising(s.fv_down_hist, fv_down))
            if up_chase and ts - s.last_taker_up_ts >= TAKER_BUY_INTERVAL_MS:
                if guard("UP", up_ask):
                    s.apply_fill("UP", up_ask, MAKER_BUY_CHASE_MIN_QTY, ts)
                else:
                    s.rejected_up += 1
            if down_chase and ts - s.last_taker_down_ts >= TAKER_BUY_INTERVAL_MS:
                if guard("DOWN", down_ask):
                    s.apply_fill("DOWN", down_ask, MAKER_BUY_CHASE_MIN_QTY, ts)
                else:
                    s.rejected_down += 1
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
    return s.cash_received - s.cash_paid - s.total_fee, s


print("=== C + D (cap) combo: max_avg_sum=0.98 ===")
for cap in [200, 300, 500, 800, 1000, 1500, 2500, 5000, 10000]:
    pnl, st = run_C_plus_D(df, 0.98, cap)
    print(f"  C(0.98)+D(cap={cap:5d}): PnL=${pnl:+8.2f} pairs={st.merged_pairs:5.0f} "
          f"chases={st.chase_up_count:3d}+{st.chase_down_count:3d} "
          f"max_up={st.max_qty_up:5.0f} max_dn={st.max_qty_down:5.0f}")

print("\n=== C threshold + cap=infinity (pure C) ===")
for thresh in [0.85, 0.90, 0.93, 0.95, 0.97, 0.98, 0.99, 1.00]:
    pnl, st = run_C_plus_D(df, thresh, 99999)
    print(f"  C({thresh:.2f}): PnL=${pnl:+8.2f} pairs={st.merged_pairs:5.0f} "
          f"chases={st.chase_up_count:3d}+{st.chase_down_count:3d} "
          f"max_up={st.max_qty_up:5.0f} max_dn={st.max_qty_down:5.0f}")
