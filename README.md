# Latency Arbitrage Engine (FV)

Binance WebSocket + Polymarket 5m 做市引擎：基于公允价（Fair Value）与稳态/激变态的被动追涨与配平逻辑，带 TUI 与模拟成交落盘。

## 功能概览

- **数据**：币安 BookTicker / Depth / AggTrade，Polymarket 5 分钟 Up-Down 订单簿
- **公允价**：BS 模型 + 行权价 K、到期 T、波动率 σ（稳态用 Poly 反解 IV，激变态用粘性/默认 σ）
- **稳态 / 激变态**：币安 5s/1s 波动判定；激变态下停用 Poly 反解、启用激变态快照与领先延迟统计
- **追涨**：仅在**激变态**下挂 Maker 买（FV 领先 Poly、上涨判定）；稳态不追涨
- **配平**：偏仓侧挂 Maker 买意图，全配平价后均价之和 < 1 才允许挂
- **成交**：Maker 买模拟成交（best_ask ≤ 挂单价），换 5m 窗口时落盘 `trades/trades_{window_end_ts}.csv`，保留最新 10 份
- **实盘下单**：Polymarket CLOB V2，HTTP/2 prior-knowledge + 长 keep-alive；三种钱包类型（EOA / Safe Type 2 / Deposit Wallet Type 3），EIP-712 / ERC-7739 签名；按下单瞬间盘口在 FAK / GTC 间分流；ttl + reprice 撤单循环

## 构建与运行

```bash
cargo build --release
```

启动时按以下规则区分 dry-run 与实盘：

- `[wallet].private_key` 留空或省略整段 → **dry-run**（不下真实订单）
- 配置了 `[wallet].private_key` → **Live 模式**
- 任何时候带 `--dry-run` 命令行参数 → **强制 dry-run**（即使配了私钥也忽略）

```bash
# Dry-run（默认）：不配私钥
cargo run --release

# Live 模式：在 config/default.toml 的 [wallet] 段填入 private_key 后启动
cargo run --release

# 强制 dry-run（即使配了私钥）
cargo run --release -- --dry-run
```

默认读取 `config/default.toml`。

## 配置要点

- `[wallet]`：
  - `signature_mode`：`eoa` / `safe` / `deposit_wallet`
  - `wallet_address`：Safe / Deposit 的 maker 地址（EOA 模式忽略）
  - `private_key`：留空 → dry-run；填入 → Live
  - `builder_code`：32-byte hex（0x 前缀，空字符串视为 zero builder）
  - `polygon_rpc_url`：pUSD 余额查询用 RPC
- `[trading]`：`symbol`（如 BTCUSDT）、`poly_token_id`、`strike_price`、波动率默认与裁剪、`order_size_usdc`（必填，无代码默认；每笔 BuyIntent 的下单名义额 USDC，`qty_shares = order_size_usdc / target_price`）
- `[exchange]`：币安 WS/REST 端点
- `[orderbook]`：`depth_levels`（订单簿档位）

L2 API key（apiKey / secret / passphrase）**不写在 toml**，由启动流程通过 `[wallet].private_key` 签 `ClobAuth` 调 `/auth/derive-api-key` 派生。

详见 `config/default.toml`。

## 目录结构

```
src/
  main.rs           # 入口、TUI 与事件循环
  config.rs         # 配置加载
  strategy/         # 信号与策略（signal.rs：稳态/激变态、追涨/配平、Maker 意图）
  tui/              # 面板与 AppState（仓位、成交、挂单意图）
  ws/               # Binance / Poly WebSocket 与市场发现
  execution/        # 下单与签名
    client.rs       # OrderClient trait（dispatch_buy_intent / tick / place_order / cancel_order）
    signer.rs       # PolySigner（EIP-712 / ERC-7739，三种钱包）
    transaction.rs  # LiveOrderClient（H2-tuned reqwest + L2 HMAC + V2 JSON）
    balance.rs      # pUSD ERC-20 balanceOf（alloy）
    sim.rs          # ExecutionSim（干跑撮合模拟器，自身 impl OrderClient）
  model/            # 订单簿、Ticker、成交解析
  metrics/          # 延迟统计
config/
  default.toml      # 默认配置
trades/             # 落盘 CSV（trades_{window_end_ts}.csv）
excited_snapshots/  # 激变态快照 CSV（可选保留）
```

## 版本

- **0.4.6**：实盘下单与撤单（Polymarket CLOB V2，FAK / GTC 按盘口分流），三种钱包（EOA / Safe / Deposit），L2 API key 自动派生，pUSD 余额查询，builder code 归因，ttl + reprice 撤单循环；`OrderClient` trait 抽象（干跑 = `ExecutionSim` 自身 impl，实盘 = `LiveOrderClient`）
- **0.4.5**：V2 签名层 + 三种钱包类型 + dry-run/live 启动区分
- **0.2.0**：只在激变态追涨、显式 `CHASE_ONLY_IN_EXCITED`、稳态清空追涨侧
- **0.1.0**：初始版本（FV、稳态/激变态、追涨与配平、Maker 买模拟成交与落盘）

详见 [CHANGELOG.md](CHANGELOG.md)。
