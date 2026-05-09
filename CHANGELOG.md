# Changelog

## [0.4.5-5m] - 2026-05-08 (Taker-Only：抓 lead-lag alpha)

### 重大架构变更

**追涨/配平从 maker 改为 taker**：
- 旧（v0.4.4 及之前）：挂 maker buy @ best_ask − 1c，等卖盘下跌触发 fill
- 新（v0.4.5）：直接 taker 吃 best_ask，立即成交

### 底层逻辑

实证数据（forward-looking-fv.md）：FV 领先 Polymarket 1.1–2.5s

Maker 模式 fill 时机错位：
- Lead 窗口内（1.1-2.5s）poly 还没反应，maker buy 不会 fill（best_ask 没跌到我们价）
- Fill 时机大概率是反转/卖压（adverse selection）→ 我们买在恶化方向上

Taker 模式抓 alpha：
- BookTicker 触发瞬间立即吃 best_ask
- 在 lead 窗口内进场，1.1-2.5s 后 poly 反应到位 → 浮盈
- 牺牲 1c 价差 + taker fee（最大 1.56% @ p=0.5）换 3-5c lead alpha

### 改动

- 新常量 `TAKER_BUY_INTERVAL_MS = 1000`：同侧 taker buy 节流（防每帧 BookTicker 暴买）
- 新字段 `SignalEngine.last_taker_up_ts_ms / last_taker_down_ts_ms`
- BookTicker 处理：删 intent 创建 → 改成立即 `apply_fill(maker_taker=false)`
- PolyBookUpdate 处理：删 maker buy fill 检测块（约 30 行）
- `maker_buy_intent_up/down` 永远写入 None（保留字段防 type 改动，但已无意义）
- `MAKER_FILL_PROBABILITY` / `CANCEL_DELAY_MS` deprecated（但字段保留以兼容）

### 经济模型变化

- `total_fee` 现在会真实增加（之前全 maker 时一直 = 0）
- `total_rebate` 增长会停止（taker 没有 maker rebate）
- 单笔暴露：100 张 × $0.5 × 1.56% ≈ $0.78 fee/笔

### 守门保留

- MAX_BUY_PRICE = 0.85（防尾部）
- MIN_EXPIRY_MIN_FOR_CHASE = 0.5（末段不追）
- CHASE_GAP_MIN = 0.03（同时即 TAKER_THRESHOLD）
- MERGE_AVG_SUM_MAX / FORCE_MERGE_EXPIRY_MIN（merge 路径不变）

### 验证

- cargo build --release ok 22s
- cargo test --bins 4/4 通过

---

## [0.4.4-5m] - 2026-05-07 (CHASE_GAP_MIN 1c → 3c)

### 改动

- `signal.rs:41` `CHASE_GAP_MIN: 0.01 → 0.03`
  - 追涨门槛 FV-Poly gap 从 1 美分提到 3 美分
  - 防止振荡市每次 1c 噪声都触发追涨，导致单边裸仓累积

### 预期效果

- 追涨频率显著下降（v0.4.3 实测每窗口 5-10 笔 → 预期每窗口 1-3 笔）
- 每笔 entry 质量更高（FV 显示更强 edge 才追）
- 减少 v0.4.3 暴露的"反复振荡市单边累积裸仓"问题
- 但可能错过快速行情的小机会

### 验证

- cargo build --release ok 22s
- 4/4 测试通过
- 待实测 15min 看追涨频率变化 + 净盈亏

---

## [0.4.3-5m] - 2026-05-07 (P&L 统一跨窗口 + Redeem 模拟)

### 修复 — P&L 混合记账 bug

之前 `net_pnl` 公式混合了：
- 跨窗口累积字段（cash_paid / cash_received / merge_pnl）
- 单窗口字段（total_fee / total_rebate / inventory_value）

每次窗口切换 reset 时丢失前面所有窗口的 fee / rebate / 残仓结算结果，
导致 net_pnl **既不是单窗口也不是全周期** 的真实值。

### 改动

**`reset_inventory_for_new_window` 删除以下字段的归零**（保留为跨窗口累积）：
- `total_fee`
- `total_rebate`
- `realized_pnl`（已废弃但保留）

