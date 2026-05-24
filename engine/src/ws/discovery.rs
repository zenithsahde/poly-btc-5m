/// ws/discovery.rs - Polymarket 市场自动发现
/// 自动获取当前/下一 5m BTC 预测市场 Token ID，并返回窗口结束时间用于自动切换
use alloy_primitives::B256;
use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize)]
pub struct GammaMarket {
    pub id: String,
    pub question: String,
    pub slug: String,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: String,
    pub active: bool,
    #[serde(rename = "endDate")]
    pub end_date: String,
    #[serde(rename = "conditionId", default)]
    pub condition_id: String,
}

/// 当前 5m 市场信息：Up + Down 双 token，用于完整订单簿订阅
#[derive(Debug, Clone)]
pub struct Active5mMarket {
    pub up_token_id: String,
    pub down_token_id: String,
    pub slug: String,
    pub condition_id: B256,
    /// Unix 秒，本 5 分钟窗口结束时间（即下一窗口开始时间）
    pub window_end_ts: i64,
}

fn parse_condition_id(raw: &str) -> Result<B256> {
    let s = raw.strip_prefix("0x").unwrap_or(raw);
    let bytes = hex::decode(s).with_context(|| format!("非法 conditionId hex: {raw}"))?;
    if bytes.len() != 32 {
        anyhow::bail!("conditionId 长度异常: {} 字节，期望 32", bytes.len());
    }
    Ok(B256::from_slice(&bytes))
}

impl Active5mMarket {
    /// 两个 token 的数组，用于 WS 订阅 [up, down]
    pub fn asset_ids(&self) -> [&str; 2] {
        [self.up_token_id.as_str(), self.down_token_id.as_str()]
    }
}

pub struct MarketDiscovery;

impl MarketDiscovery {
    /// 获取当前活跃的 5m BTC 市场及窗口结束时间
    /// 返回 (token_id, slug, window_end_ts)，window_end_ts 用于自动切换到下一 5m 市场
    pub async fn get_active_5m_btc_market() -> Result<Active5mMarket> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;

        let now_ts = chrono::Utc::now().timestamp();
        let current_window_start = (now_ts / 300) * 300;
        let window_end_ts = current_window_start + 300;
        let expected_slug = format!("btc-updown-5m-{}", current_window_start);

        info!(
            "当前时间戳: {}, 预期 Slug: {}, 窗口结束: {}",
            now_ts, expected_slug, window_end_ts
        );

        let url = format!(
            "https://gamma-api.polymarket.com/markets?slug={}",
            expected_slug
        );
        let response = client.get(&url).send().await?;
        let markets: Vec<GammaMarket> = response.json().await?;

        let target = markets
            .into_iter()
            .next()
            .context(format!("未找到预期 Slug 为 {} 的市场", expected_slug))?;
        info!(
            "🎯 精准锁定实时市场: {} (Token: {})",
            target.slug, target.clob_token_ids
        );

        let token_ids: Vec<String> =
            serde_json::from_str(&target.clob_token_ids).context("解析 clobTokenIds 失败")?;
        let up_token = token_ids.get(0).cloned().context("未找到 Up Token ID")?;
        let down_token = token_ids.get(1).cloned().context("未找到 Down Token ID")?;
        let condition_id = parse_condition_id(&target.condition_id)
            .with_context(|| format!("解析 conditionId 失败: slug={}", target.slug))?;

        Ok(Active5mMarket {
            up_token_id: up_token,
            down_token_id: down_token,
            slug: target.slug,
            condition_id,
            window_end_ts,
        })
    }

    /// 根据窗口开始时间戳获取该 5m 市场（用于预取下一档，到点直接切不调 API）
    pub async fn get_market_for_window(window_start_ts: i64) -> Result<Active5mMarket> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let window_end_ts = window_start_ts + 300;
        let expected_slug = format!("btc-updown-5m-{}", window_start_ts);
        let url = format!(
            "https://gamma-api.polymarket.com/markets?slug={}",
            expected_slug
        );
        let response = client.get(&url).send().await?;
        let markets: Vec<GammaMarket> = response.json().await?;
        let target = markets
            .into_iter()
            .next()
            .context(format!("未找到 Slug {}", expected_slug))?;
        let token_ids: Vec<String> =
            serde_json::from_str(&target.clob_token_ids).context("解析 clobTokenIds 失败")?;
        let up_token = token_ids.get(0).cloned().context("未找到 Up Token ID")?;
        let down_token = token_ids.get(1).cloned().context("未找到 Down Token ID")?;
        let condition_id = parse_condition_id(&target.condition_id)
            .with_context(|| format!("解析 conditionId 失败: slug={}", target.slug))?;
        Ok(Active5mMarket {
            up_token_id: up_token,
            down_token_id: down_token,
            slug: target.slug,
            condition_id,
            window_end_ts,
        })
    }

    /// 距离 window_end_ts 的秒数（用于 TUI 倒计时）
    pub fn seconds_until_next_window(window_end_ts: i64) -> i64 {
        (window_end_ts - chrono::Utc::now().timestamp()).max(0)
    }
}
