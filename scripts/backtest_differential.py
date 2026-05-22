#!/usr/bin/env python3
"""差异化守门：建仓腿(leg1)和配对腿(leg2)用不同标准."""
import sys
sys.path.insert(0, '/Users/zenith/btc-poly/market-maker-5m/scripts')
from collections import deque
import numpy as np
from backtest_guards import (State, MAKER_BUY_CHASE_MIN_QTY, MAX_BUY_PRICE,
                              MIN_EXPIRY_MIN_FOR_CHASE, MERGE_AVG_SUM_MAX,
                              FORCE_MERGE_EXPIRY_MIN, MERGE_INTERVAL_MS,
                              MERGE_TRIGGER_PAIR_QTY, load_data)

DATA_DIR = sys.argv[1] if len(sys.argv) > 1 else "/tmp/aerlan_live"
df = load_data(DATA_DIR)
print(f"Loaded {len(df):,} snapshots\n")


def fv_rising(hist, current):
    if len(hist) < 3: return False
    return current > sum(hist) / len(hist)


def run_differential(df, gap_min, leg2_max, throttle_ms=1000):
    """leg1: 对侧空仓 + force-balance；leg2: 对侧有仓 + my+opp_avg < leg2_max"""
    s = State()
    last_window = None
    last_binance = 0.0
    last_strike = 0.0
    leg1_count = 0
    leg2_count = 0

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

        # 差异化守门 - 区分建仓腿和配对腿
        def can_buy(side, price, ua, da):
            if not (0 < price <= MAX_BUY_PRICE):
                return None
            if side == "UP":
                # 建仓腿: 对侧 DOWN 空仓 + force-balance(qty_up<=qty_down+0)
                if s.pos_down.qty == 0.0 and s.pos_up.qty <= s.pos_down.qty:
                    return "leg1"
                # 配对腿: 对侧 DOWN 已有仓位 + 健康守门
                if s.pos_down.qty > 0.0 and (price + s.pos_down.avg) < leg2_max:
                    return "leg2"
            else:
                if s.pos_up.qty == 0.0 and s.pos_down.qty <= s.pos_up.qty:
                    return "leg1"
                if s.pos_up.qty > 0.0 and (price + s.pos_up.avg) < leg2_max:
                    return "leg2"
            return None

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
                leg = can_buy("UP", up_ask, up_ask, down_ask)
                if leg:
                    s.apply_fill("UP", up_ask, MAKER_BUY_CHASE_MIN_QTY, ts)
                    if leg == "leg1": leg1_count += 1
                    else: leg2_count += 1
            if down_chase and ts - s.last_taker_down_ts >= throttle_ms:
                leg = can_buy("DOWN", down_ask, up_ask, down_ask)
                if leg:
                    s.apply_fill("DOWN", down_ask, MAKER_BUY_CHASE_MIN_QTY, ts)
                    if leg == "leg1": leg1_count += 1
                    else: leg2_count += 1
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
    return pnl, s, leg1_count, leg2_count


print(f"{'方案':<35} {'PnL':>10} {'merge':>7} {'fee':>6} {'pairs':>5} {'leg1':>5} {'leg2':>5}")
print("-" * 80)

# 当前 v0.4.10 (gap=10c + force-balance + PAIR_HEALTH<1.05)
def baseline_v0410(df, gap_min, throttle_ms=1000):
    s = State()
    last_window = None; last_binance = 0.0; last_strike = 0.0
    for row in df.itertuples():
        ts = int(row.t_ms)
        win_end = int(row.window_end_ts)
        binance_mid = float(row.binance_mid)
        fv_up = float(row.fv_up); fv_down = float(row.fv_down)
        up_bid = float(row.poly_up_bid); up_ask = float(row.poly_up_ask)
        down_bid = float(row.poly_down_bid); down_ask = float(row.poly_down_ask)
        market_state = row.market_state
        expiry_min = float(row.expiry_min); strike = float(row.strike); source = row.source
        if last_window is not None and win_end != last_window:
            s.settle_window(last_binance, last_strike, ts)
        last_window = win_end; last_binance = binance_mid; last_strike = strike
        in_excited = (market_state == "E"); in_chase = expiry_min >= MIN_EXPIRY_MIN_FOR_CHASE

        def can_v410(side, price, ua, da):
            if not (0 < price <= MAX_BUY_PRICE): return False
            qty = MAKER_BUY_CHASE_MIN_QTY
            if side == "UP":
                if s.pos_up.qty > s.pos_down.qty: return False
                new_q = s.pos_up.qty + qty
                new_avg_up = (s.pos_up.qty * s.pos_up.avg + price * qty) / new_q
                opp = s.pos_down.avg if s.pos_down.qty > 0 else (da if da > 0 else 0.5)
                return (new_avg_up + opp) < 1.05
            else:
                if s.pos_down.qty > s.pos_up.qty: return False
                new_q = s.pos_down.qty + qty
                new_avg_dn = (s.pos_down.qty * s.pos_down.avg + price * qty) / new_q
                opp = s.pos_up.avg if s.pos_up.qty > 0 else (ua if ua > 0 else 0.5)
                return (opp + new_avg_dn) < 1.05

        if source == "B":
            up_mid = (up_bid+up_ask)/2 if (up_bid>0 and up_ask>0) else 0.0
            down_mid = (down_bid+down_ask)/2 if (down_bid>0 and down_ask>0) else 0.0
            up_chase = (in_excited and in_chase and up_mid>0 and (fv_up-up_mid)>=gap_min and fv_rising(s.fv_up_hist, fv_up))
            down_chase = (in_excited and in_chase and down_mid>0 and (fv_down-down_mid)>=gap_min and fv_rising(s.fv_down_hist, fv_down))
            if up_chase and ts - s.last_taker_up_ts >= throttle_ms:
                if can_v410("UP", up_ask, up_ask, down_ask):
                    s.apply_fill("UP", up_ask, MAKER_BUY_CHASE_MIN_QTY, ts)
            if down_chase and ts - s.last_taker_down_ts >= throttle_ms:
                if can_v410("DOWN", down_ask, up_ask, down_ask):
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
    return s.cash_received - s.cash_paid - s.total_fee, s

pnl, st = baseline_v0410(df, 0.10)
print(f"v0.4.10 (gap=10c + PAIR<1.05)".ljust(35) +
      f" ${pnl:+9.2f}  {st.merge_pnl:+6.1f}  {st.total_fee:6.1f}  "
      f"{st.merged_pairs:5.0f}  {st.chase_up_count+st.chase_down_count:5d}  -")

print()
# 差异化 leg1/leg2 with leg2_max sweep
for gap in [0.07, 0.10]:
    for leg2_max in [0.97, 0.98, 1.00, 1.02]:
        pnl, st, leg1, leg2 = run_differential(df, gap, leg2_max)
        print(f"diff gap={gap:.2f} leg2<{leg2_max:.2f}".ljust(35) +
              f" ${pnl:+9.2f}  {st.merge_pnl:+6.1f}  {st.total_fee:6.1f}  "
              f"{st.merged_pairs:5.0f}  {leg1:5d}  {leg2:5d}")
