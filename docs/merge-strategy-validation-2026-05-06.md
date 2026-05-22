# Merge 策略实证：从 sell 灾难到套利盈利（v0.3.4 → v0.4.0）

## 一、对比铁证

| 指标 | v0.3.4 (sell 模型) | v0.4.0 (merge 模型) | 改善 |
|---|---|---|---|
| 5min 窗口净盈亏 | **−$2.04** | **+$0.41** | **+$2.45** |
| Sell 笔数 | 6 笔（在低位锁亏） | **0**（全删） | 灾难根源消除 |
| 双边持仓 | 单边追涨为主 | **3 UP + 2 DOWN** | 配平回归 |
| Merge 对数 | 0 | **10 对**（2 次触发） | 套利路径打开 |
| Merge 盈利 | 0 | **+$0.40** | 新利润来源 |

## 二、v0.4.0 逐笔实测重放

```
窗口：2026-05-06 18:22-18:27 UTC
配置：MERGE_TRIGGER_PAIR_QTY=1.0, MERGE_INTERVAL_MS=1000

笔1  18:22:01  UP   @ 0.550 × 5  → 持仓 UP=5, DOWN=0  cash_paid=$2.75
笔2  18:23:37  DOWN @ 0.390 × 5  → 持仓 UP=5, DOWN=5  cash_paid=$4.70
       🔀 MERGE 5 对  avg_sum=0.94  Δpnl=+$0.30  cash_received=$5.00
笔3  18:23:37  DOWN @ 0.380 × 5  → 持仓 UP=0, DOWN=5  cash_paid=$6.60
笔4  18:23:54  UP   @ 0.600 × 5  → 持仓 UP=5, DOWN=5  cash_paid=$9.60
       🔀 MERGE 5 对  avg_sum=0.98  Δpnl=+$0.10  cash_received=$10.00
笔5  18:24:19  UP   @ 0.520 × 5  → 持仓 UP=5, DOWN=0  cash_paid=$12.20

终态：
  持仓:    UP qty=5 avg=0.520, DOWN qty=0
  现金:    paid -$12.20, received +$10.00, cash_pnl = -$2.20
  库存:    +$2.50（残仓 UP 5 张按 mid≈0.5 估值）
  已 merge:10 对, merge_pnl = +$0.40
  Taker费: -$0.00 (全 maker)
  Maker返佣: +$0.109
  📊 净盈亏: +$0.41 USDC
```

## 三、套利数学验证

每次 merge 锁定：`pair_qty × (1.00 − avg_sum)`

```
Merge #1: 5 × (1.00 − (0.55 + 0.39)) = 5 × 0.06 = +$0.30
Merge #2: 5 × (1.00 − (0.60 + 0.38)) = 5 × 0.02 = +$0.10
Total merge_pnl = +$0.40 ✓ (与运行结果一致)
```

**关键观察**：
- 两次 merge 的 avg_sum 都 < 1.00 → 都是无风险套利
- avg_sum 接近 1.00 时（0.98）利润空间小（2c/对）
- avg_sum 离 1.00 越远利润越大（0.94 给 6c/对）
- 这就是 FV 领先信号的直接货币化——在 Polymarket 还没反映 BTC 移动时抢挂

## 四、底层逻辑闭环

```
                ┌──────────────┐
   Binance      │    FV 引擎    │   ← BS + smoothed-IV，FV 领先 Poly_p 1.1-2.5s
   现货 ────→  │  (signal.rs) │
                └──────┬───────┘
                       │ FV 领先方向
                       ↓
                ┌──────────────┐
                │  追涨 maker  │   ← 抢挂 best_ask−1c, 等价 50% 部分成交
                │  buy intent  │
                └──────┬───────┘
                       │ 累积 (UP, DOWN) 双侧库存
                       ↓
                ┌──────────────┐
                │  虚拟 merge  │   ← min(qty_up, qty_down) ≥ 1 触发，1s 节流
                │  pair → USDC │
                └──────┬───────┘
                       │ 每对锁定 1 − avg_sum 套利
                       ↓
                  📊 cash_pnl + inventory + rebate − fee
```

## 五、TUI P&L 面板（v0.4.0）

仓位面板高度 12→14 行，新布局：

```
📊 仓位
  UP:   Q=5.00  P_avg=0.52  浮盈/亏=-0.10
  DOWN: Q=0.00  P_avg=0.00  浮盈/亏=+0.00
  均价之和=0.52  挂单若成交=0.97  (应<1)  偏仓=UP多
  💰 现金 cash=-2.20 (paid -12.20, received +10.00)
  📦 库存 inventory=+2.50  Taker费=-0.0000  Maker返佣=+0.1090
  🔀 已 merge 对数=10.00  merge 实现盈亏=+0.4000  可 merge 对数=0.00
  📊 净盈亏=+0.4090 USDC  (cash+inv-fee+返)  追涨侧=DOWN
  挂单 Maker买: UP @ 0.45×5  DOWN @ 0.49×5
  配平 UP需—  DOWN需—
```

## 六、可复现命令

```bash
cd /Users/zenith/btc-poly/market-maker-5m
rm -rf trades fv_snapshots

# Headless 数据采集
HEADLESS=1 RUN_SECS=360 RUST_LOG=warn ENGINE_LOG_FILE=/tmp/engine.log \
  ./target/release/rust_engine

# 看 trades CSV (只 buy，无 sell)
cat trades/trades_*.csv

# Lead-lag 实证（旧分析仍有效）
python3 scripts/analyze_lead_lag.py
```

## 七、后续 scope

P0 已完成：
- ✅ Sell 全路径删除
- ✅ Virtual merge 实现
- ✅ TUI P&L 面板重构
- ✅ 经济模型重新框定

P1（下一轮）：
- merge 事件落盘到独立 `merges_xxx.csv`（带 audit trail）
- 窗口末段强制 merge 所有可配对（避免持仓过夜）
- 残仓 redeem 模拟（5min 结束 Chainlink 报赢家边）

P2（生产化）：
- 真链上 `mergePositions` 调用（execution 模块）
- Polymarket CLOB EIP-712 真挂单（替换模拟成交）