**新增 `settle_window_and_redeem(binance_close, ts_ms)`**：
1. Force-merge 所有可配对（数学等价 redeem，每对换 $1.00）
2. 残单边裸仓按 BTC 真实方向 redeem：
   - `binance_close >= strike` → UP 赢家 += qty × $1.00
   - 否则 → DOWN 赢家 += qty × $1.00
   - 输家边作废（不增 cash_received）
3. 调用顺序：`save_trades_for_window_and_clear` → `settle_window_and_redeem` → `reset_inventory_for_new_window`

**poly_client.rs 切换前调用** settle，用 `s.mid_price` 作 BTC close 价（与 Chainlink 有 ~$10 basis）。

### 影响

- `net_pnl` 现在反映**真实的全周期 USDC 现金流**
- 单边残仓不再被 reset 吞掉，赢家边按 $1.00/张计入
- 累计 fee / rebate 真实显示，不再每窗口归零

### TUI 文案

- "净盈亏" → "**累计净盈亏**"
- 注释："(全周期 cash+inv-fee+返, redeem 已计入)"

---

## [0.4.2-5m] - 2026-05-07 (P1.2: 窗口末段强制 merge)

### 新增

- 常量 `FORCE_MERGE_EXPIRY_MIN = 1.0`：距窗口结束 < 1 分钟时启动强制 merge 模式
- merge trigger 加 `force_merge` 路径：窗口末段绕过 `MERGE_AVG_SUM_MAX` 守门，全量 merge 已配对部分

### 底层逻辑

- 每对 merge = $1.00（CTF 合约级，立即锁定）
- 数学上等价于 redeem，但避免：
  - 残仓过夜带来的链上 gas（Polygon ~$0.001-0.01/tx，可忽略，但累积）
  - 清算时机延迟（Polymarket 结算后才能 redeem）
  - 单边裸仓在 avg_sum > 1 时 merge 锁亏 = 等价于"提前承认输家结算"

### 解决问题

v0.4.1 实测 Window 2 残留 UP 200 张孤悬，期末 avg_sum 极接近 1（守门拒 merge）。
P1.2 让最后 1 分钟把可配对部分全 merge 锁定 $1/对，残留单边裸仓再处理（P1.1 / P1.3 后续）。

### 验证

- cargo build --release ok
- cargo test --bins 4/4 通过
- 实测：v0.4.2 vs v0.4.1 同样 15min 跑，看 Window 2 类问题是否解决

---

## [0.4.1-5m] - 2026-05-07 (P0 三守门：尾部风险熔断)

### 背景

实测发现 100 张档下，0.98 价位 × 100 张 = $98 单笔暴露，赢拿 $2 / 输亏 $98（49:1 风险比），
一笔吞 40 个健康窗口的累积。3 窗口实测累计 −$47.22，99% 来自此单笔。

### 新增三守门常量

- `MAX_BUY_PRICE = 0.85`：单边追涨/配平挂价上限。> 0.85 时拒绝建仓（防尾部）
- `MIN_EXPIRY_MIN_FOR_CHASE = 0.5`：距窗口结束 < 30s 停止追涨（只 merge 不建仓）
- `MERGE_AVG_SUM_MAX = 1.0`：avg_sum > 1 时拒绝 merge（避免主动锁亏；等结算 redeem 兜底）

### 守门挂载点

- `signal.rs:497-510` 追涨条件加 `in_chase_window` 和 `price_up_ok`/`price_down_ok` 检查
- `signal.rs:535-549` intent_up/intent_down 创建条件加价格守门
- `signal.rs:715-725` merge trigger 加 `merge_avg_sum_ok` 守门

### 验证

- cargo build --release ok 23s
- cargo test --bins 4/4 通过
- 实测：next 5min 跑 0 笔 0.85+ 价位单（防灾确认）

---

## [0.4.0-5m] - 2026-05-07 (虚拟 merge 重构：废弃 sell, complete-set arbitrage)

### 模型重构

- **删除所有 sell 路径**：浮亏减仓在跌势末端低位卖出会锁定亏损，是错误的退出策略
- **新增虚拟 merge**：1 UP + 1 DOWN ≡ 1 USDC（Polymarket CTF mergePositions 任意时刻可调）
- **退出方式 sell → merge**：每对净利 = 1.00 - (avg_up + avg_down)，avg_sum<1 时套利盈利
- 详见 `docs/sim-fill-fixes-2026-05-06.md` + Polymarket CTF 文档

### 新增字段（AppState）

