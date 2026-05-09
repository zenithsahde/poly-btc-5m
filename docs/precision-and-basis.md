# 币安现货「价格精度」的真实问题：不是小数位数，是价源 basis

本文档是对 5m 引擎是否应继续用 Binance BTCUSDT 作为公允价输入的根因分析。
TL;DR：**币安 tick size = $0.01，对 BS d2 完全无影响；真正风险是 USDT 计价 vs Polymarket Chainlink BTC/USD 结算源的 basis 漂移**。

---

## 一、tick size 不是问题（实锤）

通过 Binance `exchangeInfo` 拉取 BTCUSDT 的 PRICE_FILTER：

```
tickSize = 0.01000000  (USDT)
minPrice = 0.01
```

- 在 BTC ~$80k 下，0.01 USDT = **0.0001 bps** = 1.25e-9 of price
- BS 公式 `d2 = (ln(S/K) - σ²T/2) / (σ√T)` 中，ln(S/K) 对 S 的灵敏度 = 1/S，所以 1 cent 误差进 d2 的量级是 1e-9
- N(d2) 的导数 ≈ φ(d2)，最大 0.4，所以 P_up 误差上限 < 5e-10

**结论**：tick size 完全可忽略，**不是你嗅到的精度问题**。

---

## 二、真正的精度坑：价源 basis

### 2.1 Polymarket 5m BTC 用 Chainlink 结算（实锤）

通过实际拉取活跃的 5m 市场 metadata（slug=`btc-updown-5m-{window_start_ts}`）：

```
description:
  "The resolution source for this market is information from Chainlink,
   specifically the BTC/USD data stream available at
   https://data.chain.link/streams/btc-usd."

  "Please note that this market is about the price according to
   Chainlink data stream BTC/USD, not according to other sources or spot markets."
```

也就是说：
- **K（行权价）**= 窗口起始时刻的 Chainlink BTC/USD
- **结算价** = 窗口结束时刻的 Chainlink BTC/USD
- **结算货币** = USDC（≈ USD）

而我们的引擎读的是 **Binance BTCUSDT bookTicker（USDT 计价）**。两个价源不同 → basis 风险。

### 2.2 实测 basis 量级

同一时刻三家 BTC 现货报价：

| 交易所 | 计价 | 中价 |
|---|---|---|
| Binance BTCUSDT | USDT | 81585.17 |
| Bitstamp BTC/USD | USD | 81595.50 |
| Coinbase BTC-USD | USD | 81575.99 |

**Basis：**
- Binance(USDT) - Bitstamp(USD) = **−$10.33** (−1.27 bps)
- Binance(USDT) - Coinbase(USD) = **+$9.18** (+1.13 bps)

常态下 BN vs USD 报价漂移在 **±1–2 bps** 范围，等于 ±$10–20 在 BTC 当前价上。

### 2.3 5m σ_T 对照（决定影响是否致命）

5m 期权的标准差（σ_T = σ_annual × √(5/525600) × S）：

| σ_annual | σ_T (S=$80k) |
|---|---|
| 30% | $76 |
| 50% | $126 |
| 80% | $201 |
| 120% | $302 |

**常态 basis ($10) ≈ 0.05–0.13σ_T → P(d2) 误差 2–5%**。
**USDT 脱锚事件（2023/03 SVB 时 -2% = $1600）≈ 8–20σ_T → 完全错向**。

---

## 三、basis 在 d2 中如何传播（数学颗粒度）

```
d2_real = (ln(S_chain / K_chain) - σ²T/2) / (σ√T)
d2_used = (ln(S_bn   / K_bn  ) - σ²T/2) / (σ√T)
```

记 b(t) = (S_bn − S_chain) / S_chain ≈ USDT-USD basis at time t。

- 若 b(t) 在窗口内**恒定**：S_bn/K_bn ≈ S_chain/K_chain（K 也按 b 偏移），ln 比值不变，**basis 不传播**
- 若 b(t) 在窗口内**漂移** Δb：

```
Δd2 ≈ Δb / (σ√T)
```

