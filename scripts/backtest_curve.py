#!/usr/bin/env python3
"""Plot PnL curves and per-window breakdown for top guards."""
import sys
sys.path.insert(0, '/Users/zenith/btc-poly/market-maker-5m/scripts')
from backtest_guards import (State, fv_rising, MAKER_BUY_CHASE_MIN_QTY, MAX_BUY_PRICE,
                              CHASE_GAP_MIN, MIN_EXPIRY_MIN_FOR_CHASE,
                              MERGE_AVG_SUM_MAX, FORCE_MERGE_EXPIRY_MIN, MERGE_INTERVAL_MS,
                              TAKER_BUY_INTERVAL_MS, MERGE_TRIGGER_PAIR_QTY, load_data)
import numpy as np
from collections import defaultdict

DATA_DIR = sys.argv[1] if len(sys.argv) > 1 else "/tmp/fv_snap_frozen"
df = load_data(DATA_DIR)
print(f"Loaded {len(df):,} snapshots from {DATA_DIR}\n")


def run_with_guard_track(df, guard_fn, name):
    s = State()
    last_window = None
    last_binance = 0.0
    last_strike = 0.0
    pnl_per_window = defaultdict(lambda: {"open": None, "close": None, "settle": None})

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
            # Pre-settle PnL
            pre = s.cash_received - s.cash_paid - s.total_fee
            s.settle_window(last_binance, last_strike, ts)
            post = s.cash_received - s.cash_paid - s.total_fee
            pnl_per_window[last_window]["close"] = pre
            pnl_per_window[last_window]["settle"] = post

        if pnl_per_window[win_end]["open"] is None:
            pnl_per_window[win_end]["open"] = s.cash_received - s.cash_paid - s.total_fee

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
                if guard_fn(s, "UP", up_ask, up_ask, down_ask):
                    s.apply_fill("UP", up_ask, MAKER_BUY_CHASE_MIN_QTY, ts)
                else:
                    s.rejected_up += 1
            if down_chase and ts - s.last_taker_down_ts >= TAKER_BUY_INTERVAL_MS:
                if guard_fn(s, "DOWN", down_ask, up_ask, down_ask):
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
        pre = s.cash_received - s.cash_paid - s.total_fee
        s.settle_window(last_binance, last_strike, last_window*1000)
        post = s.cash_received - s.cash_paid - s.total_fee
        pnl_per_window[last_window]["close"] = pre
        pnl_per_window[last_window]["settle"] = post

    return pnl_per_window, s


def baseline_guard(s, side, price, ua, da):
    return 0 < price <= MAX_BUY_PRICE


def E200_C2_guard(s, side, price, ua, da):
    if not (0 < price <= MAX_BUY_PRICE):
        return False
    slack = 200; max_avg = 1.02
    if side == "UP":
        if s.pos_up.qty > s.pos_down.qty + slack: return False
        new_q = s.pos_up.qty + MAKER_BUY_CHASE_MIN_QTY
        new_avg_up = (s.pos_up.qty * s.pos_up.avg + price * MAKER_BUY_CHASE_MIN_QTY) / new_q
        opp = s.pos_down.avg if s.pos_down.qty > 0 else (da if da>0 else 0.5)
        return (new_avg_up + opp) < max_avg
    else:
        if s.pos_down.qty > s.pos_up.qty + slack: return False
        new_q = s.pos_down.qty + MAKER_BUY_CHASE_MIN_QTY
        new_avg_down = (s.pos_down.qty * s.pos_down.avg + price * MAKER_BUY_CHASE_MIN_QTY) / new_q
        opp = s.pos_up.avg if s.pos_up.qty > 0 else (ua if ua>0 else 0.5)
        return (opp + new_avg_down) < max_avg


def E0_guard(s, side, price, ua, da):
    if not (0 < price <= MAX_BUY_PRICE):
        return False
    if side == "UP":
        return s.pos_up.qty <= s.pos_down.qty
    else:
        return s.pos_down.qty <= s.pos_up.qty


for name, g in [("baseline", baseline_guard), ("E0", E0_guard), ("E200+C2(1.02)", E200_C2_guard)]:
    pnl_pw, st = run_with_guard_track(df, g, name)
    print(f"\n=== {name} ===")
    print(f"{'Window':>16}  {'Open':>10}  {'PreSettle':>10}  {'PostSettle':>10}  {'WindowDelta':>12}")
    cumul_open = 0
    for win in sorted(pnl_pw.keys()):
        d = pnl_pw[win]
        op = d["open"] or 0
        cl = d["close"] or 0
        st_ = d["settle"] or 0
        delta = st_ - op
        print(f"{win:16d}  {op:+10.2f}  {cl:+10.2f}  {st_:+10.2f}  {delta:+12.2f}")
    print(f"  Final: PnL=${st.cash_received - st.cash_paid - st.total_fee:+.2f}  "
          f"merge_pnl={st.merge_pnl:+.1f}  pairs={st.merged_pairs:.0f}  "
          f"max_up={st.max_qty_up:.0f}  max_dn={st.max_qty_down:.0f}")
