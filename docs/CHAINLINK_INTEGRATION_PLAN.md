# Chainlink + JumpChase 集成计划

## 背景

数据已证（5 窗口 220 事件）：
- 币安 1s ±$30 跳变 → chainlink 5s 内 pass-through 中位 1.26×，97% 同向
- 即"币安先动 + chainlink 跟"是真实领先关系

Polymarket 自己写明（gamma API `resolutionSource`）：
- K = chainlink price @ window_start
- S = chainlink price @ window_end
- 全部用 chainlink 结算

引擎现状（扫描确认）：
- **完全没有 chainlink 接入**（`grep -r chainlink engine/src` 只命中 1 处 `poly_client.rs:299` 的注释）
- K 用 binance 现价代算（`poly_client.rs:285-325`，调 `binance_rest::get_spot_price_at_time`）
- S 用 binance mid（`fv.rs:19 binance_mid`）
- 结算 PnL 用 `s.mid_price`（`poly_client.rs:302-303`），注释承认 "~$10 basis 误差"

## 目标

把 chainlink 接进引擎，让 K / S / 结算价全部对齐 Polymarket 实际结算源；
加入 binance jump 作为 FV 提前修正（不是预测信号，而是知道 chainlink 即将动）。
修掉 `PAIR_HEALTH_MAX 1.05 ↔ REBAL_HEALTH_MAX 0.98` 的 trap zone bug。

## 改动清单（按依赖顺序）

### Phase 1 — 低风险快速修复（不依赖 chainlink）

| # | 文件 | 行 | 改动 |
|---|---|---|---|
| 1.1 | `engine/src/strategy/signal.rs` | 72 | `PAIR_HEALTH_MAX: 1.05 → 1.02`；改注释说明与 0.98 的对齐 |
| 1.2 | `engine/src/position.rs` | 21-24 | `PendingOrderReason` 加 `JumpChase` 变体 |
| 1.3 | `engine/src/position.rs` | 26-31 | `reason_label` 加 `JumpChase => "jump_chase"` 分支 |
| 1.4 | `engine/src/tui/ui.rs` | ~200 | `pending_reason_label` 加分支 |
| 1.5 | `engine/src/tui/ui.rs` | 67-80 | `IocStats::from_orders` 加 `jump_chase_orders/filled` 统计字段 |

### Phase 2 — Chainlink 接入（新模块）

| # | 文件 | 类型 | 改动 |
|---|---|---|---|
| 2.1 | `engine/src/ws/chainlink_ds.rs` | **新建** | ~350 行：HMAC-SHA256 签名、WS 连接、REST get_report_at、ABI 解码 ReportDataV3 |
| 2.2 | `engine/src/model/chainlink.rs` | **新建** | ~30 行：`ChainlinkPriceData { price, observation_ts, bid, ask, recv_ts_ms }` |
| 2.3 | `engine/src/ws/mod.rs` | 改 | `pub mod chainlink_ds;` `pub mod chainlink;`（model 子模块） |
| 2.4 | `engine/src/model/mod.rs` | 改 | `pub mod chainlink;` |
| 2.5 | `engine/src/ws/stream.rs` | 8-28 | `MarketEvent` 加 `ChainlinkPrice { data, recv_ts_ns }` 变体 |
| 2.6 | `engine/src/config.rs` | 改 | 加 `ChainlinkConfig { api_key, api_secret, ws_endpoint, rest_endpoint, feed_id, enabled }`；ENV 读取 |
| 2.7 | `config/default.toml` | 改 | 加 `[chainlink]` 段，feed_id 默认值，URL |
| 2.8 | `engine/src/main.rs` | 改 | spawn `chainlink_task`；接 `event_tx` 广播 |

### Phase 3 — 让引擎用 chainlink

| # | 文件 | 行 | 改动 |
|---|---|---|---|
| 3.1 | `engine/src/tui/app.rs` | AppState 字段 | 加 `chainlink_price: Option<f64>`, `chainlink_obs_ts: Option<i64>`, `chainlink_recv_ts_ms: Option<i64>`, `basis_vs_chainlink: Option<f64>` |
| 3.2 | `engine/src/strategy/signal.rs` | run | 加 `MarketEvent::ChainlinkPrice` 处理分支，写入 AppState；维护一份本地 `chainlink_history` |
| 3.3 | `engine/src/strategy/fv.rs` | 19 | `FvInputs::binance_mid` 改名为 `spot_price`（语义） + 加 `binance_mid` 字段（保留用于 jump 检测） |
| 3.4 | `engine/src/strategy/signal.rs` | FV 调用处 | 传入 chainlink_price 作 `spot_price`（fallback to binance mid 仅当 chainlink 未就绪） |
| 3.5 | `engine/src/ws/poly_client.rs` | 285-325 | strike 改用 `chainlink_ds::get_report_at(window_start_ts)`（带 fallback 到 binance） |
| 3.6 | `engine/src/ws/poly_client.rs` | 302-303 | settle 改用 `s.chainlink_price.unwrap_or(s.mid_price)`，注释更新 |

