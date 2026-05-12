//! 与到期期限对齐的波动率估计（Horizon-Aligned Volatility）
//!
//! 思路：期权只剩 T 分钟到期，我们关心的是「未来 T 分钟内」的波动，而不是「未来一年」。
//! 因此先估计「过去 T 分钟内的已实现波动率」σ_T，再仅为了代入 BS 公式换算成年化：
//! σ_annual = σ_T / sqrt(T_years)。

use std::collections::VecDeque;

const SECS_PER_YEAR: f64 = 365.25 * 24.0 * 3600.0;
/// 价格缓冲保留的最长时间（分钟）。5m 窗口最长就是 5 分钟，保留 8 分钟即可覆盖剩余 T；
/// 比 15m 的 20min 大幅缩短，降低内存占用并加快 horizon 截窗。
const BUFFER_MAX_MINUTES: u64 = 8;

pub struct RollingVolatility {
    /// (价格, 时间戳 ms)
    prices_ts: VecDeque<(f64, u64)>,
}

impl RollingVolatility {
    pub fn new(_window_size: usize) -> Self {
        Self {
            prices_ts: VecDeque::with_capacity(2000),
        }
    }

    /// 喂入最新成交价与交易所时间戳(ms)；会丢弃超过 BUFFER_MAX_MINUTES 的旧点
    pub fn update(&mut self, price: f64, ts_ms: u64) {
        let cutoff = ts_ms.saturating_sub(BUFFER_MAX_MINUTES * 60 * 1000);
        while self
            .prices_ts
            .front()
            .map(|&(_, t)| t < cutoff)
            .unwrap_or(false)
        {
            self.prices_ts.pop_front();
        }
        if price > 0.0 {
            self.prices_ts.push_back((price, ts_ms));
        }
    }

    /// 用于判断是否用默认 σ：需要最近 horizon 内有足够样本
    pub fn sample_count(&self) -> usize {
        self.prices_ts.len()
    }

    /// 与到期期限对齐：用「过去 horizon_minutes 分钟」的已实现波动率估计，
    /// 再换算为年化 σ 供 BS 使用。公式：σ_annual = σ_over_T / sqrt(T_years)。
    ///
    /// - now_ts_ms: 当前时间 (ms)
    /// - horizon_minutes: 期权剩余分钟数（与 T 对齐）
    pub fn get_annual_vol_for_horizon(&self, now_ts_ms: u64, horizon_minutes: f64) -> f64 {
        if horizon_minutes < 0.5 || self.prices_ts.len() < 2 {
            return 0.0;
        }
        let window_start = now_ts_ms.saturating_sub((horizon_minutes * 60.0 * 1000.0) as u64);
        let slice: Vec<_> = self
            .prices_ts
            .iter()
            .filter(|&&(_, t)| t >= window_start && t <= now_ts_ms)
            .map(|&(p, t)| (p, t))
            .collect();
        if slice.len() < 2 {
            return 0.0;
        }
        let mut log_returns = Vec::with_capacity(slice.len());
        for i in 1..slice.len() {
            let (p0, _) = slice[i - 1];
            let (p1, _) = slice[i];
            if p0 > 0.0 && p1 > 0.0 {
                log_returns.push((p1 / p0).ln());
            }
        }
        let n = log_returns.len();
        if n < 2 {
            return 0.0;
        }
        let mean: f64 = log_returns.iter().sum::<f64>() / n as f64;
        let variance = log_returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
        let sigma_over_t = (n as f64 * variance).sqrt();
        let t_years = horizon_minutes / (60.0 * 24.0 * 365.25);
        let sigma_annual = sigma_over_t / t_years.sqrt();
        sigma_annual.min(5.0)
    }
}
