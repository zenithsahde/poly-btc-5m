# 反解 IV 与 FV 使用的数据来源

本文档明确：**反解隐含波动率（IV）** 和 **公允价（FV）** 各自用到的输入数据从哪里来、在什么时机用。

---

## 一、触发时机

- **反解 IV 与 FV 的更新** 都在 **BookTicker** 分支里执行（即：每收到一条**币安**的 `bookTicker` 推送就算一次）。
- 此时会从 **共享状态 `AppState`** 里读出一份「当前快照」：Poly 的 bid/ask、窗口结束时间、strike、粘性 σ 等，再配合**本条 BookTicker 里的币安 mid** 一起算 IV 和 FV。

因此：**S 来自当前这条币安 tick；K、T、Poly 价格、稳态/激变态等来自「当前时刻状态里已有的值」**，可能和 Poly 最新一条推送不同步。

---

## 二、反解 IV 用到的数据

`find_implied_volatility(spot, strike, expiry_years, target_price)` 的四个参数含义与来源如下。

| 参数 | 含义 | 当前数据来源 |
|------|------|----------------|
| **spot** | 标的价格 S | **本条 BookTicker 的币安 mid**（`bba.mid_price()`），即当前这条消息里的币安现货价。 |
| **strike** | 行权价 K | **状态里的 `s.strike_price`**；若 ≤0 则用 `mid.round()`。K 在 5m 窗口切换时由币安 REST「窗口开始秒内首笔成交价」写入，或来自配置默认。 |
| **expiry_years** | 到期前时间 T（年） | **状态里的 `s.poly_window_end_ts`**：`T = (poly_window_end_ts - now_ts) / 60` 分钟，再转成年。即「当前时刻」算出的剩余分钟数。 |
| **target_price** | 反解目标价（希望 BS 等于的值） | **隐含 Up 价**：优先用 **Up 盘口中价** `(min+max)/2`（`poly_best_bid/ask`）；若 Up 无有效盘口，则用 **1 − Down 盘口中价**（`poly_down_best_bid/ask`），因 Up + Down ≈ 1；两者都无则用 0.5。 |

要点：

- **S**：来自**本条**币安 BookTicker，和当前 tick 一致。
- **K、T、current_poly_p**：都来自**读状态时**的那一帧，即「上一次被 Poly 或其它逻辑写进状态」的值，**不是**本条 BookTicker 触发的；若 Poly 更新较慢，这里用的就是稍旧的 Poly 盘口和窗口信息。

---

## 三、FV 用到的数据

`calculate_binary_call_price(spot, strike, expiry_years, volatility)` 的四个参数含义与来源如下。

| 参数 | 含义 | 当前数据来源 |
|------|------|----------------|
| **spot** | 标的价格 S | **本条 BookTicker 的币安 mid**（同上）。 |
| **strike** | 行权价 K | **与反解 IV 相同**：`s.strike_price` 或 `mid.round()`。 |
| **expiry_years** | 到期前时间 T（年） | **与反解 IV 相同**：由 `poly_window_end_ts` 与当前时间算出。 |
| **volatility** | 年化波动率 σ | **按状态机决定**：稳态且反解 IV>0.01 时用「由 current_poly_p 反解出的 IV」并 clamp 到 [sigma_min, sigma_max_poly]；否则用粘性 σ 或配置默认 σ。 |

要点：

- **S、K、T** 与反解 IV 时用的**同一帧**一致（本条 tick 的 mid + 读状态时的 K、T）。
- **σ**：稳态下由「同一帧的 current_poly_p」反解得到（再 clamp），所以 FV 和「用 Poly 中价反解」在数学上一致；激变态或反解触底时用粘性/默认，FV 就不再跟当前 Poly 盘口强绑定。

---

## 四、Poly 中价 current_poly_p 的来源

- **定义**：`current_poly_p = (lo + hi) / 2`，其中 `lo = min(poly_best_bid, poly_best_ask)`，`hi = max(...)`。
- **current_poly_p 的组成**：
  - **有 Up 盘口**时：`current_poly_p = (min(poly_best_bid, poly_best_ask) + max(...)) / 2`；
  - **无 Up、有 Down 盘口**时：`current_poly_p = 1 - Down 中价`（Down 中价同理用 `poly_down_best_bid/ask`），这样在只有 Down 有深度时也能反解出 IV；
  - 两者都无则 0.5。
- **has_poly**：Up 或 Down 任一侧有有效盘口即视为有 Poly 数据，可进入稳态并反解 IV。
- **poly_spread**：用当前参与计算的那一侧（Up 或 Down）的价差做「价差窄」判定。

因此：**反解 IV 和 FV 里的「Poly 价格」优先来自 Up 中价，缺 Up 时用 Down 推导的隐含 Up 价，均为「状态里当前那帧」的值。**

---

## 五、潜在问题（与「还有很多地方有问题」的对应）

1. **时序不同步**  
   FV/IV 在**币安 BookTicker** 时用「当时状态里的 Poly 盘口」算。若 Poly 更新频率低于币安，或 Poly 与币安走不同线程，会出现：**S 是最新的币安价，K/T/Poly 中价是稍旧的状态**。

2. **只用 Up 的盘口**  
   current_poly_p 只用了 **Up** 的 best_bid/ask。若你希望「用 Down 盘口反推再换算 Up」或「Up/Down 一起约束」，当前实现没有做。

3. **K 的语义**  
   K 来自「窗口开始时刻的币安价」（REST 秒级或 1m 开盘价），与 Polymarket 官方对 5m 行权价的定义是否完全一致需要对照文档。

4. **T 的基准**  
   T 用本机 `chrono::Utc::now()` 与 `poly_window_end_ts` 的差。若 `poly_window_end_ts` 是窗口结束的 Unix 秒，且与 Poly 结算时间一致，则 T 正确；否则会有系统偏差。

5. **稳态判定用的也是「状态里的」Poly**  
   稳态条件里有「Poly 价差窄」等，用的同样是当前状态里的 `poly_best_bid/ask` 和价差，与反解 IV 同源、同帧。

---

## 六、小结表

| 量 | 反解 IV 时用 | FV 时用 | 实际来源 |
|----|--------------|---------|----------|
| S  | ✓            | ✓       | 本条 BookTicker 的币安 mid |
| K  | ✓            | ✓       | 状态 strike_price（窗口切换时由币安 REST 写入或配置） |
| T  | ✓            | ✓       | 状态 poly_window_end_ts − now，转成分钟再年化 |
| Poly 价 | ✓（target_price） | 间接（通过反解出的 σ） | 状态 poly_best_bid/ask（Up），(lo+hi)/2 |
| σ  | —            | ✓       | 稳态：反解 IV clamp；否则粘性/默认 |

若你希望「反解 IV 和 FV 用同一套、且更清晰的数据源」（例如统一用「最近一次 Poly 推送时的 S/K/T/Poly 价」或「显式时间戳对齐」），可以在上述基础上再改一版数据流设计，我可以按你的目标给出具体改法（仍不涉及下单/撤单）。