- `merged_pairs: f64`：累计已 merge 对数
- `merge_pnl: f64`：merge 实现盈亏
- `cash_received: f64`：merge 收到的 USDC
- `cash_paid: f64`：buy 累计花出的 USDC
- `last_merge_ts_ms: i64`：节流戳

### 新增方法

- `apply_merge(pair_qty, ts_ms)`：扣减两侧 qty，accrue merge_pnl/cash_received
- `mergeable_pairs() = min(qty_up, qty_down)`
- `cash_pnl() = cash_received - cash_paid`
- `inventory_value()`：可 merge 部分按 1.00/对，超出按 mid
- `net_pnl() = cash_pnl + inventory - taker_fee + maker_rebate` (v0.3.4 公式被替换)

### 删除（破坏性）

- 常量：`MAKER_SELL_RATIO`, `OVERWEIGHT_MIN_RATIO`, `FLOAT_LOSS_TRIGGER`
- 字段：`pending_maker_sell`
- 方法：`chase_side_float_loss_pct()`
- signal.rs 浮亏减仓挂卖块（约 25 行）
- signal.rs maker sell fill 检测块（约 15 行）
- TUI "Maker卖" / "浮亏≥20%" 显示行
- apply_fill 的 sell 分支（debug_assert + early return）

### 新增（Merge 触发器）

- 常量 `MERGE_TRIGGER_PAIR_QTY = 1.0`、`MERGE_INTERVAL_MS = 1000`
- signal.rs PolyBookUpdate 末尾加 merge trigger（节流 + ≥1 对就 merge）

### TUI 升级

- 仓位面板高度 12 → 14 行
- 新增显示行：`💰 现金 cash`（paid/received）/ `📦 库存 inventory` / `🔀 已 merge 对数 + 实现盈亏 + 可 merge 对数`
- 净盈亏公式改为 `cash + inventory - fee + rebate`

---

## [0.3.4-5m] - 2026-05-07 (模拟成交全修 + TUI 盈亏面板)

### 经济模型修复

- **疑点 3 修复**：Taker fee 从写死 3% 改为 Polymarket crypto 公式 `qty × 0.072 × p × (1−p)`（最大 1.56%@p=0.5）
- **疑点 4 修复**：新增 `total_rebate` 字段 + 25% maker rebate 计算（apply_fill maker 分支）
- 新增 `AppState::net_pnl()` = 浮盈 + 已实现 − Taker费 + Maker返佣

### 触发条件修复

- **疑点 1 修复**（致命方向错）：Maker sell 成交条件从 `current_ask > price` 改为 `current_bid >= price`，对齐 CLOB 标准
- **疑点 2 修复**：Maker sell 挂价从 `best_ask` 改为 `best_bid + TICK`（主动型 maker）

### 真实约束补齐

- **疑点 5/6 近似**：新增 `MAKER_FILL_PROBABILITY = 0.5`（队列优先 + 部分成交保守上界，详见注释）
- **疑点 7 修复**：新增 `CANCEL_DELAY_MS = 200`，intent tuple 加 `placed_ts`，fill 检测要求挂单老化 ≥200ms
- intent type `(f64, f64)` → `(f64, f64, i64)`，价/量未变时复用旧 placed_ts，避免每帧重置

### 最低订单约束

- **疑点 8 修复**：rebalance_qty < 5 时 intent 设 None，避免下单被 Polymarket 拒（5 张最低）

### TUI

- 仓位面板从 10 行扩到 12 行
- 新增 P&L 行：显示 浮盈 / 已实现 / Taker费 / Maker返佣
- 新增 净盈亏行（含返佣，醒目高亮）
- maker 卖提示文本对齐新触发条件

---

## [0.3.3-5m] - 2026-05-07 (实证闭环)

### 新增

- `src/strategy/fv_snapshots.rs`：every-frame 快照采集器，毫秒时间戳，按 5m 窗口轮转，保留 6 个窗口
- `src/main.rs`：`HEADLESS=1` + `RUN_SECS=N` 环境变量，跳过 TUI 用于数据采集
- `scripts/analyze_lead_lag.py`：cross-correlation lead-lag 分析（zero-dep Python）
- `scripts/plot_lead_lag.py`：ASCII bar chart 可视化（zero-dep）
- `docs/empirical-lead-lag-2026-05-06.md`：**首次实证报告**

