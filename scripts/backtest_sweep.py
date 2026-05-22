#!/usr/bin/env python3
"""Parameter sweep for the leading guard schemes."""
import sys
sys.path.insert(0, '/Users/zenith/btc-poly/market-maker-5m/scripts')
from backtest_guards import backtest, load_data

import sys as _sys
_data_dir = _sys.argv[1] if len(_sys.argv) > 1 else "/Users/zenith/btc-poly/market-maker-5m/fv_snapshots"
df = load_data(_data_dir)
print(f"Loaded {len(df):,} snapshots")

print("\n=== C: avg_sum_after threshold sweep ===")
for thresh in [0.90, 0.92, 0.94, 0.95, 0.96, 0.98, 1.00, 1.02, 1.05]:
    r = backtest(df, "C", max_avg_sum=thresh)
    print(f"  C(max={thresh:.2f}): PnL=${r['final_pnl']:+8.2f} pairs={r['merged_pairs']:5.0f} "
          f"chases={r['chase_up']:3d}+{r['chase_down']:3d}={r['chase_up']+r['chase_down']:3d} "
          f"max_up={r['max_qty_up']:5.0f} max_dn={r['max_qty_down']:5.0f}")

print("\n=== B: arb_window threshold sweep ===")
for thresh in [1.00, 1.01, 1.015, 1.02, 1.025, 1.03, 1.04, 1.05]:
    r = backtest(df, "B", arb_thresh=thresh)
    print(f"  B(thr={thresh:.3f}): PnL=${r['final_pnl']:+8.2f} pairs={r['merged_pairs']:5.0f} "
          f"chases={r['chase_up']:3d}+{r['chase_down']:3d} "
          f"max_up={r['max_qty_up']:5.0f} max_dn={r['max_qty_down']:5.0f}")

print("\n=== D: qty cap sweep ===")
for cap in [200, 300, 500, 800, 1000, 1500, 2000, 3000]:
    r = backtest(df, "D", cap=cap)
    print(f"  D(cap={cap:5d}): PnL=${r['final_pnl']:+8.2f} pairs={r['merged_pairs']:5.0f} "
          f"chases={r['chase_up']:3d}+{r['chase_down']:3d} "
          f"max_up={r['max_qty_up']:5.0f} max_dn={r['max_qty_down']:5.0f}")

print("\n=== E: force-balance slack sweep ===")
for slack in [0, 100, 200, 300, 500, 1000]:
    r = backtest(df, "E", slack=slack)
    print(f"  E(slack={slack:4d}): PnL=${r['final_pnl']:+8.2f} pairs={r['merged_pairs']:5.0f} "
          f"chases={r['chase_up']:3d}+{r['chase_down']:3d} "
          f"max_up={r['max_qty_up']:5.0f} max_dn={r['max_qty_down']:5.0f}")

print("\n=== C2: cost-aware avg_sum threshold sweep ===")
for thresh in [1.00, 1.005, 1.01, 1.015, 1.02, 1.025, 1.03, 1.05]:
    r = backtest(df, "C2", max_avg_sum=thresh)
    print(f"  C2(max={thresh:.3f}): PnL=${r['final_pnl']:+8.2f} pairs={r['merged_pairs']:5.0f} "
          f"chases={r['chase_up']:3d}+{r['chase_down']:3d} "
          f"max_up={r['max_qty_up']:5.0f} max_dn={r['max_qty_down']:5.0f}")
