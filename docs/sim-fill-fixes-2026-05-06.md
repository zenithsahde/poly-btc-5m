# 模拟成交全修实证报告（v0.3.4-5m）

## 一、实验

- 二进制：`market-maker-5m@0.3.4-5m`
- 跑法：`HEADLESS=1 RUN_SECS=360 ./target/release/rust_engine`
- 时间：UTC 2026-05-06 17:53:30 → 17:59:30（6 分钟）
- 产出：1 个完整窗口 trades CSV (trades_1778090100.csv) + 2 个 fv_snapshots

## 二、修复对照表（v0.3.3 → v0.3.4）

| # | 疑点 | 旧版（v0.3.3） | 新版（v0.3.4） | 实证证据 |
|---|---|---|---|---|
| 1 | maker sell 触发 | `current_ask > price`（方向反） | `current_bid >= price` | 旧版 363 笔 0 sell；新版本窗口 9 笔含 6 sell |
| 2 | maker sell 挂价 | `best_ask`（队尾排队） | `best_bid + TICK`（主动型） | sell 价 0.30–0.39 紧贴 best_bid |
| 3 | taker fee | 写死 3%（高估 2–10×） | `qty × 0.072 × p × (1−p)` | 本窗口 0 taker 成交（无对照） |
| 4 | maker rebate | **完全漏算** | 25% × taker_fee | 本窗口 **+$0.111 rebate**（旧版 = $0） |
| 5/6 | 全量+无队列 | 100% 全成交假设 | `MAKER_FILL_PROBABILITY=0.5` 部分成交 | sell qty 出现 2.5/1.25/0.625/3.828 |
| 7 | 撤单延迟 | 瞬时撤单 | `CANCEL_DELAY_MS=200ms` 守门 + placed_ts | intent type `(f64,f64,i64)` |
| 8 | 5 张最低 | rebalance < 5 也挂 | `< MIN_QTY` 时 None | 本窗口仅整数 5 buy + 部分 sell |

## 三、本窗口完整 trades 流水

```
17:54:08  UP buy  maker p=0.60 q=5.00     ← 追涨触发
17:54:10  UP sell maker p=0.34 q=2.50     ← 浮亏触发, fill 50% (2.5)
17:54:10  UP sell maker p=0.33 q=1.25     ← 剩余 fill 50% (1.25)
17:54:10  UP sell maker p=0.35 q=0.625    ← 继续部分成交
17:54:11  UP sell maker p=0.35 q=0.312
17:54:15  UP buy  maker p=0.50 q=5.00     ← 第二次追涨
17:54:17  UP sell maker p=0.39 q=2.656    ← 浮亏触发
17:54:38  UP buy  maker p=0.44 q=5.00     ← 第三次追涨
17:54:39  UP sell maker p=0.30 q=3.828    ← 浮亏触发

最终持仓: UP qty=3.83, avg=0.463
Total Taker fee:  -$0.000  (本窗口无 taker 成交)
Total Maker rebate: +$0.111
Realized P&L:    -$2.153
Net (realized - fee + rebate): -$2.042
```

## 四、对比 v0.3.3（旧版）

```
旧版 v0.3.3 - 363 笔 fill / 10 个窗口：
  - 0 笔 sell（疑点 1 致命 bug：sell 永远不触发）
  - 0 maker rebate（疑点 4 漏算）
  - sum_qty 全是 5 倍数（疑点 5/6 全量成交假设）
  - 包含 < 5 张 rebalance 单（疑点 8 违反 Polymarket 最低）
```

实证铁证：旧版的 363 笔成交数据**全是错的**——不能用做策略评估。

## 五、新暴露的策略层问题（非模拟 bug）

实证看到 **realized P&L = −$2.15** 来自浮亏减仓。机制：

1. `chase_side=UP, buy avg=0.463, qty=10`（追涨买了 3 次×5=15 张）
2. 浮亏触发：`(0.463 - 0.30) / 0.463 = 35% > 20%` 阈值
3. 减仓挂 best_bid+TICK = 0.31（远低于 avg 0.463）
4. 触发：`best_bid >= 0.31` ✓
5. 锁定 ~50% 亏损

**根因**：浮亏减仓 trigger 价用 `best_bid + TICK` 在亏损深的时候是糟糕的卖点——本质上"在跌势的尾部低位减仓"。

**改进方向**（不在本次修复 scope）：
- 浮亏减仓应该用更高的目标价（如 `avg_price × 0.95`），等反弹再减
- 或加 max_drawdown 硬熔断（−10% 直接全平）
- 或减仓只在 chase_side 反转时触发（不再追涨 = 减仓信号）

## 六、TUI 新增 P&L 面板

仓位面板（高度 10→12）新增两行：

```
💰 浮盈=+0.0000  已实现=+0.0000  Taker费=-0.0000  Maker返佣=+0.0000
📊 净盈亏=+0.0000 USDC  (=浮+实-费+返)  追涨侧=UP
```

启动 TUI：

```bash
./target/release/rust_engine
# 或带文件日志：
RUST_LOG=info ENGINE_LOG_FILE=/tmp/engine.log ./target/release/rust_engine
```

## 七、可复现命令

```bash
cd /Users/zenith/btc-poly/market-maker-5m
rm -rf trades fv_snapshots
HEADLESS=1 RUN_SECS=360 RUST_LOG=info ./target/release/rust_engine
python3 scripts/analyze_lead_lag.py
# 手算经济模型见 docs/sim-fill-fixes-2026-05-06.md 的 Python 脚本
```
