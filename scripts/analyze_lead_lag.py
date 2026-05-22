#!/usr/bin/env python3
"""
后置 lead-lag 实证分析（v0.3.3-5m）。

输入：fv_snapshots/fv_{window_end_ts}.csv  (BookTicker + PolyBookUpdate 全量帧)
输出：
  1. FV vs Poly_p 偏差分布（mean / std / quantile）
  2. cross-correlation: corr(FV(t), Poly_p(t+lag)) for lag in [-3000ms, +3000ms]
  3. 最优 lag (= FV 领先 Poly_p 的时间常数)
  4. σ_ema 收敛轨迹

用法: python3 scripts/analyze_lead_lag.py [fv_*.csv ...]
"""
import sys, os, glob, csv
from collections import defaultdict

def load_csv(path):
    rows = []
    with open(path) as f:
        rdr = csv.DictReader(f)
        for r in rdr:
            try:
                rows.append({
                    "t_ms": int(r["t_ms"]),
                    "source": r["source"],
                    "binance_mid": float(r["binance_mid"]),
                    "fv_up": float(r["fv_up"]),
                    "fv_down": float(r["fv_down"]),
                    "poly_up_bid": float(r["poly_up_bid"]),
                    "poly_up_ask": float(r["poly_up_ask"]),
                    "poly_down_bid": float(r["poly_down_bid"]),
                    "poly_down_ask": float(r["poly_down_ask"]),
                    "sigma": float(r["sigma"]),
                    "iv_raw": float(r["iv_raw"]),
                    "sigma_ema": float(r["sigma_ema"]),
                    "market_state": r["market_state"],
                    "expiry_min": float(r["expiry_min"]),
                    "strike": float(r["strike"]),
                })
            except (ValueError, KeyError):
                continue
    rows.sort(key=lambda x: x["t_ms"])
    return rows

def poly_up_mid(r):
    a, b = r["poly_up_bid"], r["poly_up_ask"]
    if a > 0 and b > 0:
        return (a + b) / 2.0
    a2, b2 = r["poly_down_bid"], r["poly_down_ask"]
    if a2 > 0 and b2 > 0:
        return 1.0 - (a2 + b2) / 2.0
    return None

def stats(arr):
    if not arr: return None
    arr_s = sorted(arr)
    n = len(arr_s)
    mean = sum(arr) / n
    var = sum((x - mean) ** 2 for x in arr) / max(1, n - 1)
    std = var ** 0.5
    def q(p):
        idx = int(p * (n - 1))
        return arr_s[idx]
    return dict(n=n, mean=mean, std=std, p1=q(0.01), p5=q(0.05),
                p25=q(0.25), p50=q(0.50), p75=q(0.75), p95=q(0.95), p99=q(0.99),
                mn=arr_s[0], mx=arr_s[-1])

def resample_uniform(rows, step_ms=100):
    """重采样到 step_ms 等间距网格，用 forward-fill 补齐每个时刻的 fv 和 poly_p"""
    if not rows: return []
    t0 = rows[0]["t_ms"]
    t1 = rows[-1]["t_ms"]
    grid = []
    i = 0
    last_fv, last_poly = None, None
    t = t0
    while t <= t1:
        # 推进 i 到 ≤ t 的最新一行
        while i + 1 < len(rows) and rows[i + 1]["t_ms"] <= t:
            i += 1
        r = rows[i]
        fv = r["fv_up"] if r["fv_up"] > 0 else last_fv
        pm = poly_up_mid(r)
        if pm is None:
            pm = last_poly
        if fv is not None: last_fv = fv
        if pm is not None: last_poly = pm
        if last_fv is not None and last_poly is not None:
            grid.append((t, last_fv, last_poly))
        t += step_ms
    return grid

def cross_correlate(grid, max_lag_steps, step_ms):
    """corr(FV(t), Poly_p(t+lag)) for lag ∈ [-max_lag_steps, +max_lag_steps]
    正 lag = FV 早于 Poly_p（FV 领先）"""
    fv = [g[1] for g in grid]
    pp = [g[2] for g in grid]
    n = len(fv)
    if n < 2 * max_lag_steps + 10:
        return []

    def corr(x, y):
        m = len(x)
        mx = sum(x) / m
        my = sum(y) / m
        sxy = sum((x[i] - mx) * (y[i] - my) for i in range(m))
        sxx = sum((x[i] - mx) ** 2 for i in range(m))
        syy = sum((y[i] - my) ** 2 for i in range(m))
        denom = (sxx * syy) ** 0.5
        return sxy / denom if denom > 0 else 0.0

    results = []
    for lag in range(-max_lag_steps, max_lag_steps + 1):
        if lag >= 0:
            x = fv[:n - lag]
            y = pp[lag:]
        else:
            x = fv[-lag:]
            y = pp[:n + lag]
        c = corr(x, y)
        results.append((lag * step_ms, c))
    return results

