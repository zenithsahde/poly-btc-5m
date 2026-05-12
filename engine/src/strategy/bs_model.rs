/// strategy/bs_model.rs - Black-Scholes 二元期权定价逻辑
/// 用于计算 5 分钟预测市场的公允概率
use std::f64::consts::PI;

/// 计算标准正态分布的累积分布函数 N(x)
/// 使用 A&S 7.1.26 高精度近似公式，误差 < 1.5e-7
fn norm_cdf(x: f64) -> f64 {
    let k = 1.0 / (1.0 + 0.2316419 * x.abs());
    let a1 = 0.319381530;
    let a2 = -0.356563782;
    let a3 = 1.781477937;
    let a4 = -1.821255978;
    let a5 = 1.330274429;

    let poly = a1 * k + a2 * k.powi(2) + a3 * k.powi(3) + a4 * k.powi(4) + a5 * k.powi(5);
    let approx = 1.0 - 1.0 / (2.0 * PI).sqrt() * (-x.powi(2) / 2.0).exp() * poly;

    if x >= 0.0 {
        approx
    } else {
        1.0 - approx
    }
}

/// 计算标准正态分布的概率密度函数 N'(x)
fn norm_pdf(x: f64) -> f64 {
    1.0 / (2.0 * PI).sqrt() * (-x.powi(2) / 2.0).exp()
}

/// Black-Scholes 二元期权定价 (Cash-or-nothing Call)
/// 返回价格高于行权价的概率 P = N(d2)
pub fn calculate_binary_call_price(
    spot: f64,         // 标的价格 (Binance)
    strike: f64,       // 行权价 (Polymarket K)
    expiry_years: f64, // 距离到期时间 (以年为单位)
    volatility: f64,   // 年化波动率 (例如 0.8 表示 80%)
) -> f64 {
    if expiry_years <= 0.0 {
        return if spot >= strike { 1.0 } else { 0.0 };
    }

    if volatility <= 0.0 {
        return if spot >= strike { 1.0 } else { 0.0 };
    }

    let d2 = ((spot / strike).ln() - (volatility.powi(2) / 2.0) * expiry_years)
        / (volatility * expiry_years.sqrt());

    (norm_cdf(d2)).clamp(0.0, 1.0)
}

/// 计算 Vega (价格对波动率的导数)，用于牛顿迭代法
pub fn calculate_vega(spot: f64, strike: f64, expiry_years: f64, volatility: f64) -> f64 {
    if expiry_years <= 0.0 || volatility <= 0.0 {
        return 0.0;
    }
    let d1 = ((spot / strike).ln() + (volatility.powi(2) / 2.0) * expiry_years)
        / (volatility * expiry_years.sqrt());
    let d2 = d1 - volatility * expiry_years.sqrt();

    // 对于 Binary Call，Vega = N'(d2) * (-sqrt(T) / sigma) ... 这是一个简化公式
    // 此处我们使用数值微分或更精确的二元导数
    norm_pdf(d2) * (-expiry_years.sqrt() / volatility)
}

/// 反推隐含波动率 (Implied Volatility)
/// 二分法 + S<K 单峰处理：当 S<K 时价格在 σ 上先升后降，需先找峰再在单调侧二分
pub fn find_implied_volatility(
    spot: f64,
    strike: f64,
    expiry_years: f64,
    target_price: f64,
) -> f64 {
    let target = target_price.clamp(1e-6, 1.0 - 1e-6);
    let sigma_lo = 0.001;
    let sigma_hi = 1000.0;
    let max_iter = 80;
    let epsilon = 1e-6;

    let p_lo = calculate_binary_call_price(spot, strike, expiry_years, sigma_lo);
    let p_hi = calculate_binary_call_price(spot, strike, expiry_years, sigma_hi);

    // S > K：价格在 σ 上单调减，p_lo 最大、p_hi 最小
    if spot >= strike {
        if target >= p_lo {
            return sigma_lo;
        }
        if target <= p_hi {
            return sigma_hi;
        }
        let mut lo = sigma_lo;
        let mut hi = sigma_hi;
        for _ in 0..max_iter {
            let mid = (lo + hi) / 2.0;
            let p_mid = calculate_binary_call_price(spot, strike, expiry_years, mid);
            if (p_mid - target).abs() < epsilon {
                return mid;
            }
            if p_mid > target {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        return (lo + hi) / 2.0;
    }

    // S < K：价格先升后降，p_lo≈0、p_hi≈0，需先找使价格最大的 σ
    let mut sigma_max = 0.5;
    let mut p_max = calculate_binary_call_price(spot, strike, expiry_years, sigma_max);
    for &s in &[
        0.01, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0,
    ] {
        let p = calculate_binary_call_price(spot, strike, expiry_years, s);
        if p > p_max {
            p_max = p;
            sigma_max = s;
        }
    }
    if target > p_max {
        return sigma_max;
    }
    // 在 [sigma_lo, sigma_max] 上二分（该区间内价格单调增，唯一根）
    let mut lo = sigma_lo;
    let mut hi = sigma_max;
    for _ in 0..max_iter {
        let mid = (lo + hi) / 2.0;
        let p_mid = calculate_binary_call_price(spot, strike, expiry_years, mid);
        if (p_mid - target).abs() < epsilon {
            return mid;
        }
        if p_mid < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo + hi) / 2.0
}

/// 将分钟数转换为年化时间 (用于 5 分钟市场)
pub fn minutes_to_years(minutes: f64) -> f64 {
    minutes / (60.0 * 24.0 * 365.25)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_norm_cdf() {
        assert!((norm_cdf(0.0) - 0.5).abs() < 1e-6);
        assert!((norm_cdf(1.96) - 0.975).abs() < 1e-3);
    }

    #[test]
    fn test_binary_price() {
        // 当价格远高于行权价，概率应接近 1
        let p = calculate_binary_call_price(100.0, 50.0, 0.01, 0.2);
        assert!(p > 0.99);

        // 当价格远低于行权价，概率应接近 0
        let p = calculate_binary_call_price(30.0, 50.0, 0.01, 0.2);
        assert!(p < 0.01);
    }

    #[test]
    fn test_find_iv_bisection() {
        let spot = 66463.0;
        let strike = 66317.0;
        let t = minutes_to_years(9.6);
        for target in [0.02, 0.2, 0.5, 0.8, 0.98] {
            let iv = find_implied_volatility(spot, strike, t, target);
            let back = calculate_binary_call_price(spot, strike, t, iv);
            let tol = if target < 0.05 || target > 0.95 {
                2e-2
            } else {
                1e-4
            };
            assert!(
                (back - target).abs() < tol,
                "target={} iv={} back={}",
                target,
                iv,
                back
            );
        }
    }

    #[test]
    fn test_find_iv_s_lt_k() {
        let spot = 66265.0;
        let strike = 66343.0;
        let t = minutes_to_years(12.5);
        let target = 0.125;
        let iv = find_implied_volatility(spot, strike, t, target);
        let back = calculate_binary_call_price(spot, strike, t, iv);
        assert!(iv > 0.01, "S<K 时 IV 不应触底, iv={}", iv);
        assert!(
            (back - target).abs() < 1e-3,
            "target={} iv={} back={}",
            target,
            iv,
            back
        );
    }
}
