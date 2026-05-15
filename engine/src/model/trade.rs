/// model/trade.rs - 聚合成交流数据结构
/// 对应币安 <symbol>@aggTrade 流
use serde::Deserialize;

/// 币安 aggTrade WebSocket 消息（组合流格式）
#[derive(Debug, Deserialize, Clone)]
pub struct AggTradeEvent {
    pub stream: String,
    pub data: AggTradeData,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AggTradeData {
    /// 事件类型
    #[serde(rename = "e")]
    pub event_type: String,

    /// 事件时间（ms）
    #[serde(rename = "E")]
    pub event_time: u64,

    /// 交易对
    #[serde(rename = "s")]
    pub symbol: String,

    /// 聚合成交 ID
    #[serde(rename = "a")]
    pub agg_trade_id: u64,

    /// 成交价格
    #[serde(rename = "p")]
    pub price: String,

    /// 成交数量
    #[serde(rename = "q")]
    pub quantity: String,

    /// 第一个成交 ID
    #[serde(rename = "f")]
    pub first_trade_id: u64,

    /// 最后一个成交 ID
    #[serde(rename = "l")]
    pub last_trade_id: u64,

    /// 成交时间（ms）
    #[serde(rename = "T")]
    pub trade_time: u64,

    /// 是否为做市方卖出（true = 主动卖，false = 主动买）
    #[serde(rename = "m")]
    pub is_buyer_maker: bool,
}

/// 解析后的成交数据
#[derive(Debug, Clone)]
pub struct Trade {
    pub symbol: String,
    pub price: f64,
    pub quantity: f64,
    /// true = 主动卖（Taker Sell），false = 主动买（Taker Buy）
    pub is_buyer_maker: bool,
    /// 交易所成交时间戳（ms）
    pub trade_time_ms: u64,
    /// 本机收到时间戳（纳秒）
    pub recv_ts_ns: u128,
}

impl TryFrom<&AggTradeData> for Trade {
    type Error = anyhow::Error;

    fn try_from(d: &AggTradeData) -> Result<Self, Self::Error> {
        Ok(Trade {
            symbol: d.symbol.clone(),
            price: d.price.parse()?,
            quantity: d.quantity.parse()?,
            is_buyer_maker: d.is_buyer_maker,
            trade_time_ms: d.trade_time,
            recv_ts_ns: 0,
        })
    }
}

impl Trade {
    /// 主动买（Taker Buy）
    pub fn is_taker_buy(&self) -> bool {
        !self.is_buyer_maker
    }

    /// 成交金额（USDT）
    pub fn notional(&self) -> f64 {
        self.price * self.quantity
    }
}
