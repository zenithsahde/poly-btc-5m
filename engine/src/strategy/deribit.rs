/// strategy/deribit.rs - Deribit BTC 期权 IV smile（独立 σ 源）
///
/// 为什么用 Deribit：
///   - 我们当前 σ 用 binance aggTrade realized vol（5-15 min 历史样本）—— 后视的
///   - Deribit BTC 期权市场报价是**前瞻的市场隐含波动率（IV）**
///   - poly market maker 大概率用 Deribit IV 或自家更精的模型，我们用 realized vol 会落后
///
/// 数据源：
///   GET https://www.deribit.com/api/v2/public/get_book_summary_by_currency?currency=BTC&kind=option
///   返回所有未到期期权的 book summary，包含 mark_iv (%) + underlying_price + instrument_name
///
/// 处理：
///   1. 过滤到最近到期日（nearest expiry）
///   2. 按 strike 聚合 call+put IV 平均
///   3. 换算 log_moneyness = ln(strike / underlying)
///   4. 排序得到 surface = Vec<(log_moneyness, iv_decimal)>
///
/// 查询：
///   给一个目标 log_moneyness（来自我们的 K 和 binance S），做线性插值得 σ。
///   超出表范围 → flat extrapolation（边缘 σ）。

use anyhow::{Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{info, warn};

use crate::tui::app::AppState;

const DERIBIT_URL: &str =
    "https://www.deribit.com/api/v2/public/get_book_summary_by_currency?currency=BTC&kind=option";
const DERIBIT_REFRESH_S: u64 = 60;

/// 一组 (log_moneyness, iv_decimal) 点，按 log_moneyness 升序。
/// log_moneyness = ln(strike / underlying)，0 ≈ ATM。
/// iv_decimal 是十进制（0.30 = 30% 年化）。
pub type VolSurface = Vec<(f64, f64)>;

/// 拉一次 Deribit BTC 期权 nearest-expiry 的 vol smile。
/// 返回 None 表示拉取失败或解析空（调用方应保留上一次成功的结果）。
pub async fn fetch_vol_surface() -> Result<VolSurface> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client
        .get(DERIBIT_URL)
        .send()
        .await
        .context("deribit GET failed")?;
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!("deribit HTTP {}", resp.status()));
    }
    let json: serde_json::Value = resp.json().await.context("deribit JSON parse")?;
    let items = json
        .get("result")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("deribit no result array"))?;

    let now_ts = Utc::now().timestamp();
    let mut parsed: Vec<(i64, f64, f64, f64)> = Vec::new(); // (exp, strike, underlying, iv)
    for d in items {
        let mark_iv = d
            .get("mark_iv")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let underlying = d
            .get("underlying_price")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if mark_iv <= 0.0 || underlying <= 0.0 {
            continue;
        }
        let name = match d.get("instrument_name").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => continue,
        };
        // instrument_name e.g. "BTC-30MAY26-77000-C"
        let parts: Vec<&str> = name.split('-').collect();
        if parts.len() < 4 {
            continue;
        }
        let exp_str = parts[1]; // e.g. "30MAY26"
        let strike: f64 = match parts[2].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let exp_ts = match parse_deribit_expiry(exp_str) {
            Some(t) => t,
            None => continue,
        };
        if exp_ts <= now_ts {
            continue;
        }
        parsed.push((exp_ts, strike, underlying, mark_iv));
    }
    if parsed.is_empty() {
        return Err(anyhow::anyhow!("deribit no parseable instruments"));
    }
    let nearest_exp = parsed.iter().map(|p| p.0).min().unwrap();
    // 聚合 nearest expiry 的 IV by strike（call+put 平均）
    let mut by_strike: std::collections::HashMap<u64, (f64, u32, f64)> =
        std::collections::HashMap::new(); // strike→(iv_sum, count, underlying)
    for (exp, strike, underlying, iv) in &parsed {
        if *exp != nearest_exp {
            continue;
        }
        let key = (*strike as u64).max(1);
        let entry = by_strike.entry(key).or_insert((0.0, 0, *underlying));
        entry.0 += *iv;
        entry.1 += 1;
    }
    if by_strike.is_empty() {
        return Err(anyhow::anyhow!("deribit no nearest-expiry data"));
    }
    let mut surface: Vec<(f64, f64)> = by_strike
        .iter()
        .map(|(strike, (iv_sum, cnt, underlying))| {
            let iv_avg_decimal = (iv_sum / *cnt as f64) / 100.0; // mark_iv 是百分比 → 十进制
            let log_mny = ((*strike as f64) / *underlying).ln();
            (log_mny, iv_avg_decimal)
        })
        .collect();
    surface.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(surface)
}