def analyze_file(path):
    print(f"\n{'='*70}")
    print(f"FILE: {path}")
    print(f"{'='*70}")
    rows = load_csv(path)
    if not rows:
        print("  (空)")
        return None

    src_counts = defaultdict(int)
    state_counts = defaultdict(int)
    for r in rows:
        src_counts[r["source"]] += 1
        state_counts[r["market_state"]] += 1
    span_s = (rows[-1]["t_ms"] - rows[0]["t_ms"]) / 1000.0
    print(f"  rows={len(rows)} span={span_s:.1f}s  src={dict(src_counts)}  state={dict(state_counts)}")

    # FV - Poly_p 偏差
    diffs = []
    fv_lead_count = 0
    fv_lag_count = 0
    for r in rows:
        pm = poly_up_mid(r)
        if pm is None: continue
        d = r["fv_up"] - pm
        diffs.append(d)
    s = stats(diffs)
    if s:
        print(f"\n  FV_up - Poly_up_mid 偏差 (cents):")
        print(f"    n={s['n']}  mean={s['mean']*100:+.2f}c  std={s['std']*100:.2f}c")
        print(f"    p1={s['p1']*100:+.2f}c  p25={s['p25']*100:+.2f}c  p50={s['p50']*100:+.2f}c  p75={s['p75']*100:+.2f}c  p99={s['p99']*100:+.2f}c")
        print(f"    min={s['mn']*100:+.2f}c  max={s['mx']*100:+.2f}c")

    # σ_ema 收敛
    sigma_ema_arr = [r["sigma_ema"] for r in rows if r["sigma_ema"] > 0]
    if sigma_ema_arr:
        s_ema = stats(sigma_ema_arr)
        first_init_idx = next((i for i, r in enumerate(rows) if r["sigma_ema"] > 0), None)
        first_init_t = (rows[first_init_idx]["t_ms"] - rows[0]["t_ms"]) if first_init_idx is not None else None
        print(f"\n  σ_ema 轨迹:")
        print(f"    init delay: {first_init_t}ms after stream start")
        print(f"    final value (last frame): {sigma_ema_arr[-1]:.4f}")
        print(f"    min={s_ema['mn']:.4f}  max={s_ema['mx']:.4f}  std={s_ema['std']:.4f}")

    # cross-correlation lead-lag
    print(f"\n  Lead-lag (resample 100ms grid, scan ±3000ms):")
    grid = resample_uniform(rows, step_ms=100)
    print(f"    grid size: {len(grid)}")
    if len(grid) >= 100:
        ccs = cross_correlate(grid, max_lag_steps=30, step_ms=100)
        if ccs:
            best_lag, best_c = max(ccs, key=lambda x: x[1])
            zero_c = next((c for l, c in ccs if l == 0), None)
            print(f"    corr@lag=0:    {zero_c:+.4f}")
            print(f"    best_lag:      {best_lag:+d}ms  (FV 领先 Poly_p {best_lag}ms)" if best_lag > 0 else f"    best_lag:      {best_lag:+d}ms  (FV 滞后 Poly_p {-best_lag}ms)")
            print(f"    best_corr:     {best_c:+.4f}")
            print(f"    ── 关键 lag 点位 ──")
            for target_lag in [-1500, -1000, -500, -200, 0, 200, 500, 1000, 1500]:
                c = next((cc for l, cc in ccs if l == target_lag), None)
                if c is not None:
                    print(f"    corr@lag={target_lag:+5d}ms: {c:+.4f}")
    return rows

def main():
    if len(sys.argv) > 1:
        files = sys.argv[1:]
    else:
        files = sorted(glob.glob("fv_snapshots/fv_*.csv"))
    if not files:
        print("ERROR: 未找到 fv_snapshots/fv_*.csv，先跑引擎采集数据。")
        sys.exit(1)
    for f in files:
        analyze_file(f)
    print(f"\n{'='*70}\n分析完成。共 {len(files)} 个窗口。")

if __name__ == "__main__":
    main()
