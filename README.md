# poly-btc-5m

Binance 行情驱动的 Polymarket BTC 5 分钟二元市场做市 / 套利引擎。基于公允价（Black-Scholes）与稳态 / 激变态切换的被动追涨与配平逻辑，已接入 Polymarket 实盘下单（CLOB POST /order + Safe / EOA 签名 + User-Channel WSS + 收盘 on-chain merge），TUI 与 Leptos Web 双前端。

## 快速开始

```bash
# 1. 准备配置（含私钥的本地配置不入 git）
cp config.toml.example config.toml
$EDITOR config.toml    # 至少填 trading.poly_token_id；实盘还需 wallet.*

# 2. 编译
cargo build --release

# 3. 运行（默认 TUI 渲染；按 q 或 Ctrl+C 退出）
cargo run --release                # 实盘（若 wallet.private_key 已填）
cargo run --release -- --dry-run   # 强制 dry-run（模拟撮合）
```

启动时 TUI 顶部会显示 `LIVE` / `DRY-RUN` 指示器。

## CLI 参数

| 参数         | 说明                                                          |
|--------------|---------------------------------------------------------------|
| `--dry-run`  | 强制使用 ExecutionSim，即使 `[wallet].private_key` 已配置也忽略 |

## 环境变量

| 变量                     | 说明                                                                      |
|--------------------------|---------------------------------------------------------------------------|
| `BINANCE_API_KEY`        | 覆盖 `[api].api_key`（推荐用环境变量注入，避免落盘）                      |
| `BINANCE_SECRET_KEY`     | 覆盖 `[api].secret_key`                                                    |
| `APP__SECTION__KEY`      | 通用覆盖：双下划线分层级，如 `APP__CIRCUIT_BREAKER__MAX_DAILY_LOSS=20`     |
| `RUST_LOG`               | tracing filter，例：`info,poly_btc_5m::execution=debug`                    |
| `ENGINE_LOG_FILE`        | 日志重定向到文件（设置后不污染 TUI）                                       |
| `HEADLESS`               | `=1` 时跳过 TUI 渲染，适合服务器 / 容器无 tty 环境                          |
| `RUN_SECS`               | 仅 HEADLESS 模式下生效；运行 N 秒后自动退出，方便录制 / 压测                |

## 配置详解

所有 section 见 [`config.toml.example`](config.toml.example)。重点：

| Section            | 关键字段                                                                                     |
|--------------------|----------------------------------------------------------------------------------------------|
| `[exchange]`       | `ws_endpoint`, `rest_endpoint`                                                               |
| `[trading]`        | `symbol`, `poly_token_id`, `strike_price`, `default_volatility_annual`, `order_size_usdc`     |
| `[websocket]`      | `ping_interval_secs`, `reconnect_base_ms / max_ms`                                            |
| `[latency]`        | `warn_threshold_ms`, `report_interval_secs`                                                   |
| `[orderbook]`      | `depth_levels`                                                                                |
| `[api]`            | `api_key`, `secret_key`（推荐用环境变量覆盖）                                                |
| `[logging]`        | `level`                                                                                       |
| `[wallet]`         | `signature_mode`（eoa / safe / deposit_wallet）, `wallet_address`, `private_key`, `builder_code`, `polygon_rpc_url` |
| `[circuit_breaker]`| `enabled`, `max_daily_loss`（USDC）, `max_consecutive_errors`                                |

## 模块

### User Channel WSS
实盘启动后订阅 Polymarket `/user` 频道（本账户全部市场），按 order / trade 事件回灌 `AppState.my_orders` 与 `my_fills`，重启或 5m 窗口切换都能 resync ledger，避免靠轮询 GET。

### 窗口收盘 on-chain merge
当 5m 窗口结束、AppState 中 Up/Down 数量相等时，调用 NegRiskAdapter 的 `mergePositions(condId, pair_qty)` 1:1 兑回 pUSDC。两种执行路径：
- **EOA**：私钥直接签 `eth_sendTransaction`
- **Safe relayer**：通过 0x Safe Apps 风格的 relayer（参考 Polymarket 官方文档）

### 持久化
SQLite 库放在 `webui_db/`，由独立 writer task 经 mpsc 写入：
- `pnl_samples` —— 30s 周期采样 net_pnl / cash_pnl / inventory_value
- `my_orders` —— LiveOrderClient + resubmit 链每次 POST 的 order row
- `my_fills` —— User-WSS 回灌的成交

热路径（POST /order）不阻塞 SQLite IO。

### 双前端
- **TUI**（ratatui，默认）：实时盘口 / FV / 持仓 / pending orders / pnl 面板
- **Web**（`web-ui/`，Leptos + axum static serve）：可读历史 pnl 曲线与成交明细

### 🛑 安全熔断（CircuitBreaker）
仅作用于实盘（`LiveOrderClient` + resubmit 链），dry-run / `ExecutionSim` 不受影响。

**触发条件**：
- `MaxDailyLoss`：`AppState.cash_pnl() < -[circuit_breaker].max_daily_loss`
- `ConsecutiveErrors`：连续下单 reject / transport failed 次数 ≥ `max_consecutive_errors`

**行为**：
- 一旦 trip → 永久 `halted`：`dispatch_buy_intent` 直接 return，resubmit worker `drop resubmit`
- 不可逆，**需重启进程恢复**
- 启动期日志：`circuit breaker armed (live only)` + 阈值

**关闭熔断**（不推荐）：`config.toml` 设 `[circuit_breaker].enabled = false`，或 `APP__CIRCUIT_BREAKER__ENABLED=false`。

## 目录结构

```
src/
  main.rs                      # 入口 / TUI 事件循环 / dry-run vs live 分支
  config.rs                    # 配置加载（根目录 config.toml + env 覆盖）
  cli.rs                       # clap 参数
  strategy/                    # 信号 / FV / BS / 波动率
  tui/                         # AppState + 面板渲染
  ws/                          # Binance / Poly market WS + Poly user WS
  execution/
    transaction.rs             # LiveOrderClient + SharedClob + sign_and_post
    circuit_breaker.rs         # 安全熔断（本仓库重点）
    resubmit.rs                # FAK 部分成交 / 400 失败重发链
    merge.rs / merge_*.rs      # NegRiskAdapter 收盘 merge
    user_ws_handler.rs         # User-Channel 事件回灌
    signer.rs                  # EOA / Safe 签名
    sim.rs                     # ExecutionSim（dry-run 撮合）
  web/                         # SQLite writer + axum 静态服务
config.toml.example            # 配置模板（committed）
config.toml                    # 本地配置（含私钥；.gitignore）
web-ui/                        # Leptos 前端
webui_db/                      # SQLite 库
trades/ fv_snapshots/ ...      # 离线快照（.gitignore）
```

## 版本

详见 [CHANGELOG.md](CHANGELOG.md)。
