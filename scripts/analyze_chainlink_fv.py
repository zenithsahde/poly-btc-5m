#!/usr/bin/env python3
"""
analyze_chainlink_fv.py — offline analysis of verify_chainlink_fv.csv

Answers: does Chainlink-anchored FV track Polymarket better than Binance-anchored FV?
And how often / by how much do the two price sources disagree?

Inputs:
    CSV path as positional arg (default: verify_chainlink_fv.csv)

Output: text report to stdout.

Dependencies:
    pip install pandas
"""

import sys
from pathlib import Path

import pandas as pd


def fmt(x):
    if pd.isna(x):
        return "NA"
    if isinstance(x, float):
        return f"{x:.4f}"
    return str(x)


def section(title: str):
    print()
    print("=" * 72)
    print(f" {title}")
    print("=" * 72)


def describe(s: pd.Series, name: str, unit: str = ""):
    """Print mean / median / p99 / max-abs for a series."""
    s = s.dropna()
    if len(s) == 0:
        print(f"  {name:32s}  (no data)")
        return
    abs_s = s.abs()
    print(
        f"  {name:32s}  "
        f"n={len(s):>7d}  "
        f"mean={s.mean():+10.4f}  "
        f"med={s.median():+10.4f}  "
        f"|mean|={abs_s.mean():9.4f}  "
        f"p50|.|={abs_s.median():9.4f}  "
        f"p95|.|={abs_s.quantile(0.95):9.4f}  "
        f"p99|.|={abs_s.quantile(0.99):9.4f}  "
        f"max|.|={abs_s.max():9.4f}{unit}"
    )