### 实证结果

7 分钟运行，2 个 5m 窗口，43k+16k 行毫秒快照：
- **FV 领先 Poly_p 1100–2500ms**（cross-correlation peak）
- **FV-Poly 中位偏差 +0.37c / +0.10c**（共识对齐验证）
- 完美命中调研预测的 Streams 端到端滞后 [1s, 3s] 区间

---

## [0.3.2-5m] - 2026-05-07 (前瞻 FV — 修正版)

### 策略 (回滚 0.3.1 的粗暴方案，落定最终版)

- **撤回**：v0.3.1 用历史已实现波动率替代反解 IV——经评审被否决，因为这样 σ 数值与 Poly 共识脱钩，FV 偏离 Poly 太远导致追价无锚。
- **落定**：保留 IV 反解，但加 EMA 平滑（α=0.02，τ≈5s），分离两个时间常数：
  - **σ 慢响应**：σ_ema 来自历史 Poly 共识（市场对未来波动的预期），慢于 5s 不响应 Binance 瞬时跳动
  - **S 快响应**：FV 用 (S_now, K, T, σ_ema) 算，Binance 1–2s 领先信号在 d2 上完整体现
  - **数学保证**：当 Binance 跳 +$20 但 Poly 还没动时，iv_raw 瞬时下跌 → σ_ema 几乎不变 → d2 因 S 上升而上升 → FV 正确追涨
- 新字段 `SignalEngine.sigma_ema`，仅在稳态喂 EMA（激变态反解不可信，跳过）
- 详见 `docs/forward-looking-fv.md`（已修订）

## [0.3.1-5m] - 2026-05-07 (P0 - 已撤回)

- 移除稳态 IV 反解，改用历史已实现波动率——粗暴方案，仅作 thinking trail 保留

### 实证

- Polygon Chainlink BTC/USD Data Feed 实测更新 cadence: **min=32s, max=67s, avg=45.2s**
- Polymarket 5m 用 Data **Streams**（非 Feed），但仍有 ~1–3s 端到端滞后
- 5m 期权的 1–2s lead = 0.7–1.3% 总持仓时间 alpha，**核心利润来源**

---

## [0.3.0-5m] - 2026-05-07

### 重构 (15m → 5m fork)

- **窗口周期**：900s → 300s（main.rs / discovery.rs / poly_client.rs / scripts）
- **slug**：`btc-updown-15m-{ts}` → `btc-updown-5m-{ts}`
- **结构 / 函数命名**：`Active15mMarket` → `Active5mMarket`，`get_active_15m_btc_market` → `get_active_5m_btc_market`
- **fallback expiry**：15.0 → 5.0 分钟（signal.rs / tui/app.rs）

### 阈值重拟（√t 缩放）

- `STEADY_MOVE_5S_MAX`: $5 → **$3**（5m 内更敏感）
- `EXCITED_MOVE_1S_MIN`: $50 → **$30**
- `LEAD_TIMEOUT_MS`: 30s → **10s**（5m 总长不允许 30s 等待）
- `BUFFER_MAX_MINUTES`（volatility）: 20 → **8**
- 价格类阈值（POLY_SPREAD_NARROW_MAX / CATCHUP_EPSILON / CHASE_GAP_MIN）保持不变

### 新增文档

- `docs/precision-and-basis.md`：调研 Polymarket 5m 用 Chainlink BTC/USD 结算（非 Binance），量化 USDT-USD basis 风险，给出 P0–P3 改造路径

### 验证

- `cargo build --release` ok（1m01s）
- `cargo test --bins` 4/4 BS 模型测试通过

---

## [0.2.0] - 2025-02-22

### 策略

- **只在激变态追涨**：追涨挂单仅在激变态（`!steady`）下进行；稳态时清空 `chase_side`、不挂追涨单，避免无波动时误建仓。
- 显式 `CHASE_ONLY_IN_EXCITED` 与 `in_excited` 条件，便于维护与后续配置化。

### 其他

- 成交日志保持不变：本窗口内存 `trades_current_window`，换窗口落盘 `trades/trades_{window_end_ts}.csv`，保留最新 10 份。

---

## [0.1.0]

- 初始版本：Binance WebSocket + Poly 5m 做市引擎、稳态/激变态、追涨与配平意图、Maker 买模拟成交与落盘。
