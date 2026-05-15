# Latency Arbitrage Engine (FV)

Binance WebSocket + Polymarket 5m 做市引擎：基于公允价（Fair Value）与稳态/激变态的被动追涨与配平，支持实盘下单 / 用户成交回流 / 窗口收盘自动合并 / 持久化与 TUI + Web 面板。

## 功能概览

- **行情**：币安 BookTicker / Depth / AggTrade，Polymarket 5 分钟 Up-Down 订单簿（Market Channel）。
- **公允价**：BS 模型 + 行权价 K、到期 T、波动率 σ（稳态用 Poly 反解 IV，激变态用粘性/默认 σ）。
- **稳态 / 激变态**：币安 5s/1s 波动判定；激变态下停用 Poly 反解、启用激变态快照与领先延迟统计。
- **追涨**：仅在**激变态**下挂 Maker 买（FV 领先 Poly、上涨判定）；稳态不追涨。
- **配平**：偏仓侧挂 Maker 买意图，全配平价后均价之和 < 1 才允许挂。
- **实盘下单**：Polymarket CLOB V2，HTTP/2 prior-knowledge + 长 keep-alive；三种钱包（EOA / Safe Type 2 / Deposit Wallet Type 3），EIP-712 / ERC-7739 签名；按下单瞬间盘口在 FAK / GTC 间分流；ttl + reprice 撤单循环；FAK 部分成交 / 400 失败走重发链（`MAX_RESUBMIT_ATTEMPTS=4`，不加价）。
- **User Channel WSS**：订阅本账户 order / trade 事件，实时回灌 ledger 与 SQLite 表 `my_orders` / `my_fills`，覆盖 MATCHED → MINED → CONFIRMED 全生命周期。
- **窗口收盘合并**：5m 窗口结束时把 Up/Down 配对仓位经 Polygon RPC merge 回 USDC（EOA 直连 NegRiskCtfExchange `mergePositions`，Safe 走 relayer）。
- **持久化**：SQLite (`webui_db/`) 同步落库 `pnl_samples` / `my_orders` / `my_fills`；窗口落盘 CSV (`trades/` / `orders/`)；30ms 等间隔快照 (`snapshots_30ms/`) 与激变态快照 (`excited_snapshots/`)。
- **前端**：TUI（默认，50ms 刷新）或 Web 面板（axum + SSE + Leptos/WASM）；两者均显示**实盘 / dry-run 模式指示器**。
- **安全熔断**：当日 cash_pnl 亏损或连续下单错误超阈值时永久阻止实盘下单，需重启进程恢复（仅作用于实盘，dry-run 不受影响）。

## 快速开始

```bash
# 1. 构建
cargo build --release

# 2. 准备配置（首次）
cp config.toml.example config.toml
# 在 config.toml 的 [wallet] 段填入 private_key / wallet_address（实盘用）；
# 或保持留空走 dry-run。

# 3. 启动
cargo run --release                    # 默认 TUI
cargo run --release -- --web           # Web 面板（浏览器访问 http://127.0.0.1:3000）
cargo run --release -- --dry-run       # 强制 dry-run（即使配了私钥也忽略）
HEADLESS=1 RUN_SECS=300 cargo run --release  # 无 UI 后台采集，到时退出
```

### dry-run / 实盘判定

- `[wallet].private_key` 留空或缺失 → **dry-run**（`ExecutionSim` 撮合，不签名不发网络）
- `[wallet].private_key` 已填 → **Live**（`LiveOrderClient` 真实下单）
- 命令行 `--dry-run` 始终强制 dry-run，无视私钥

### CLI 参数

| 参数 | 说明 |
| --- | --- |
| `--tui` / 默认 | 终端 TUI |
| `--web` | 启用 axum + SSE 前端（Web UI 由 trunk 单独构建到 `web-ui/dist/`） |
| `--host` / `--port` | Web 监听地址（默认 `127.0.0.1:3000`） |
| `--web-dist` | Web UI 静态资源目录（默认 `web-ui/dist`） |
| `--dry-run` | 强制 dry-run |

### 环境变量

| 变量 | 用途 |
| --- | --- |
| `BINANCE_API_KEY` / `BINANCE_SECRET_KEY` | 币安 REST 凭据（不写在 toml 内） |
| `APP__SECTION__KEY` | 覆盖任意配置项（双下划线分隔层级，如 `APP__CIRCUIT_BREAKER__MAX_DAILY_LOSS=20`） |
| `RUST_LOG` | tracing filter（缺省即不开日志） |
| `ENGINE_LOG_FILE` | 把日志写到指定文件而非 stderr |
| `HEADLESS=1` | 跳过 TUI/Web，仅后台采集 |
| `RUN_SECS=N` | 配合 `HEADLESS`，N 秒后自动退出 |

## 配置详解

配置文件路径：**仓库根目录 `config.toml`**（不入 git，模板见 `config.toml.example`）。
环境变量优先级高于 toml。

