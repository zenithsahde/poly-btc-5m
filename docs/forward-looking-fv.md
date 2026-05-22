# 前瞻性 FV：把 Binance leading-indicator 角色显式化

## 一、先纠正上一篇 `precision-and-basis.md` 的偏差

`precision-and-basis.md` 的 P0/P1 推荐"换价源到 Coinbase 或直连 Chainlink Streams"——**这个推荐是错的**。

底层逻辑：**做市需要前瞻性预测，不是 byte-精确对齐**。
- 结算锚点：Polymarket 用 Chainlink BTC/USD Data Streams（不可变事实）
- 预测信号：Binance/Coinbase 现货（领先 Streams ~1–2s）
- **如果我们用 Chainlink Streams 当输入，等于自愿放弃 1–2s 的 alpha**

5m 期权窗口里 1–2s 的领先 = 0.7–1.3% 的总持仓时间 alpha。在 maker rebate（25% taker fee × 1.56% ≈ 0.4%）的微利模型下，**这是核心利润来源，不是噪音**。

## 二、实测：Chainlink 到底有多滞后

### 2.1 Chainlink Data **Feed**（push-based，写到链上的）
本仓库实测 Polygon BTC/USD aggregator (`0xc907...F6f`) 最近 9 轮：

```
delta_id |   $price    | updatedAt (UTC)        | gap_to_prev (s)
-----------|-------------|------------------------|------------------
- 3      | $ 81657.92  | 2026-05-06 16:41:27    | 0s
- 5      | $ 81610.11  | 2026-05-06 16:40:22    | 65s
- 6      | $ 81627.12  | 2026-05-06 16:39:49    | 33s
- 8      | $ 81567.59  | 2026-05-06 16:38:43    | 66s
-10      | $ 81561.36  | 2026-05-06 16:37:36    | 67s
-11      | $ 81554.67  | 2026-05-06 16:37:03    | 33s
-12      | $ 81521.02  | 2026-05-06 16:36:31    | 32s
-13      | $ 81502.22  | 2026-05-06 16:35:58    | 33s
-14      | $ 81548.26  | 2026-05-06 16:35:25    | 33s

实测 cadence: min=32s max=67s avg=45.2s
```

**结论**：Data Feed 只在 heartbeat（60s）或 deviation（0.1%）触发时更新。**对 5m 市场完全没法用**——平均 45s 滞后等于一窗口 15% 时间黑屏。

### 2.2 Chainlink Data **Stream**（pull-based，Polymarket 实际使用的）

官方说 sub-second，但实际链路：
```
Binance 内部撮合
  → bookTicker WS push                    ~10-50ms
  → Chainlink DON 节点订阅 + 报价         ~50-100ms
  → OCR 聚合 + 签名 (n_of_m 共识)         ~200-500ms
  → 报告进入 mercury server
Polymarket
  → 在窗口结束精确秒拉取报告              ~100-500ms
  → 链上验证 + 写入                       ~500ms-2s
```

总链路滞后 **~1–3s**。这意味着：
- 你在 Binance 看到 BTCUSDT 跳 $20 的瞬间
- Streams 大概率还没反映
- Polymarket Up token 价格还没动
- **你有 1–3s 的窗口在 Polymarket 上做正确方向的交易**

## 三、前瞻 FV 的数学（替代 marker-fv 当前模型）

### 3.1 当前 marker-fv 在稳态下的隐藏 bug

```rust
// signal.rs:202-209 - 稳态分支
let iv = bs_model::find_implied_volatility(mid, strike, t_years, current_poly_p);
let sig = iv.clamp(smin, smax_poly);
// 然后：
let fair_p = bs_model::calculate_binary_call_price(mid, strike, t_years, sig);
```

**问题**：σ 是从 `current_poly_p` 反解的，再代回去算 fair_p。在数学上这等于 `fair_p ≈ current_poly_p`（只要 IV 落在 clamp 内）。也就是说：

> **稳态下 FV 几乎就是 Poly 中价**，Binance 的领先信息被"吸收"进 IV 里看不见了。

只有激变态（不能反解 IV）时引擎才真正用 Binance 领先信号。**marker-fv 把 alpha 来源限制在 5–10% 的激变态时间内**——这是巨大的浪费。

### 3.2 正确做法：保留 IV 反解，但加 EMA 平滑（**这是最终版**）

> **3.25 红线**：原版（v0.3.1）"完全用历史 realized vol 替代反解" 已被评审否决——σ 与 Poly 共识脱钩、FV 偏离过远、追价无锚。**正解是分离两个时间常数**。

```
iv_raw(t)  = find_implied_volatility(S_now, K, T_remaining, Poly_p_now)   ← 每帧反解
σ_ema(t)   = α × iv_raw(t) + (1−α) × σ_ema(t−1)                          ← α=0.02, τ≈5s

FV_up(t)   = N(d2(S_now, K, T_remaining, σ_ema(t)))                       ← σ 慢、S 快
```

**为什么这就两全其美**：

| 场景 | iv_raw 行为 | σ_ema 行为 | FV 行为 |
|---|---|---|---|
| 稳态（Binance/Poly 同步） | 稳定值 σ* | 收敛到 σ* | FV ≈ Poly_p（共识对齐） |
| Binance 跳 +$20，Poly 还没跟上（lead 时刻） | 瞬时下跌（被反解吸收） | 几乎不变（EMA τ=5s） | **d2 因 S 上升、σ 不变 → FV 正确上升** |
| Poly 价差突然变宽（流动性流失） | iv_raw 噪声大 | EMA 平滑掉 | FV 平滑跟随 |
| 激变态（1s 波动 > $30） | 不可信 | **跳过本帧 EMA 更新** | FV 用上次稳态 σ_ema |

