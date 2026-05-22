/// model/chainlink.rs - Chainlink Data Streams 推价数据结构
///
/// 数据来自 wss://ws.dataengine.chain.link/api/v1/ws；服务端推送的是一个 base64
/// encoded ReportBlob，经 HMAC 校验后用 ABI 解码出 `ReportDataV3` 9 字段。
/// 我们只保留交易决策需要的几项：benchmark price、bid、ask、observation timestamp。
///
/// 价格 1e18 scaling（chainlink 标准）；解码端会乘以 1e-18 还原成普通 f64 USD。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainlinkPriceData {
    /// feedID hex（含 0x 前缀）；用于多 feed 时识别来源。
    pub feed_id: String,
    /// benchmarkPrice，USD（已乘 1e-18），ReportDataV3 第 7 个 word。
    pub price: f64,
    /// 买价；通常 ≈ price，但极端波动时会有 spread。
    pub bid: f64,
    /// 卖价；同上。
    pub ask: f64,
    /// observationsTimestamp，unix 秒；chainlink 报价的"参考时刻"。
    pub observation_ts: i64,
    /// validFrom，unix 秒；report 有效起始时间。
    pub valid_from: i64,
    /// expiresAt，unix 秒；report 过期时间。
    pub expires_at: i64,
}

impl ChainlinkPriceData {
    /// 与 binance mid 的 basis，正值=binance 高 chainlink 低。
    /// 用于"币安先动 chainlink 跟"alpha 触发器：basis 绝对值变大说明 chainlink 还没跟上。
    pub fn basis_vs(&self, other_price: f64) -> f64 {
        other_price - self.price
    }
}
