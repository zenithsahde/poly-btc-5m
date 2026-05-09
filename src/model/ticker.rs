/// model/ticker.rs - Book Ticker 最优买卖价数据结构
/// 对应币安 <symbol>@bookTicker 流
use serde::Deserialize;

/// 币安 bookTicker WebSocket 消息（组合流格式）
#[derive(Debug, Deserialize, Clone)]
pub struct BookTickerEvent {
    /// 流名称，例如 "btcusdt@bookTicker"
    pub stream: String,
    /// 消息数据
    pub data: BookTickerData,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BookTickerData {
    /// 更新序号
    #[serde(rename = "u")]
    pub update_id: u64,

    /// 交易对
    #[serde(rename = "s")]
    pub symbol: String,

    /// 最优买价（字符串，需解析为 f64）
    #[serde(rename = "b")]
    pub best_bid_price: String,

    /// 最优买量
    #[serde(rename = "B")]
    pub best_bid_qty: String,

    /// 最优卖价
    #[serde(rename = "a")]
    pub best_ask_price: String,

    /// 最优卖量
    #[serde(rename = "A")]
    pub best_ask_qty: String,
}

/// 解析后的买卖价结构（f64，方便计算）
#[derive(Debug, Clone)]
pub struct BestBidAsk {
    pub symbol: String,
    pub bid: f64,
    pub bid_qty: f64,
    pub ask: f64,
    pub ask_qty: f64,
    pub update_id: u64,
    /// 本机收到时间戳（Unix 纳秒）
    pub recv_ts_ns: u128,
}

impl TryFrom<&BookTickerData> for BestBidAsk {
    type Error = anyhow::Error;

    fn try_from(d: &BookTickerData) -> Result<Self, Self::Error> {
        Ok(BestBidAsk {
            symbol: d.symbol.clone(),
            bid: d.best_bid_price.parse()?,
            bid_qty: d.best_bid_qty.parse()?,
            ask: d.best_ask_price.parse()?,
            ask_qty: d.best_ask_qty.parse()?,
            update_id: d.update_id,
            recv_ts_ns: 0, // 由 client.rs 注入
        })
    }
}

impl BestBidAsk {
    /// 中间价
    pub fn mid_price(&self) -> f64 {
        (self.bid + self.ask) / 2.0
    }

    /// 买卖价差（绝对值）
    pub fn spread(&self) -> f64 {
        self.ask - self.bid
    }

    /// 买卖价差（bps）
    pub fn spread_bps(&self) -> f64 {
        (self.spread() / self.mid_price()) * 10_000.0
    }
}
