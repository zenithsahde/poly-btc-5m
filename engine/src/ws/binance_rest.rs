/// 币安 REST：获取指定时刻的现货价格，用作 5m 行权价 K（秒级优先，无成交则用 1m 开盘价）
use anyhow::{Context, Result};
use reqwest::Client;

/// 秒级取价：优先用 unix_ts_secs 这一秒内的第一笔成交价；若无成交则回退到该分钟 1m K 线开盘价
pub async fn get_spot_price_at_time(
    base_url: &str,
    symbol: &str,
    unix_ts_secs: i64,
) -> Result<f64> {
    let start_ms = unix_ts_secs * 1000;
    let end_ms = start_ms + 1000; // 仅取这一秒
    let url = format!(
        "{}/api/v3/aggTrades?symbol={}&startTime={}&endTime={}&limit=1",
        base_url.trim_end_matches('/'),
        symbol,
        start_ms,
        end_ms
    );
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("构建 reqwest Client 失败")?;
    let res = client.get(&url).send().await.context("请求币安 aggTrades 失败")?;
    let json: Vec<serde_json::Value> = res.json().await.context("解析 aggTrades JSON 失败")?;
    if let Some(first) = json.first() {
        let p = first
            .get("p")
            .and_then(|v| v.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| v.as_f64()))
            .context("aggTrades 首笔价格缺失")?;
        return Ok(p);
    }
    // 该秒内无成交，回退到该分钟 1m K 线开盘价
    get_spot_open_at_time(base_url, symbol, unix_ts_secs).await
}

/// 获取 unix_ts_secs 所在分钟的那根 1m K 线的开盘价（备用 / 回退）
async fn get_spot_open_at_time(
    base_url: &str,
    symbol: &str,
    unix_ts_secs: i64,
) -> Result<f64> {
    let start_time_ms = unix_ts_secs * 1000;
    let url = format!(
        "{}/api/v3/klines?symbol={}&interval=1m&startTime={}&limit=1",
        base_url.trim_end_matches('/'),
        symbol,
        start_time_ms
    );
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("构建 reqwest Client 失败")?;
    let res = client.get(&url).send().await.context("请求币安 klines 失败")?;
    let json: Vec<Vec<serde_json::Value>> = res.json().await.context("解析 klines JSON 失败")?;
    let candle = json
        .into_iter()
        .next()
        .context("klines 返回为空（可能该时刻尚未有数据）")?;
    let open = candle
        .get(1)
        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok())))
        .context("klines 开盘价缺失或无法解析")?;
    Ok(open)
}