5m T_years ≈ 9.5e-6，σ=0.5 → σ√T ≈ 0.00154
**Δb=10 bps（极端 1m 内漂移）→ Δd2 ≈ 0.65 → ΔP_up ≈ 24%！**

底层逻辑：**期权越短，σ√T 越小，basis 漂移的相对放大越严重**。这是 5m 比 15m 风险更高的核心原因（15m 的 σ√T 大 √3 倍）。

---

## 四、对 marker-fv-5m 的具体影响点

| 模块 | 当前用 Binance | basis 风险 | 缓解 |
|---|---|---|---|
| `signal.rs` BookTicker → S | Binance BTCUSDT mid | 实时 basis 影响 d2 numerator | 改 Coinbase Pro WS（USD 计价） |
| `binance_rest.rs` 取 K | 币安窗口起秒首笔成交价 | K 与 Chainlink K 可差 ±$15 | 改读 Chainlink data stream（最贴合 Polymarket 结算） |
| `volatility.rs` σ_T 估计 | Binance trade 流 | 价源切换不影响（log-return 相对值，basis 抵消） | 不动 |
| Strike round 到整数美元 | `mid.round()` | 币安价 round 后 K 错位 → 跨档 d2 跳跃 | 取消 round，直接用 Chainlink 报告价 |

---

## 五、推荐改造（按优先级）

### P0：换价源到 Coinbase Advanced Trade WS

- **理由**：USD 计价，monotonic 与 Chainlink BTC/USD（Chainlink BTC/USD 聚合多家但 Coinbase 占主要权重）
- **代价**：rewrite `ws/client.rs`（把币安 stream 接口换成 Coinbase l2 + ticker）

### P1：直接订阅 Chainlink Data Streams

- **理由**：与 Polymarket 结算源 byte-精确一致，**消除 basis 风险**
- **API**：https://docs.chain.link/data-streams/getting-started 提供 v0.3 stream，可走 WS 拉报价 + signed report
- **代价**：要管理 Chainlink off-chain Reports 验证（其实只读不需要验证），且需要 API key

### P2：保留 Binance 但补 basis 监控
若不想换价源（保留低延迟），最低限度：
1. 每 1s 拉一次 Coinbase BTC-USD spot
2. 算 `basis = bn_mid - cb_spot`
3. 在 signal 里把 S_used = S_bn − basis 作为 Chainlink 估计
4. 用一阶 EMA 平滑 basis（避免单点 spike 污染）
5. 当 |basis| > 5σ_T 时进入降级模式（停追涨、保留配平）

### P3：USDT 脱锚熔断

- 监控 Binance USDTUSD 或 Curve 3pool 的 USDT 折价
- |1 - USDT/USD| > 0.5% 时全引擎熔断（停所有意图）

---

## 六、与 marker-fv 文档作者已识别问题对照

`docs/fv-and-iv-data-sources.md:74-75` 作者自己已经写：

> "K 来自「窗口开始时刻的币安价」（REST 秒级或 1m 开盘价），与 Polymarket 官方对 5m/15m 行权价的定义是否完全一致需要对照文档。"

本调研补完了那个 TODO：**Polymarket 用 Chainlink，不用币安**。这个 TODO 必须升级为 P0。

---

## 七、对你的疑惑的最终回应

| 你的疑惑 | 实测结论 |
|---|---|
| 币安现货价格精度问题 | **小数位数不是问题（tick=$0.01，可忽略）** |
| 真正问题在哪 | **价源 basis（USDT-USD）+ Chainlink vs Binance reference 不一致** |
| 量级 | 常态 ±$10（0.05–0.13σ_T），脱锚事件可达 12σ+ |
| 为什么 5m 比 15m 严重 | σ√T 小 √3 倍 → basis 漂移在 d2 上的放大也大 √3 倍 |
| 最小可行修复 | P2（保留 Binance + Coinbase basis 校正）2 天可上 |
| 根治方案 | P1（直连 Chainlink data stream）一周工作量 |