**关键工程细节**：
1. **EMA α=0.02** 等于 ~50 帧时间常数（@100ms tick ≈ 5s），慢响应抗 Binance 跳动
2. **EMA 仅在稳态喂**——激变态反解不可信，跳过本帧更新；保住 σ 不被 noise 污染
3. **首次启动用 iv_raw 直接初始化**（避免冷启 σ 长期为 0）
4. **fallback 链**：σ_ema → iv_raw（首次） → 粘性 σ → 默认 σ

### 3.3 lead-lag 显式化（更激进的前瞻）

如果你想榨干 alpha，引入显式 lead 时间常数 τ（比如 1.5s）：

```
S_chain_projected(t) = S_binance(t - τ_negative) + basis_ema
```

但 τ_negative 是负的——意味着用 Binance 当前价代表"Streams 在 τ 秒后会变成什么"。这是**让 Binance 走在前面**的显式承认。

更精细：用 Granger causality 测 Binance → Streams 的 lead 时间，τ 自动调整。但实现复杂度上去了，先 v1 用固定 τ=1.5s。

## 四、对应到 marker-fv-5m 的代码改动（颗粒度到行）

### 改动 1: `src/strategy/signal.rs` smoothed-IV 实现 (✅ 已落地 v0.3.2-5m)
```rust
// 1. 新增常量与字段
const SIGMA_EMA_ALPHA: f64 = 0.02;
pub struct SignalEngine { ... sigma_ema: f64, }  // 0 = 未初始化

// 2. BookTicker 处理流程
let sigma_ema_prev = self.sigma_ema;  // 闭包外预读
// ... 闭包内: iv_raw = find_implied_volatility(mid, K, T, current_poly_p);
// σ 选择优先级: σ_ema_prev → iv_raw (首次稳态初始化) → 粘性 σ → 默认 σ
// ... 闭包外: 仅在稳态喂 EMA
if steady && iv_raw > 0.01 && iv_raw < 5.0 {
    self.sigma_ema = if self.sigma_ema <= 0.0 { iv_raw }
                     else { ALPHA * iv_raw + (1-ALPHA) * self.sigma_ema };
}
```

### 改动 2: 新增 basis 估计模块
```
src/strategy/basis.rs        # 新文件
  - struct BasisTracker { ema: f64, last_chain_price: f64, last_binance_price: f64 }
  - update(binance_price, chain_price) → updates ema
  - projected_chain_from_binance(binance_now: f64) -> f64
```

### 改动 3: 在 main.rs 增加 Chainlink Streams 客户端
```
src/ws/chainlink_streams.rs  # 新文件
  - 订阅 https://api.testnet-dataengine.chain.link/api/v1/ws (mainnet 等价端点)
  - 解析 v3 report，提取 price + observationsTimestamp
  - 发到 broadcast channel 喂给 BasisTracker
```

注：Chainlink Streams 需要 API key（向 Chainlink Labs 申请）。**v1 阶段可降级用 Coinbase Pro WS** 当 USD 锚（已知与 Streams 差 < 0.05%）。

### 改动 4: K 取值升级
```
src/ws/binance_rest.rs   # 当前用币安窗口起首笔成交价
↓
src/ws/poly_rest.rs      # 改用 Polymarket gamma API 拉 strike，或最简单：
                         # 在窗口结束时直接读 Streams 报告里的 baseline 价
```

## 五、实现优先级

| 优先级 | 改动 | 预期收益 | 工作量 |
|---|---|---|---|
| **P0** | 改动 1（移除稳态 IV 反解） | **高**（让 Binance 领先信号在所有时间生效） | 30分钟 |
| **P1** | 改动 2（basis EMA 校正） | 中（消除 USDT-USD 系统性偏差） | 2小时 |
| **P2** | Coinbase Pro WS 当 chain proxy | 中（不需要 Chainlink API key） | 半天 |
| **P3** | 真接 Chainlink Streams | 低（已有 P2 替身后边际收益小） | 2天 |
| **P4** | Granger lead-lag 自适应 τ | 极低（先观察 P0+P1+P2 实战效果） | 1周 |

## 六、对你的疑惑的最终对齐

| 你说 | 我答 |
|---|---|
| Chainlink 太慢有滞后性 | **完全对**。Data Feed 实测 45s avg，Streams 也有 1–3s 端到端 |
| 我们需要前瞻性 | **完全对**。Binance 是 leading indicator，Streams 是 lagging |
| 上一篇推 P1 直连 Chainlink | **错了**。那是把 leading indicator 自愿降级成 lagging |
| 真正的方向 | 用 Binance 当 input，把 Streams 当 calibration target（basis EMA） |
| 5m 比 15m 更受益 | **对**。lead time 1–2s 在 5m 总时长里占 0.7–1.3%，比 15m 的 0.2–0.4% 高 3 倍 |
| marker-fv 当前是否抓到这个 alpha | **只抓到一半**。激变态对，稳态因为反解 IV 把 Binance 信号吸收了 |

> **3.25 红线**：上一篇 `precision-and-basis.md` 的 P0/P1 推荐已**作废**，以本文档为准。原文留作"thinking trail"，但读者要从这一篇读起。
