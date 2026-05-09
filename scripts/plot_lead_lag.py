#!/usr/bin/env python3
"""ASCII bar chart of cross-correlation curve over lag (zero-dep)."""
import sys, glob, csv

def load_csv(path):
    rows = []
    with open(path) as f:
        rdr = csv.DictReader(f)
        for r in rdr:
            try:
                rows.append({
                    "t_ms": int(r["t_ms"]),
                    "fv_up": float(r["fv_up"]),
                    "poly_up_bid": float(r["poly_up_bid"]),
                    "poly_up_ask": float(r["poly_up_ask"]),
                    "poly_down_bid": float(r["poly_down_bid"]),
                    "poly_down_ask": float(r["poly_down_ask"]),
                })
            except (ValueError, KeyError):
                continue
    rows.sort(key=lambda x: x["t_ms"])
    return rows

def poly_up_mid(r):
    a, b = r["poly_up_bid"], r["poly_up_ask"]
    if a > 0 and b > 0: return (a + b) / 2.0
    a2, b2 = r["poly_down_bid"], r["poly_down_ask"]
    if a2 > 0 and b2 > 0: return 1.0 - (a2 + b2) / 2.0
    return None

def resample(rows, step_ms=100):
    if not rows: return []
    t0 = rows[0]["t_ms"]; t1 = rows[-1]["t_ms"]
    grid = []
    i = 0
    last_fv, last_poly = None, None
    t = t0
    while t <= t1:
        while i + 1 < len(rows) and rows[i + 1]["t_ms"] <= t:
            i += 1
        r = rows[i]
        fv = r["fv_up"] if r["fv_up"] > 0 else last_fv
        pm = poly_up_mid(r) or last_poly
        if fv is not None: last_fv = fv
        if pm is not None: last_poly = pm
        if last_fv is not None and last_poly is not None:
            grid.append((t, last_fv, last_poly))
        t += step_ms
    return grid

def corr(x, y):
    n = len(x)
    if n < 2: return 0.0
    mx = sum(x) / n; my = sum(y) / n
    sxy = sum((x[i] - mx) * (y[i] - my) for i in range(n))
    sxx = sum((x[i] - mx) ** 2 for i in range(n))
    syy = sum((y[i] - my) ** 2 for i in range(n))
    d = (sxx * syy) ** 0.5
    return sxy / d if d > 0 else 0.0

def cross_corr(grid, max_lag, step):
    fv = [g[1] for g in grid]; pp = [g[2] for g in grid]
    n = len(fv); res = []
    for lag in range(-max_lag, max_lag + 1):
        if lag >= 0:
            x = fv[:n - lag]; y = pp[lag:]
        else:
            x = fv[-lag:]; y = pp[:n + lag]
        res.append((lag * step, corr(x, y)))
    return res

def ascii_bar_chart(ccs, width=60):
    if not ccs: return
    cmin = min(c for _, c in ccs); cmax = max(c for _, c in ccs)
    span = max(abs(cmin), abs(cmax), 1e-6)
    print(f"\n  Cross-correlation FV(t) vs Poly_p(t+lag)")
    print(f"  正 lag = FV 领先 Poly_p")
    print(f"  range: [{cmin:+.3f}, {cmax:+.3f}]")
    print(f"  {'lag(ms)':>8} | {'corr':>8} | bar")
    print(f"  {'-'*8}-+-{'-'*8}-+-{'-'*width}")
    for lag, c in ccs:
        n_chars = int(round(c / span * (width - 2)))
        if n_chars >= 0:
            bar = " " * (width // 2) + "█" * n_chars
        else:
            bar = " " * (width // 2 + n_chars) + "█" * (-n_chars)
        marker = " ← BEST" if c == max(cc for _, cc in ccs) else ""
        print(f"  {lag:>+8d} | {c:>+8.4f} | {bar}{marker}")

def main():
    files = sys.argv[1:] or sorted(glob.glob("fv_snapshots/fv_*.csv"))
    if not files:
        print("ERROR: no fv_*.csv files")
        sys.exit(1)
    for f in files:
        print(f"\n{'='*72}\n{f}\n{'='*72}")
        rows = load_csv(f)
        if len(rows) < 100:
            print(f"  too few rows ({len(rows)})")
            continue
        grid = resample(rows, step_ms=100)
        print(f"  rows={len(rows)}  grid_points={len(grid)}  span={(rows[-1]['t_ms']-rows[0]['t_ms'])/1000:.1f}s")
        if len(grid) < 100: continue
        # 5s scan window
        ccs = cross_corr(grid, max_lag=50, step=100)
        ascii_bar_chart(ccs, width=60)

if __name__ == "__main__":
    main()