| 段 | 关键字段 |
| --- | --- |
| `[exchange]` | `ws_endpoint` / `ws_endpoint_alt` / `rest_endpoint` 币安 WS/REST 端点 |
| `[trading]` | `symbol`（如 `BTCUSDT`）、`streams`、`poly_token_id`（留空 → 自动发现当前 5m 市场）、`strike_price`、`default_volatility_annual`、`volatility_sigma_min/max`、`volatility_sigma_max_poly`、`order_size_usdc`（必填，每笔 BuyIntent 的下单名义额 USDC，`qty_shares = order_size_usdc / target_price`） |
| `[websocket]` | 心跳与重连参数 |
| `[latency]` | 延迟统计阈值与上报间隔 |
| `[orderbook]` | `depth_levels`（订单簿档位） |
| `[api]` | 币安 API key 占位；实际从 `BINANCE_API_KEY` / `BINANCE_SECRET_KEY` 环境变量读取 |
| `[logging]` | `level`（仅作记录，运行时实际由 `RUST_LOG` 控制） |
| `[circuit_breaker]` | `enabled` / `max_daily_loss`（USDC，按 cash_pnl 口径） / `max_consecutive_errors`（连续 reject / transport-failed 次数） |
| `[wallet]` | `signature_mode`（`eoa` / `safe` / `deposit_wallet`）、`wallet_address`（Safe / Deposit 的 maker 地址，EOA 模式忽略）、`private_key`（留空 → dry-run）、`builder_code`（32-byte hex，0x 前缀，空字符串视为 zero builder）、`polygon_rpc_url`（pUSD 余额查询 + 收盘 merge 用 RPC） |

L2 API key（apiKey / secret / passphrase）**不写在 toml**：实盘启动时由 `[wallet].private_key` 签 `ClobAuth` 调 `/auth/derive-api-key` 派生。

### 熔断器行为

`[circuit_breaker]` 仅在实盘 (`LiveOrderClient`) 接入；触发条件二选一即永久 halt：

1. **MaxDailyLoss**：调用 `AppState::cash_pnl()`（已实现现金流 + rebate − fee），亏损超过 `max_daily_loss` USDC。
2. **ConsecutiveErrors**：连续 `max_consecutive_errors`（默认 5）次下单失败（HTTP rejected 或 transport failed，重发链同样计数）。

触发后所有 `dispatch_buy_intent` 与 resubmit 请求立即被拦截，日志 `🚨 CIRCUIT BREAKER TRIPPED: …`，需重启进程恢复。dry-run 模式不接入熔断。

## 目录结构

```
config.toml.example       # 配置模板（入 git）
config.toml               # 本地配置（含私钥，gitignored）
engine/                   # Rust 引擎
  src/
    main.rs               # 入口、CLI、模式分发（TUI / Web / Headless）
    cli.rs                # clap 参数
    config.rs             # 配置加载（含 CircuitBreakerConfig）
    position.rs           # 仓位 / ledger / PnL 计算
    strategy/             # 信号引擎与 BS 公允价
      signal.rs           # 稳态 / 激变态、追涨 / 配平、Maker 意图
      decision.rs         # BuyIntent 决策（纯函数）
      bs_model.rs         # Black-Scholes + IV 反解
      fv.rs / volatility.rs / fv_snapshots.rs / excited_snapshots.rs / snap30ms.rs
    execution/            # 下单 / 签名 / 合并 / 熔断
      client.rs           # OrderClient trait
      signer.rs           # PolySigner（EIP-712 / ERC-7739）
      transaction.rs      # LiveOrderClient（H2-tuned reqwest + L2 HMAC）
      sim.rs              # ExecutionSim（干跑撮合）
      resubmit.rs         # FAK 部分成交 / 400 失败重发链
      merge.rs            # 收盘 mergePositions（EOA 直连 + Safe relayer）
      balance.rs          # pUSD ERC-20 balanceOf
      circuit_breaker.rs  # 安全熔断
      user_ws_handler.rs  # User WSS 事件 → ledger / DB 回灌
      ioc.rs / order_manager.rs / gateway.rs
    ws/
      client.rs           # 币安 WS
      poly_client.rs      # Polymarket Market Channel
      poly_user_ws.rs     # Polymarket User Channel（实盘订单 / 成交事件）
      discovery.rs        # 5m 市场自动发现
      binance_rest.rs
    tui/                  # ratatui 面板 + AppState（含 is_live_mode）
    web/                  # axum + SSE + SQLite writer
      db.rs               # webui_db/ SQLite（pnl_samples / my_orders / my_fills）
      sse.rs / routes.rs / snapshot.rs
    model/                # 订单簿 / Ticker / 成交解析
    metrics/              # 延迟统计
web-ui/                   # Leptos / WASM 前端（trunk 单独构建）
shared-types/             # 引擎与 web-ui 共享类型
trades/                   # 窗口成交 CSV（保留最新 10 份）
orders/                   # 窗口订单 CSV
fv_snapshots/             # FV 时序快照
excited_snapshots/        # 激变态快照
snapshots_30ms/           # 30ms 等间隔时序快照
webui_db/                 # SQLite 数据库（gitignored）
```

## 版本

- **未发布 (feat/trade)**：安全熔断（MaxDailyLoss / ConsecutiveErrors，仅实盘）；`config/default.toml` → 根目录 `config.toml` + `config.toml.example`，本地配置入 `.gitignore`；TUI / Web 加实盘模式指示器；实盘 order/fill 持久化 + ResubmitRequest 链；Polymarket User Channel WSS 接入；窗口收盘 on-chain merge（EOA 直连 + Safe relayer）
- **0.4.6**：实盘下单与撤单（CLOB V2，FAK / GTC 分流），三种钱包，L2 API key 自动派生，pUSD 余额查询，builder code 归因
- **0.4.5**：V2 签名层 + 三种钱包类型 + dry-run/live 启动区分
- **0.2.0**：只在激变态追涨、稳态清空追涨侧
- **0.1.0**：初始版本（FV、稳态/激变态、Maker 买模拟成交与落盘）

详见 [CHANGELOG.md](CHANGELOG.md)。
