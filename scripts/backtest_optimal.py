#!/usr/bin/env python3
"""Find true optimal: E + C2 combos, fine grid."""
import sys
sys.path.insert(0, '/Users/zenith/btc-poly/market-maker-5m/scripts')
from backtest_guards import (State, fv_rising, MAKER_BUY_CHASE_MIN_QTY, MAX_BUY_PRICE,
                              CHASE_GAP_MIN, FV_HISTORY_LEN, MIN_EXPIRY_MIN_FOR_CHASE,
                              MERGE_AVG_SUM_MAX, FORCE_MERGE_EXPIRY_MIN, MERGE_INTERVAL_MS,
                              TAKER_BUY_INTERVAL_MS, MERGE_TRIGGER_PAIR_QTY, load_data)
import numpy as np

DATA_DIR = sys.argv[1] if len(sys.argv) > 1 else "/tmp/fv_snap_frozen"
df = load_data(DATA_DIR)
print(f"Loaded {len(df):,} snapshots from {DATA_DIR}\n")


def run_with_guard(df, guard_fn):
    s = State()
    last_window = None
    last_binance = 0.0
    last_strike = 0.0
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
        s.settle_window(last_binance, last_strike, last_window*1000)
    final_pnl = s.cash_received - s.cash_paid - s.total_fee
    return final_pnl, s


def make_E(slack):
    def g(s, side, price, up_ask, down_ask):
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        if side == "UP":
            return s.pos_up.qty <= s.pos_down.qty + slack
        else:
            return s.pos_down.qty <= s.pos_up.qty + slack
    return g


def make_E_with_cap(slack, cap):
    def g(s, side, price, up_ask, down_ask):
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        if side == "UP":
            if s.pos_up.qty >= cap: return False
            return s.pos_up.qty <= s.pos_down.qty + slack
        else:
            if s.pos_down.qty >= cap: return False
            return s.pos_down.qty <= s.pos_up.qty + slack
    return g


def make_E_only_first_chase(slack):
    """E + only allow first leg to be on the side with lower poly price (lead alpha side)."""
    def g(s, side, price, up_ask, down_ask):
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        # If both sides empty, allow any (let chase decide direction)
        if s.pos_up.qty == 0 and s.pos_down.qty == 0:
            return True
        if side == "UP":
            return s.pos_up.qty <= s.pos_down.qty + slack
        else:
            return s.pos_down.qty <= s.pos_up.qty + slack
    return g


def make_E_balanced_strict(max_diff):
    """Strictly limit |qty_up - qty_down| <= max_diff."""
    def g(s, side, price, up_ask, down_ask):
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        if side == "UP":
            new_up = s.pos_up.qty + MAKER_BUY_CHASE_MIN_QTY
            return (new_up - s.pos_down.qty) <= max_diff
        else:
            new_dn = s.pos_down.qty + MAKER_BUY_CHASE_MIN_QTY
            return (new_dn - s.pos_up.qty) <= max_diff
    return g


def make_E_with_C2(slack, max_avg):
    def g(s, side, price, up_ask, down_ask):
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        # E
        if side == "UP":
            if s.pos_up.qty > s.pos_down.qty + slack: return False
            new_q = s.pos_up.qty + MAKER_BUY_CHASE_MIN_QTY
            new_avg_up = (s.pos_up.qty * s.pos_up.avg + price * MAKER_BUY_CHASE_MIN_QTY) / new_q
            opp = s.pos_down.avg if s.pos_down.qty > 0 else (down_ask if down_ask>0 else 0.5)
            new_sum = new_avg_up + opp
        else:
            if s.pos_down.qty > s.pos_up.qty + slack: return False
            new_q = s.pos_down.qty + MAKER_BUY_CHASE_MIN_QTY
            new_avg_down = (s.pos_down.qty * s.pos_down.avg + price * MAKER_BUY_CHASE_MIN_QTY) / new_q
            opp = s.pos_up.avg if s.pos_up.qty > 0 else (up_ask if up_ask>0 else 0.5)
            new_sum = opp + new_avg_down
        return new_sum < max_avg
    return g


print("=== E (force-balance) fine sweep ===")
for slack in [0, 50, 100, 150, 200, 250, 300, 400, 500]:
    pnl, st = run_with_guard(df, make_E(slack))
    print(f"  E(slack={slack:4d}): PnL=${pnl:+8.2f} pairs={st.merged_pairs:5.0f} "
          f"chases={st.chase_up_count:3d}+{st.chase_down_count:3d}={st.chase_up_count+st.chase_down_count:3d} "
          f"max_up={st.max_qty_up:5.0f} max_dn={st.max_qty_down:5.0f}")

print("\n=== E + qty cap (slack=100) ===")
for cap in [100, 200, 300, 500, 1000]:
    pnl, st = run_with_guard(df, make_E_with_cap(100, cap))
    print(f"  E(slack=100,cap={cap:4d}): PnL=${pnl:+8.2f} pairs={st.merged_pairs:5.0f} "
          f"chases={st.chase_up_count:3d}+{st.chase_down_count:3d} "
          f"max_up={st.max_qty_up:5.0f} max_dn={st.max_qty_down:5.0f}")

print("\n=== Strict balance |diff| <= max_diff ===")
for d in [50, 100, 150, 200, 300, 500]:
    pnl, st = run_with_guard(df, make_E_balanced_strict(d))
    print(f"  StrictBal(diff<={d:3d}): PnL=${pnl:+8.2f} pairs={st.merged_pairs:5.0f} "
          f"chases={st.chase_up_count:3d}+{st.chase_down_count:3d} "
          f"max_up={st.max_qty_up:5.0f} max_dn={st.max_qty_down:5.0f}")

print("\n=== E + C2 combo ===")
for slack in [0, 100, 200]:
    for max_avg in [1.01, 1.02, 1.03, 1.05]:
        pnl, st = run_with_guard(df, make_E_with_C2(slack, max_avg))
        print(f"  E(s={slack:3d})+C2(max={max_avg:.2f}): PnL=${pnl:+8.2f} pairs={st.merged_pairs:5.0f} "
              f"chases={st.chase_up_count:3d}+{st.chase_down_count:3d} "
              f"max_up={st.max_qty_up:5.0f} max_dn={st.max_qty_down:5.0f}")
