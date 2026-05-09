# Latency Arbitrage Engine (FV)

Binance WebSocket + Polymarket 5m 做市引擎：基于公允价（Fair Value）与稳态/激变态的被动追涨与配平逻辑，带 TUI 与模拟成交落盘。

## 功能概览

- **数据**：币安 BookTicker / Depth / AggTrade，Polymarket 5 分钟 Up-Down 订单簿
- **公允价**：BS 模型 + 行权价 K、到期 T、波动率 σ（稳态用 Poly 反解 IV，激变态用粘性/默认 σ）
- **稳态 / 激变态**：币安 5s/1s 波动判定；激变态下停用 Poly 反解、启用激变态快照与领先延迟统计
- **追涨**：仅在**激变态**下挂 Maker 买（FV 领先 Poly、上涨判定）；稳态不追涨
- **配平**：偏仓侧挂 Maker 买意图，全配平价后均价之和 < 1 才允许挂
- **成交**：Maker 买模拟成交（best_ask ≤ 挂单价），换 5m 窗口时落盘 `trades/trades_{window_end_ts}.csv`，保留最新 10 份

## 构建与运行

```bash
cargo build --release
cargo run --release
```

默认读取 `config/default.toml`，可通过环境变量或 `CONFIG` 指定其它配置文件。

## 配置要点

- `[trading]`：`symbol`（如 BTCUSDT）、`poly_token_id`、`wallet_address`、`strike_price`、波动率默认与裁剪
- `[exchange]`：币安 WS/REST 端点
- `[orderbook]`：`depth_levels`（订单簿档位）

详见 `config/default.toml`。

## 目录结构

```
src/
  main.rs           # 入口、TUI 与事件循环
  config.rs         # 配置加载
  strategy/         # 信号与策略（signal.rs：稳态/激变态、追涨/配平、Maker 意图）
  tui/              # 面板与 AppState（仓位、成交、挂单意图）
  ws/               # Binance / Poly WebSocket 与市场发现
  execution/        # 下单与签名（Poly API）
  model/            # 订单簿、Ticker、成交解析
  metrics/          # 延迟统计
config/
  default.toml      # 默认配置
trades/             # 落盘 CSV（trades_{window_end_ts}.csv）
excited_snapshots/  # 激变态快照 CSV（可选保留）
```

## 版本

- **0.2.0**：只在激变态追涨、显式 `CHASE_ONLY_IN_EXCITED`、稳态清空追涨侧
- **0.1.0**：初始版本（FV、稳态/激变态、追涨与配平、Maker 买模拟成交与落盘）

详见 [CHANGELOG.md](CHANGELOG.md)。
