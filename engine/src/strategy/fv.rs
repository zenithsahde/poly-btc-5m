/// strategy/fv.rs - 前瞻公允价 FV 计算
///
/// 单一职责：把 Binance mid + Poly 中价 + 时间结构 → 一帧 FV 结果。
/// 不读 AppState，只接受/返回值；σ_ema 和 FV 历史由 `FvEngine` 自身托管。
///
/// 设计要点（详见 docs/forward-looking-fv.md）：
///   - σ 来自 Poly IV 反解 + EMA 平滑（仅稳态喂 EMA，激变态跳过避免污染）
///   - σ 选择优先级：σ_ema → 稳态 iv_raw 初值 → 粘性 σ → 默认 σ
///   - FV = N(d2(S, K, T, σ_ema))，T 用窗口剩余分钟换算成年
use std::collections::VecDeque;

use crate::strategy::bs_model;

const FV_HISTORY_LEN: usize = 10;
/// IV EMA 平滑系数 α；时间常数 τ ≈ 1/α 帧 ≈ 5s @ 100ms/tick
const SIGMA_EMA_ALPHA: f64 = 0.02;

pub struct FvInputs {
    pub binance_mid: f64,
    pub strike: f64,
    pub expiry_min: f64,
    pub poly_p: f64,
    pub has_poly: bool,
    pub steady: bool,
    pub sticky_sigma: f64,
    pub sigma_min: f64,
    pub sigma_max_poly: f64,
    pub default_sigma: f64,
}

pub struct FvResult {
    pub fair_up: f64,
    pub fair_down: f64,
    pub sigma: f64,
    pub sigma_used_default: bool,
    pub sigma_source: &'static str,
    pub new_sticky: f64,
    pub iv_raw: f64,
    pub iv_poly: f64,
    pub fv_up_rising: bool,
    pub fv_down_rising: bool,
}

pub struct FvEngine {
    sigma_ema: f64,
    fv_up_history: VecDeque<f64>,
    fv_down_history: VecDeque<f64>,
}

impl FvEngine {
    pub fn new() -> Self {
        Self {
            sigma_ema: 0.0,
            fv_up_history: VecDeque::with_capacity(FV_HISTORY_LEN + 5),
            fv_down_history: VecDeque::with_capacity(FV_HISTORY_LEN + 5),
        }
    }

    pub fn sigma_ema(&self) -> f64 {
        self.sigma_ema
    }

    pub fn compute(&mut self, inp: FvInputs) -> FvResult {
        let t_years = bs_model::minutes_to_years(inp.expiry_min);

        let iv_raw = if inp.has_poly {
            bs_model::find_implied_volatility(inp.binance_mid, inp.strike, t_years, inp.poly_p)
        } else {
            0.0
        };

        // 稳态时把可信 iv_raw 喂 EMA（5.0 上限防异常解爆掉）；激变态跳过
        if inp.steady && iv_raw > 0.01 && iv_raw < 5.0 {
            if self.sigma_ema <= 0.0 {
                self.sigma_ema = iv_raw;
            } else {
                self.sigma_ema =
                    SIGMA_EMA_ALPHA * iv_raw + (1.0 - SIGMA_EMA_ALPHA) * self.sigma_ema;
            }
        }

        let (sigma, sigma_used_default, sigma_source, new_sticky) = if self.sigma_ema > 0.01 {
            let s = self.sigma_ema.clamp(inp.sigma_min, inp.sigma_max_poly);
            (s, false, "Poly IV (EMA)", self.sigma_ema)
        } else if inp.steady && iv_raw > 0.01 {
            let s = iv_raw.clamp(inp.sigma_min, inp.sigma_max_poly);
            (s, false, "Poly IV (init)", iv_raw)
        } else if inp.sticky_sigma > 0.0 {
            (inp.sticky_sigma, false, "粘性", inp.sticky_sigma)
        } else {
            (inp.default_sigma, true, "默认", inp.sticky_sigma)
        };

        let fair_up =
            bs_model::calculate_binary_call_price(inp.binance_mid, inp.strike, t_years, sigma);
        let fair_down = (1.0 - fair_up).clamp(0.0, 1.0);

        let iv_poly =
            bs_model::find_implied_volatility(inp.binance_mid, inp.strike, t_years, inp.poly_p);

        let fv_up_rising = is_rising(&self.fv_up_history, fair_up);
        let fv_down_rising = is_rising(&self.fv_down_history, fair_down);
        push_truncate(&mut self.fv_up_history, fair_up, FV_HISTORY_LEN);
        push_truncate(&mut self.fv_down_history, fair_down, FV_HISTORY_LEN);

        FvResult {
            fair_up,
            fair_down,
            sigma,
            sigma_used_default,
            sigma_source,
            new_sticky,
            iv_raw,
            iv_poly,
            fv_up_rising,
            fv_down_rising,
        }
    }
}

fn is_rising(history: &VecDeque<f64>, current: f64) -> bool {
    if history.len() < 3 {
        return false;
    }
    let mean: f64 = history.iter().sum::<f64>() / history.len() as f64;
    current > mean
}

fn push_truncate(history: &mut VecDeque<f64>, value: f64, max_len: usize) {
    history.push_back(value);
    while history.len() > max_len {
        history.pop_front();
    }
}
