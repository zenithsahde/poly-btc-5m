#!/usr/bin/env python3
"""Test E0 + C combo on live data."""
import sys
sys.path.insert(0, '/Users/zenith/btc-poly/market-maker-5m/scripts')
from backtest_curve import run_with_guard_track
from backtest_guards import load_data, MAKER_BUY_CHASE_MIN_QTY, MAX_BUY_PRICE

DATA_DIR = sys.argv[1] if len(sys.argv) > 1 else "/tmp/aerlan_live"
df = load_data(DATA_DIR)
print(f"Loaded {len(df):,} snapshots from {DATA_DIR}\n")


def make_E0_C(max_avg, slack=0.0):
    """E0 force-balance + C avg_sum_after guard."""
    def g(s, side, price, ua, da):
        if not (0 < price <= MAX_BUY_PRICE):
            return False
        # E0: only buy the smaller side
        if side == "UP":
            if s.pos_up.qty > s.pos_down.qty + slack:
                return False
        else:
            if s.pos_down.qty > s.pos_up.qty + slack:
                return False
        # C: avg_sum after the fill must be < max_avg
        qty = MAKER_BUY_CHASE_MIN_QTY
        if side == "UP":
            new_q = s.pos_up.qty + qty
            new_avg_up = (s.pos_up.qty * s.pos_up.avg + price * qty) / new_q if new_q > 0 else price
            opp = s.pos_down.avg if s.pos_down.qty > 0 else (da if da > 0 else 0.5)
            new_sum = new_avg_up + opp
        else:
            new_q = s.pos_down.qty + qty
            new_avg_down = (s.pos_down.qty * s.pos_down.avg + price * qty) / new_q if new_q > 0 else price
            opp = s.pos_up.avg if s.pos_up.qty > 0 else (ua if ua > 0 else 0.5)
            new_sum = opp + new_avg_down
        return new_sum < max_avg
    return g


def baseline_guard(s, side, price, ua, da):
    return 0 < price <= MAX_BUY_PRICE


def E0_only(s, side, price, ua, da):
    if not (0 < price <= MAX_BUY_PRICE):
        return False
    if side == "UP":
        return s.pos_up.qty <= s.pos_down.qty
    else:
        return s.pos_down.qty <= s.pos_up.qty


print(f"{'方案':<25} {'最终PnL':>10} {'merge':>8} {'pairs':>6} {'max_u':>6} {'max_d':>6} {'chase':>6} {'max_dd':>9}")
print("-" * 90)

cases = [
    ("baseline", baseline_guard),
    ("E0 (v0.4.6 当前线上)", E0_only),
    ("E0 + C(max=0.99)", make_E0_C(0.99)),
    ("E0 + C(max=1.00)", make_E0_C(1.00)),
    ("E0 + C(max=1.01)", make_E0_C(1.01)),
    ("E0 + C(max=1.02)", make_E0_C(1.02)),
    ("E0 + C(max=1.03)", make_E0_C(1.03)),
    ("E0 + C(max=1.05)", make_E0_C(1.05)),
]
for name, g in cases:
    pnl_pw, st = run_with_guard_track(df, g, name)
    final = st.cash_received - st.cash_paid - st.total_fee
    min_p = min((d['close'] or 0) for d in pnl_pw.values() if d['close'] is not None)
    chase = st.chase_up_count + st.chase_down_count
    print(f"{name:<25} ${final:+9.2f}  {st.merge_pnl:+7.1f}  {st.merged_pairs:6.0f}  "
          f"{st.max_qty_up:6.0f} {st.max_qty_down:6.0f}  {chase:6d}  ${min_p:+8.0f}")
