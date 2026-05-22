# poly-btc-5m — Jump Capture 引擎 (v0.6)

Polymarket BTC 5-minute up-down 二元期权的 **taker** 交易引擎，基于
**binance 价格跳变** 触发 + **chainlink 结算源对齐** 的双向 jump capture 模型。

## 核心策略（v0.6 唯一路径）

```
事件: binance mid 1 秒内变化 |Δ| ≥ $15
       ↓
判定方向: +15 ⇒ UP，-15 ⇒ DOWN
       ↓
守门:
  - 剩余时间 1.5 min ≤ expiry < 4.5 min
  - poly 该侧 best_ask > 0 且 ≤ 0.65
  - force-balance: 我方持仓 ≤ 对侧持仓
  - 同侧 1s 节流
  - 末段（< 1 min）方向锁定：binance vs strike $30+ 同向才放行
  - pair_health: fill 后 avg_sum < 1.02（末段动态放宽到 1.20）
       ↓
执行: IOC buy 15 张 @ best_ask 精确 (worst = best_ask, 只吃一档)
       ↓
持仓: hold to settle，不挂 SELL、不主动平
       ↓
窗口结束:
  - 双向都有持仓 → apply_merge 配对 → 每对收 $1
  - 单边残仓 → settle_window_and_redeem → 赢家 redeem $1 / 输家 redeem $0
```

**赚钱底层逻辑**：binance 双向震荡时 UP / DOWN 都触发 → 积累两边仓位 → 平均
进价之和 < $1（典型 avg_sum ≈ 0.78）→ apply_merge 锁结构性套利每对 $1−0.78 = $0.22。
**不是押方向预测**，是用 jump 触发器自然积累双向仓位。

实测：v0.6 单进程 28h 净盈利 **+$437.04**（cash_paid $1,316，cash_received $1,799，
merge_pnl +$158，merged_pairs 798）。

## 数据流（4 个 WS task）

| 源 | 用途 | 模块 |
|---|---|---|
| **Binance WS** (`bookTicker / depth / aggTrade`) | jump 探测 + σ 计算（aggTrade trades） | `ws/client.rs` |
| **Polymarket WS** (`/ws/market`) | UP/DOWN 订单簿 + 窗口翻滚发现 | `ws/poly_client.rs` |
| **Chainlink Data Streams WS** (`ws.dataengine.chain.link`) | 结算源实时价；strike (REST `/api/v1/reports`) | `ws/chainlink_ds.rs` |
| **Deribit REST** (`get_book_summary_by_currency`) | BTC 期权 IV smile (前瞻 σ 源)，60s 一拉 | `strategy/deribit.rs` |

## 模块结构

```
engine/src/
  main.rs                      # 入口：spawn 4 个 WS task + signal/snap30 task
  config.rs                    # 配置（TOML + env overlay）
  position.rs                  # PositionLedger / apply_fill / apply_merge / settle_redeem
  strategy/
    signal.rs                  # 主循环：BookTicker → jump 探测 → FV → decision::build_intents → IOC
    decision.rs                # 纯函数：jump 守门链 + BuyIntent
    fv.rs                      # BS-binary FV (S = chainlink + 5s jump correction)
    bs_model.rs                # Black-Scholes 二元期权定价
    deribit.rs                 # Deribit IV smile 拉取 + 插值
    volatility.rs              # Binance aggTrade realized vol (备用 σ)
    fv_snapshots.rs            # 全量 FV 快照 CSV
    excited_snapshots.rs       # 激变态事件快照 CSV
    snap30ms.rs                # 30ms 等间隔时序 CSV
  ws/
    binance_rest.rs            # Binance REST (K 线 fallback)
    chainlink_ds.rs            # Chainlink Data Streams (HMAC-SHA256 + ABI 解码)
    client.rs                  # Binance WS 客户端
    discovery.rs               # Polymarket gamma API 市场发现
    poly_client.rs             # Polymarket CLOB WS 客户端 + 窗口翻滚
    reconnect.rs               # 通用指数退避重连
    stream.rs                  # MarketEvent 枚举（BookTicker/Depth/AggTrade/PolyBook/Chainlink）
  execution/
    actor.rs                   # 下单 channel (生产环境用)
    ioc.rs                     # IOC 走簿（v0.6 设 worst=best_ask 退化为只吃一档）
    poly_api.rs                # Polymarket CLOB REST 下单
    signer.rs                  # EIP-712 订单签名 (BUY 0 / SELL 1)
    order_manager.rs           # 订单生命周期追踪
  model/                       # 各类数据结构 (ticker / trade / orderbook / chainlink)
  tui/                         # ratatui TUI 面板
  web/                         # axum web 服务器 + SSE (--web 模式)
  metrics/                     # 延迟统计 (HdrHistogram)

web-ui/                        # Leptos CSR 仪表盘 (WASM)
shared-types/                  # 前后端共享 serde struct
docs/
  v0.6-jump-capture-results.md # v0.6 设计与实盘结果
  v0.6-dead-code-inventory.md  # v0.5 → v0.6 清理清单
config/default.toml
scripts/                       # Python 验证器 + 离线分析
```

## 构建与运行