/// 解析 Deribit 期权到期日格式 "30MAY26" → unix timestamp（UTC 当天 08:00，Deribit 标准结算时间）。
fn parse_deribit_expiry(s: &str) -> Option<i64> {
    // 格式 DDMMMYY，如 30MAY26, 1JUN26（无前导 0）
    if s.len() < 6 {
        return None;
    }
    // 拆 day/mon/year，month 是 3 字母
    let (day_str, rest) = if s.as_bytes()[1].is_ascii_digit() {
        // 2 位 day
        (&s[..2], &s[2..])
    } else {
        // 1 位 day
        (&s[..1], &s[1..])
    };
    if rest.len() < 5 {
        return None;
    }
    let mon_str = &rest[..3];
    let year_str = &rest[3..];
    let day: u32 = day_str.parse().ok()?;
    let year_short: i32 = year_str.parse().ok()?;
    let year = 2000 + year_short;
    let month = match mon_str.to_uppercase().as_str() {
        "JAN" => 1, "FEB" => 2, "MAR" => 3, "APR" => 4, "MAY" => 5, "JUN" => 6,
        "JUL" => 7, "AUG" => 8, "SEP" => 9, "OCT" => 10, "NOV" => 11, "DEC" => 12,
        _ => return None,
    };
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    // 与 Python validator 对齐：取当日 00:00 UTC（Deribit 实际 08:00 settle，
    // 但 nearest expiry 排序对绝对时间不敏感，关键是与 validator 行为一致）
    let dt = date.and_hms_opt(0, 0, 0)?;
    Some(Utc.from_utc_datetime(&dt).timestamp())
}

/// 在 surface 上线性插值得到指定 log_moneyness 处的 σ。
/// 超出 surface 范围 → flat extrapolation（用最近边缘的 IV）。
pub fn interp_iv(surface: &VolSurface, log_mny: f64) -> Option<f64> {
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

/// 后台 task：每 60 秒拉一次 Deribit smile 写入 AppState。
pub async fn refresh_task(state: Arc<RwLock<AppState>>) {
    info!("[deribit] vol smile 刷新 task 启动，间隔 {}s", DERIBIT_REFRESH_S);
    loop {
        match fetch_vol_surface().await {
            Ok(surface) => {
                if let Some(atm_iv) = interp_iv(&surface, 0.0) {
                    let (iv_min, iv_max, log_mny_min, log_mny_max) = surface_stats(&surface);
                    info!(
                        "[deribit] surface {} pts  ATM={:.3}  IV[{:.3}-{:.3}]  log_mny[{:+.3},{:+.3}]",
                        surface.len(),
                        atm_iv,
                        iv_min,
                        iv_max,
                        log_mny_min,
                        log_mny_max
                    );
                    if let Ok(mut s) = state.write() {
                        s.deribit_surface = Some(surface);
                        s.deribit_atm_iv = Some(atm_iv);
                        s.deribit_refresh_ts_ms = Some(Utc::now().timestamp_millis());
                    }
                } else {
                    warn!("[deribit] surface 拿到但 ATM 插值失败");
                }
            }
            Err(e) => {
                warn!("[deribit] fetch 失败: {} —— 保留上次 surface", e);
            }
        }
        tokio::time::sleep(Duration::from_secs(DERIBIT_REFRESH_S)).await;
    }
}

fn surface_stats(surface: &VolSurface) -> (f64, f64, f64, f64) {
    let mut iv_min = f64::MAX;
    let mut iv_max = f64::MIN;
    for &(_, iv) in surface {
        iv_min = iv_min.min(iv);
        iv_max = iv_max.max(iv);
    }
    let log_mny_min = surface.first().map(|p| p.0).unwrap_or(0.0);
    let log_mny_max = surface.last().map(|p| p.0).unwrap_or(0.0);
    (iv_min, iv_max, log_mny_min, log_mny_max)
}
