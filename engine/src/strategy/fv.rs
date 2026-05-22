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
    /// Deribit BTC nearest-expiry vol surface: Vec<(log_moneyness, iv_decimal)>，None = 未就绪。
    /// 优先级：deribit_smile @ log(K/S) > sigma_ema (Poly IV) > 稳态 iv_raw > 粘性 > 默认。
    pub deribit_surface: Option<Vec<(f64, f64)>>,
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

        let sigma_ema_prev = self.sigma_ema;

        // σ 优先级（v0.5.3 新增 Deribit smile 在最前）：
        //   1. Deribit IV smile @ log(K/S) — 期权市场前瞻波动率，与 poly MM 同水平
        //   2. Poly IV EMA — 从 poly_p 反解的隐含波动率 EMA
        //   3. 稳态 Poly IV 初值
        //   4. 粘性 σ
        //   5. 默认 σ
        let deribit_sigma = inp.deribit_surface.as_ref().and_then(|surf| {
            if inp.binance_mid > 0.0 && inp.strike > 0.0 {
                let log_mny = (inp.strike / inp.binance_mid).ln();
                interp_iv_local(surf, log_mny)
            } else {
                None
            }
        });

        let (sigma, sigma_used_default, sigma_source, new_sticky) =
            if let Some(s_deribit) = deribit_sigma {
                let s = s_deribit.clamp(inp.sigma_min, inp.sigma_max_poly);
                (s, false, "Deribit smile", inp.sticky_sigma)
            } else if sigma_ema_prev > 0.01 {
                let s = sigma_ema_prev.clamp(inp.sigma_min, inp.sigma_max_poly);
                (s, false, "Poly IV (EMA)", sigma_ema_prev)
            } else if inp.steady && iv_raw > 0.01 {
                let s = iv_raw.clamp(inp.sigma_min, inp.sigma_max_poly);
                (s, false, "Poly IV (init)", iv_raw)
            } else if inp.sticky_sigma > 0.0 {
                (inp.sticky_sigma, false, "粘性", inp.sticky_sigma)
            } else {
                (inp.default_sigma, true, "默认", inp.sticky_sigma)
            };

        // 与 origin/main 对齐：当前 FV 使用上一帧 sigma_ema；随后才把本帧稳态 iv_raw 喂 EMA。
        if inp.steady && iv_raw > 0.01 && iv_raw < 5.0 {
            if self.sigma_ema <= 0.0 {
                self.sigma_ema = iv_raw;
            } else {
                self.sigma_ema =
                    SIGMA_EMA_ALPHA * iv_raw + (1.0 - SIGMA_EMA_ALPHA) * self.sigma_ema;
            }
        }

        let fair_up =
            bs_model::calculate_binary_call_price(inp.binance_mid, inp.strike, t_years, sigma);
        let fair_down = (1.0 - fair_up).clamp(0.0, 1.0);

        let iv_poly =
            bs_model::find_implied_volatility(inp.binance_mid, inp.strike, t_years, inp.poly_p);

        push_truncate(&mut self.fv_up_history, fair_up, FV_HISTORY_LEN);
        push_truncate(&mut self.fv_down_history, fair_down, FV_HISTORY_LEN);
        let fv_up_rising = is_rising(&self.fv_up_history, fair_up);
        let fv_down_rising = is_rising(&self.fv_down_history, fair_down);

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

/// Deribit smile 线性插值（独立副本以避免 fv ↔ deribit 模块循环依赖）。
fn interp_iv_local(surface: &[(f64, f64)], log_mny: f64) -> Option<f64> {
    if surface.is_empty() {
        return None;
    }
    if log_mny <= surface[0].0 {
        return Some(surface[0].1);
    }
    if log_mny >= surface[surface.len() - 1].0 {
        return Some(surface[surface.len() - 1].1);
    }
    for i in 1..surface.len() {
        let (x0, y0) = surface[i - 1];
        let (x1, y1) = surface[i];
        if x0 <= log_mny && log_mny <= x1 {
            if (x1 - x0).abs() < 1e-12 {
                return Some(y0);
            }
            let t = (log_mny - x0) / (x1 - x0);
            return Some(y0 + t * (y1 - y0));
        }
    }
    None
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