```bash
# 构建
cargo build --release

# 必需环境变量
export CHAINLINK_DS_API_KEY=<your-key>
export CHAINLINK_DS_API_SECRET=<your-secret>
# 可选
export CHAINLINK_DS_FEED_ID_BTCUSD=0x00039d9e45394f473ab1f050a1b963e6b05351e52d71e507509ada0c95ed75b8
export RUST_LOG=info

# 运行模式
./target/release/rust_engine                              # TUI 模式
./target/release/rust_engine --web --host 0.0.0.0 --port 3000  # web 模式
HEADLESS=1 ./target/release/rust_engine                   # 无界面后台运行
```

**Chainlink 未启用时**：引擎自动 fallback —— K 改用 Binance 窗口开始秒级成交价，
settle 改用 Binance mid。功能完整但精度下降，**不建议生产环境跑无 chainlink 版本**。

## 主要常量（`engine/src/strategy/signal.rs`）

| 常量 | 值 | 说明 |
|---|---|---|
| `JUMP_USD_THRESHOLD` | 15.0 | 1s 内 binance mid 变化阈值 |
| `JUMP_LOOKBACK_MS` | 1000 | jump 回看窗口 |
| `JUMP_CONFIRM_MS` | 5000 | jump 信号有效时长（与 spot_s decay 对齐） |
| `JUMP_CHASE_QTY` | 15 | 单笔基础张数 |
| `MAX_BUY_PRICE` | 0.85 | 兜底价格上限 |
| `PAIR_HEALTH_MAX` | 1.02 | 静态健康守门，末段放宽到 1.20 |
| `MIN_EXPIRY_MIN_FOR_CHASE` | 1.5 | jump 窗口下界（剩余分钟） |
| `MIN_ORDER_QTY` | 5.0 | Polymarket 最低张数 |
| `TAKER_BUY_INTERVAL_MS` | 1000 | 同侧节流 |
| `MERGE_TRIGGER_PAIR_QTY` | 1.0 | merge 触发对数 |
| `MERGE_INTERVAL_MS` | 1000 | merge 节流 |

`decision.rs` 内：`MAX_ENTRY_PRICE_JUMP = 0.65`，`MAX_EXPIRY_MIN_JUMP = 4.5`，
`LOCKIN_WINDOW_MIN = 1.0`，`LOCKIN_USD_THRESHOLD = 30.0`，`LOCKIN_PAIR_HEALTH_MAX = 1.20`。

## 输出落盘

```
trades/trades_{window_end_ts}.csv       # 每窗口成交记录
orders/orders_{window_end_ts}.csv       # 每窗口订单生命周期
fv_snapshots/fv_{window_end_ts}.csv     # 每 BookTicker/PolyBook 的 FV 快照
excited_snapshots/exc_{window_end_ts}.csv  # 激变态事件快照
snapshots_30ms/snap_{window_end_ts}.csv # 30ms 等间隔时序
engine.log                              # 运行日志（仅 --web/--headless 模式）
```

CSV 同窗口归档保留最近 10 个，自动滚动。

## 部署（爱尔兰服务器示例）

```bash
# systemd-run 后台运行（不依赖 SSH 会话）
loginctl enable-linger ubuntu   # 一次性
systemd-run --user --unit=poly-engine \
  --setenv=RUST_LOG=info \
  --setenv=CHAINLINK_DS_API_KEY=... \
  --setenv=CHAINLINK_DS_API_SECRET=... \
  --working-directory=/home/ubuntu/poly-btc-5m \
  /home/ubuntu/poly-btc-5m/target/release/rust_engine --web --host 0.0.0.0 --port 3000

# 查状态
systemctl --user status poly-engine
journalctl --user -u poly-engine -f
```

## 版本

- **0.6**：纯 jump capture 重构。砍掉 chase / rebal / SELL；只保留 binance ±$15 jump → IOC buy → hold to settle。force-balance 守门保留。Deribit smile σ + Chainlink K/S/settle 三件套对齐 Polymarket 实际结算源。
- 0.5.x：chase + rebal + jump 三路径并存。trap zone 修复、Deribit smile 接入、chainlink 集成。
- 0.4.x：FV 模型 + force-balance + pair_health 守门。
- 0.2.0：稳态/激变态判定。
- 0.1.0：初版 FV + 模拟成交。

详见 [CHANGELOG.md](CHANGELOG.md) 与 `docs/`。

## 重要文档

- `docs/v0.6-jump-capture-results.md` — v0.6 设计意图、实盘结果、教训反思
- `docs/v0.6-dead-code-inventory.md` — v0.5 → v0.6 清理清单
- `docs/forward-looking-fv.md` — FV 算法 + σ 来源
- `docs/empirical-lead-lag-2026-05-06.md` — binance/chainlink/poly 实测延迟
- `docs/merge-strategy-validation-2026-05-06.md` — merge 套利数学验证

## 已知约束 & 风险

- **延迟**：爱尔兰 → Polymarket (美东) 单程 RTT ~100ms。极短窗口 jump 抢跑机会会输给美东 co-located bot。
- **无 SELL**：进 IOC 后只能 hold to settle，押错方向 = 全亏 cost。靠 force-balance + 末段方向锁定 + max_entry_price 0.65 兜底。
- **calm market**：BTC 波动减弱 → jump 频率下降 → 配对机会减少 → PnL 跟随下降。
- **Polymarket fee**：maker=taker 同费率（约 7.2% × p(1−p)），无 maker rebate。
- **Chainlink Data Streams 鉴权**：HMAC-SHA256，每个 WS 连接独立签名。Secret 一旦泄露需联系 Chainlink 重置。
