#!/usr/bin/env python3
"""30ms 快照全维度分析."""
import pandas as pd
import numpy as np
import glob
from pathlib import Path

files = sorted(glob.glob("/tmp/snap30ms/snap_*.csv"))
print(f"Loading {len(files)} files...")
dfs = [pd.read_csv(f) for f in files]
df = pd.concat(dfs, ignore_index=True).sort_values("t_ms").reset_index(drop=True)
print(f"Total: {len(df):,} rows  span={((df.t_ms.max()-df.t_ms.min())/60000):.1f} min")
print()

# 过滤无效数据（Poly 未连接前）
df_v = df[(df.poly_up_ask > 0) & (df.poly_down_ask > 0)].copy()
print(f"Valid (poly connected): {len(df_v):,} rows ({100*len(df_v)/len(df):.1f}%)")
print()

# === 1. 市场状态分布 ===
print("=== 1. 市场状态分布 ===")
state_counts = df_v.market_state.value_counts()
for st, n in state_counts.items():
    print(f"  {st}: {n:,} ({100*n/len(df_v):.1f}%)")
print()

# === 2. BTC 偏离 K 的分布 ===
df_v['btc_K_offset'] = df_v.binance_mid - df_v.strike
print("=== 2. BTC 偏 K 分布 ===")
print(df_v.btc_K_offset.describe().round(1).to_string())
print()

# === 3. FV 分布 ===
print("=== 3. FV_up 分布 ===")
print(df_v.fv_up.describe().round(3).to_string())
print()
df_v['fv_zone'] = pd.cut(df_v.fv_up, bins=[0, 0.1, 0.3, 0.5, 0.7, 0.9, 1.0],
                         labels=['<0.1', '0.1-0.3', '0.3-0.5', '0.5-0.7', '0.7-0.9', '>0.9'])
print("FV_up 区间分布:")
for z, n in df_v.fv_zone.value_counts().sort_index().items():
    print(f"  {z}: {n:,} ({100*n/len(df_v):.1f}%)")
print()

# === 4. Poly ask 之和分布（mint 套利空间）===
df_v['ask_sum'] = df_v.poly_up_ask + df_v.poly_down_ask
print("=== 4. Ask 之和分布（< 1.0 = mint 套利存在）===")
print(df_v.ask_sum.describe().round(4).to_string())
print(f"  ask_sum < 1.00: {(df_v.ask_sum<1.00).sum():,} ({100*(df_v.ask_sum<1.00).sum()/len(df_v):.2f}%)")
print(f"  ask_sum < 1.01: {(df_v.ask_sum<1.01).sum():,} ({100*(df_v.ask_sum<1.01).sum()/len(df_v):.2f}%)")
print(f"  ask_sum < 1.02: {(df_v.ask_sum<1.02).sum():,} ({100*(df_v.ask_sum<1.02).sum()/len(df_v):.2f}%)")
print()

# === 5. FV-Poly gap 分布（chase 触发空间）===
df_v['poly_up_mid'] = (df_v.poly_up_bid + df_v.poly_up_ask) / 2
df_v['poly_down_mid'] = (df_v.poly_down_bid + df_v.poly_down_ask) / 2
df_v['gap_up'] = df_v.fv_up - df_v.poly_up_mid
df_v['gap_down'] = df_v.fv_down - df_v.poly_down_mid

print("=== 5. FV-Poly gap 分布（关键：chase 触发要 ≥ 10c）===")
print("gap_up describe:")
print(df_v.gap_up.describe().round(4).to_string())
print()
for thresh in [0.05, 0.07, 0.10, 0.15]:
    n_up = (df_v.gap_up >= thresh).sum()
    n_dn = (df_v.gap_down >= thresh).sum()
    print(f"  gap >= {thresh*100:.0f}c: UP={n_up:,} ({100*n_up/len(df_v):.2f}%)  DOWN={n_dn:,} ({100*n_dn/len(df_v):.2f}%)")
print()

# === 6. 双边 health 分布（avg_sum 锁定套利空间）===
print("=== 6. (best_ask_up + best_ask_dn) 健康度 ===")
hv = df_v.ask_sum.value_counts(bins=[0.99, 1.00, 1.005, 1.01, 1.015, 1.02, 1.05, 1.10]).sort_index()
for r, n in hv.items():
    print(f"  {r}: {n:,} ({100*n/len(df_v):.1f}%)")
print()

# === 7. Lead-Lag 分析：FV 移动 1c 后 Poly 多久跟上 ===
print("=== 7. Lead-Lag 分析：FV 移动 ≥ 1c 时 Poly 反应延迟 ===")
df_v['fv_diff_1s'] = df_v.fv_up.diff(periods=33).abs()  # 1 秒前 vs 现在
df_v['poly_diff_1s'] = df_v.poly_up_mid.diff(periods=33).abs()
mask = df_v.fv_diff_1s >= 0.01  # FV 1 秒内移动 ≥ 1c
movements = df_v[mask].head(50)
if len(movements) > 0:
    leads = []
    for i, row in movements.iterrows():
        # 找未来 5 秒内 poly_up_mid 跟上的时刻
        future = df_v.iloc[i:i+167]  # 167 ticks ≈ 5s
        target_poly = row.fv_up
        catch_idx = future[abs(future.poly_up_mid - target_poly) < 0.005].index
        if len(catch_idx) > 0:
            lead_ms = df_v.loc[catch_idx[0], 't_ms'] - row.t_ms
            leads.append(lead_ms)
    if leads:
        leads = np.array(leads)
        print(f"  样本数: {len(leads)}")
        print(f"  Poly 跟上 FV 的延迟: mean={leads.mean():.0f}ms, median={np.median(leads):.0f}ms")
        print(f"  P25={np.percentile(leads,25):.0f}ms, P75={np.percentile(leads,75):.0f}ms, P95={np.percentile(leads,95):.0f}ms")
print()

# === 8. 当前 chase 触发命中率（信号强度）===
# v0.4.12 触发：in_excited + gap >= 10c + fv_rising
df_v['in_excited'] = df_v.market_state == 'E'
print("=== 8. Chase 触发条件命中率 ===")
print(f"  in_excited (激变态): {df_v.in_excited.sum():,} ({100*df_v.in_excited.mean():.1f}%)")
ch_up = df_v.in_excited & (df_v.gap_up >= 0.10)
ch_dn = df_v.in_excited & (df_v.gap_down >= 0.10)
print(f"  excited & gap_up≥10c: {ch_up.sum():,} ({100*ch_up.mean():.2f}%)")
print(f"  excited & gap_dn≥10c: {ch_dn.sum():,} ({100*ch_dn.mean():.2f}%)")
print(f"  任何一边 chase 可触发: {(ch_up|ch_dn).sum():,} ({100*(ch_up|ch_dn).mean():.2f}%)")
print()

# === 9. 按窗口分组看 alpha 机会数 ===
print("=== 9. 各窗口 chase 触发次数（gap≥10c & 激变）===")
for win, g in df_v.groupby('window_end_ts'):
    g_excited = g.in_excited.sum()
    g_ch_up = (g.in_excited & (g.gap_up >= 0.10)).sum()
    g_ch_dn = (g.in_excited & (g.gap_down >= 0.10)).sum()
    btc_range = (g.binance_mid.max() - g.binance_mid.min())
    print(f"  win={win}: excited={g_excited:,}  ch_up={g_ch_up}  ch_dn={g_ch_dn}  BTC_range=${btc_range:.0f}")