### Phase 4 — JumpChase 触发器（消费 chainlink + binance jump）

| # | 文件 | 行 | 改动 |
|---|---|---|---|
| 4.1 | `engine/src/strategy/signal.rs` | constants 区 | 加 `JUMP_USD_THRESHOLD: 30.0`, `JUMP_LOOKBACK_MS: 1000`, `JUMP_CONFIRM_MS: 800`, `JUMP_PROBE_QTY: 5.0`, `JUMP_ADD_QTY: 15.0` |
| 4.2 | `engine/src/strategy/signal.rs` | SignalEngine 结构体 | 加 `last_jump_ts_ms: i64`, `last_jump_dir: Option<i8>` |
| 4.3 | `engine/src/strategy/signal.rs` | run / handle_book_ticker | 每帧检测 1s lookback 内 `Δbinance_mid >= 30`，写入 state.binance_jump_1s + 设 last_jump_* |
| 4.4 | `engine/src/strategy/decision.rs` | build_intents | 加 `up_jump / down_jump` 触发；BuyIntent reason 加 `JumpChase` 分支；qty 用 `JUMP_PROBE_QTY`；worst = target + 4c |
| 4.5 | `engine/src/strategy/signal.rs` | Thresholds 装填处 | 加 `jump_threshold_usd`、`jump_active`、`basis_vs_chainlink` 等传递 |

### Phase 5 — TUI 显示

| # | 文件 | 改动 |
|---|---|---|
| 5.1 | `engine/src/tui/ui.rs` `render_fair_value_panel` | 显示 chainlink_price、basis_vs_chainlink、binance_jump_1s、last_jump_age |
| 5.2 | `engine/src/tui/ui.rs` `IocStats` 显示 | 加 jump_chase 列 |
| 5.3 | `engine/src/web/snapshot.rs` | 加 chainlink + jump 字段供 web UI 拉 |

## 测试与验证

| 测试 | 怎么验 |
|---|---|
| Chainlink WS 连通 | 启动后 5s 内 TUI 应显示 chainlink_price，basis 在 ±$200 内 |
| Strike 对齐 | 新窗口翻滚后 strike_price 应等于 chainlink_ds::get_report_at(window_start)，**不是** binance 现价 |
| Jump 触发 | 制造（或等到）真实 ±$30/s 跳变，TUI 应显示 last_jump_age ≤ 1000ms |
| JumpChase 下单 | 在 paper-trade 模式跑 1 小时，统计 jump_chase 命中率（应 > 0） |
| 结算对齐 | 窗口结束时 cash_received 应基于 chainlink_close vs strike（不再 ~$10 basis） |
| trap zone 不再亏 | 跑 3 天后 IocStats 里 cancel reason 不再出现 "pair_health_breach" 同时配平腿打不通的情况 |

## 风险与回退

- Phase 2 chainlink_ds.rs 写错（HMAC / ABI）→ WS 连不上 → fallback：仍用 binance（state.chainlink_price = None）
- Phase 4 JumpChase 误触发频繁 → 加 `JUMP_DAILY_MAX_COUNT` 限频 + 单笔小仓兜底
- 部署：先 D+0 灰度只跑 Phase 1+2+3，shadow mode 跑 1 天再开 Phase 4
- Kill switch: `touch /tmp/kill-engine` 即停（main.rs 加监听）

## 提交策略

1 个 commit 一个 Phase，message:
- `fix(decision): tighten PAIR_HEALTH_MAX 1.05→1.02 to seal trap zone`
- `feat(position): add JumpChase variant to PendingOrderReason`
- `feat(ws): add Chainlink Data Streams client (HMAC + ABI)`
- `feat(strategy): switch FV spot price source to chainlink`
- `feat(poly_client): use chainlink for strike + settle`
- `feat(decision): add JumpChase branch on binance ±$30/1s`
- `feat(tui): display chainlink price + basis + jump state`