def main():
    csv_path = sys.argv[1] if len(sys.argv) > 1 else "verify_chainlink_fv.csv"
    p = Path(csv_path)
    if not p.exists():
        print(f"ERROR: {csv_path} not found", file=sys.stderr)
        sys.exit(1)

    print(f"loading {csv_path} ...")
    df = pd.read_csv(csv_path)
    print(f"  rows = {len(df):,}")
    print(f"  cols = {list(df.columns)}")

    # Derive poly mid for direct FV comparison
    df["poly_up_mid"] = (df["poly_up_bid"] + df["poly_up_ask"]) / 2
    df["gap_chainlink_up"] = df["fv_chainlink_up"] - df["poly_up_mid"]
    df["gap_binance_up"] = df["fv_binance_up"] - df["poly_up_mid"]

    # Subset where every field needed for FV comparison is present
    valid = df.dropna(subset=["chainlink_price", "binance_price", "poly_up_bid", "poly_up_ask", "strike", "t_seconds_left"]).copy()
    valid = valid[valid["t_seconds_left"] > 0]
    print(f"  rows with all fields populated = {len(valid):,}")

    # ----- 1. Raw price source disagreement -----
    section("1. Chainlink ↔ Binance 原始价格差 (USD)")
    describe(valid["chainlink_minus_binance"], "CL − BI", unit=" USD")
    diff = valid["chainlink_minus_binance"].abs()
    if len(diff) > 0:
        for thresh in (5, 10, 20, 50, 100):
            pct = (diff > thresh).mean() * 100
            print(f"  |CL−BI| > ${thresh:<4d}      :  {pct:6.2f}% of snapshots")

    # ----- 2. FV vs Poly mid -----
    section("2. FV 与 Polymarket up_mid 的距离  (越接近 0 越准)")
    describe(valid["gap_chainlink_up"], "FV_chainlink − poly_up_mid")
    describe(valid["gap_binance_up"], "FV_binance   − poly_up_mid")

    # Which FV is closer per row?
    valid["chainlink_closer"] = valid["gap_chainlink_up"].abs() < valid["gap_binance_up"].abs()
    closer_pct = valid["chainlink_closer"].mean() * 100
    print()
    print(f"  Chainlink-FV 更接近 Poly  :  {closer_pct:6.2f}% of snapshots")
    print(f"  Binance-FV   更接近 Poly  :  {100 - closer_pct:6.2f}% of snapshots")

    # Mean-abs-error head-to-head
    mae_cl = valid["gap_chainlink_up"].abs().mean()
    mae_bi = valid["gap_binance_up"].abs().mean()
    print()
    print(f"  全体 MAE: Chainlink = {mae_cl:.4f}   Binance = {mae_bi:.4f}")
    if mae_cl < mae_bi:
        diff_pct = (mae_bi - mae_cl) / mae_bi * 100
        print(f"  → Chainlink 平均偏差比 Binance 小 {diff_pct:.2f}%  (站 Chainlink 一边)")
    else:
        diff_pct = (mae_cl - mae_bi) / mae_cl * 100
        print(f"  → Binance 平均偏差比 Chainlink 小 {diff_pct:.2f}%  (站 Binance 一边)")

    # ----- 3. Per-window breakdown -----
    section("3. 分窗口 (window_end_ts)")
    windows = valid["window_end_ts"].dropna().astype(int).unique()
    windows = sorted(windows)
    print(f"  {'window_end_ts':>14s}  {'rows':>7s}  {'|CL−BI|':>10s}  {'mae(CL)':>10s}  {'mae(BI)':>10s}  {'CL更近%':>8s}")
    for w in windows:
        sub = valid[valid["window_end_ts"] == w]
        if len(sub) < 100:
            continue
        cl_bi = sub["chainlink_minus_binance"].abs().mean()
        m_cl = sub["gap_chainlink_up"].abs().mean()
        m_bi = sub["gap_binance_up"].abs().mean()
        pct = sub["chainlink_closer"].mean() * 100
        print(f"  {w:>14d}  {len(sub):>7d}  {cl_bi:>10.4f}  {m_cl:>10.4f}  {m_bi:>10.4f}  {pct:>7.2f}%")

    # ----- 4. Lead-lag (raw price change correlation) -----
    section("4. 谁先动?  滞后秒数(正=Chainlink领先,负=Binance领先)")
    # Compute first-difference of each source, look at cross-correlation at small lags
    # Resample to ~100ms grid to reduce noise
    tmp = valid[["ts_ms", "chainlink_price", "binance_price"]].copy()
    tmp["ts"] = pd.to_datetime(tmp["ts_ms"], unit="ms")
    tmp = tmp.set_index("ts").resample("100ms").last().ffill().dropna()
    cl_diff = tmp["chainlink_price"].diff().dropna()
    bi_diff = tmp["binance_price"].diff().dropna()
    common = cl_diff.index.intersection(bi_diff.index)
    cl_diff = cl_diff.loc[common]
    bi_diff = bi_diff.loc[common]
    print(f"  100ms resampled samples: {len(cl_diff):,}")
    print(f"  {'lag (sec)':>10s}  {'corr(CL[t], BI[t+lag])':>25s}")
    for lag_ms in [-2000, -1000, -500, -200, 0, 200, 500, 1000, 2000]:
        n_steps = lag_ms // 100
        if n_steps == 0:
            c = cl_diff.corr(bi_diff)
        elif n_steps > 0:
            c = cl_diff.iloc[:-n_steps].corr(bi_diff.iloc[n_steps:].reset_index(drop=True).set_axis(cl_diff.iloc[:-n_steps].index))
        else:
            c = cl_diff.iloc[-n_steps:].reset_index(drop=True).set_axis(bi_diff.iloc[:n_steps].index).corr(bi_diff.iloc[:n_steps])
        print(f"  {lag_ms / 1000:>10.2f}  {c:>25.4f}")
    print("  正 lag 处相关性最高 → Chainlink 领先 Binance 那么多秒。")

    # ----- 5. Quick verdict -----
    section("5. 结论")
    print(f"  ① 两源原始价格平均 |差| = ${diff.mean():.2f}, 最大 ${diff.max():.2f}")
    print(f"  ② FV vs Poly:  {'Chainlink' if mae_cl < mae_bi else 'Binance'} 平均更准")
    print(f"     (MAE: CL={mae_cl:.4f}  BI={mae_bi:.4f})")
    print(f"  ③ {closer_pct:.1f}% 的快照 Chainlink-FV 更贴近 Poly")
    print()


if __name__ == "__main__":
    main()
