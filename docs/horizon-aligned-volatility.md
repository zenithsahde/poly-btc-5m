# 期限对齐的波动率估计（Horizon-Aligned Volatility）

本文档说明为何以及如何实现「先估计与到期期限一致的波动率，再换算成年化」用于 5 分钟 Polymarket UP/DOWN 公允价。

---

## 一、为什么需要「期限对齐」？

Black-Scholes 公式里的 **σ 是年化波动率**，而 5 分钟期权的实际风险来自「**未来 5 分钟**」的波动，不是「未来一年」。

若用「每 tick 收益」直接按时间年化，例如：

- 先算每段（相邻 tick 之间）收益的标准差 σ_per_period；
- 再乘上年化因子：`σ_annual = σ_per_period × √(periods_per_year)`，其中 `periods_per_year = 一年秒数 / 平均每段秒数`。

则「平均每段」可能是几十毫秒，periods_per_year 会非常大，导致：

- 对 5 分钟这种短期限，估计极不稳定；
- 估计的是「极短周期」的波动率年化，与「未来 5 分钟」的风险并不一致。

更合理的做法是：**先估计「未来 τ 这段」的波动率**（τ 与到期期限一致，如 5 分钟或当前剩余 T），**再只为代入 BS 换算成年化**。

---

## 二、公式关系

记：

- **T_min**：期权剩余分钟数（例如 9.8 分钟）；
- **T_years**：T_min 对应的年化时间，即 `T_min / (60×24×365.25)`；
- **σ_T**：「过去 T 分钟」内 log-return 的**已实现波动率**（即这段总收益的标准差）。

在 BS 里，**到期时** log-return 的方差为 `σ_annual² × T_years`。因此若我们**定义**「年化 σ」使得「未来 T 段」的波动率等于我们估计的 σ_T，则有：

```
σ_T² = σ_annual² × T_years   ⇒   σ_annual = σ_T / √(T_years)
```

代入 BS 后，公式里的 `σ√T` 项等于 `(σ_T/√(T_years)) × √(T_years) = σ_T`，即期权定价直接用到的是「T 段波动率」σ_T，年化只是中间步骤。

---

## 三、实现要点

### 3.1 数据

- 只保留**最近约 20 分钟**的 (价格, 时间戳)，用于在任意「剩余 T 分钟」时截取「过去 T 分钟」的 tick。
- 每次 BookTicker 时用**当前剩余到期分钟数** `expiry_min` 作为 `horizon_minutes` 截取窗口，保证「估计的期限」与「期权剩余期限」一致。

### 3.2 已实现波动率 σ_T

在「过去 T 分钟」窗口内：

1. 取该窗口内所有 (P_i, t_i)，按时间排序；
2. 计算相邻价格的对数收益：`r_i = ln(P_i / P_{i-1})`；
3. 算这 n 个 r_i 的**样本方差** Var(r)（分母 n-1）；
4. 在 iid 假设下，**T 段总收益** R = r_1 + … + r_n 的方差为 `Var(R) = n × Var(r)`，故：
   - **σ_T = √(n × Var(r))**

即「T 段」的已实现波动率。

### 3.3 年化与代入 BS

- **σ_annual = σ_T / √(T_years)**，再 clamp 到配置的 `volatility_sigma_min`～`volatility_sigma_max`；
- 用该 σ_annual 与 S、K、T 代入 `calculate_binary_call_price` 得到 FV_up，FV_down = 1 - FV_up。

### 3.4 样本不足时

若「过去 T 分钟」内点数少于 2 或 σ_T 算出来为 0，则 `get_annual_vol_for_horizon` 返回 0，上层逻辑会使用配置的 `default_volatility_annual`，并标记为「使用默认 σ」（TUI 中可显示为固定/默认状态）。

---

## 四、代码位置

- **波动率逻辑**：`src/strategy/volatility.rs`
  - `update(price, ts_ms)`：只喂价与时间戳，保留最近 20 分钟；
  - `get_annual_vol_for_horizon(now_ts_ms, horizon_minutes)`：截取过去 horizon_minutes 的数据，算 σ_T 再年化。
- **调用处**：`src/strategy/signal.rs` 的 BookTicker 分支
  - 用 `expiry_min` 作为 horizon，`now_ts_ms = Utc::now().timestamp_millis()`，取 `raw_sigma = vol_calc.get_annual_vol_for_horizon(now_ts_ms, expiry_min)`，再 default/clamp 后代入 BS。

---

## 五、效果小结

- σ 不再依赖「每 tick 间隔」的巨大年化倍数，而依赖「**过去 T 分钟**」的真实已实现波动，与 5 分钟 UP/DOWN 的期限一致。
- 剩余时间 T 变化时（例如从 5 分钟变到 5 分钟），估计窗口随之缩短，始终与到期期限对齐。
- 样本不足或数据不可用时，回退到默认 σ，行为与之前一致。
