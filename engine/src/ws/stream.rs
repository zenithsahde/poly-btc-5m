use crate::model::{
    chainlink::ChainlinkPriceData, orderbook::DepthData, ticker::BookTickerData,
    trade::AggTradeData,
};
/// ws/stream.rs - 市场数据流事件类型
/// 统一描述来自所有订阅流的消息
use serde::Deserialize;

/// 解析后的市场数据流事件
#[derive(Debug, Clone)]
pub enum MarketEvent {
    /// 最优买卖价更新（来自 bookTicker 流）
    BookTicker {
        data: BookTickerData,
        recv_ts_ns: u128,
    },
    /// 订单簿差量更新（来自 depth 流）
    Depth { data: DepthData, recv_ts_ns: u128 },
    /// 聚合成交（来自 aggTrade 流）
    AggTrade {
        data: AggTradeData,
        recv_ts_ns: u128,
    },
    /// Polymarket 订单簿更新 (来自 CLOB WS)
    PolyBookUpdate {
        data: PolyBookData,
        recv_ts_ns: u128,
    },
    /// Chainlink Data Streams 推价（来自 wss://ws.dataengine.chain.link/api/v1/ws）
    /// 是 Polymarket 实际结算源；用于对齐 K / S / 结算 PnL，并作为 binance jump alpha
    /// 的 "follow-target"（数据证 binance ±$30/1s 后 chainlink 5s 内 ~126% pass-through）。
    ChainlinkPrice {
        data: ChainlinkPriceData,
        recv_ts_ns: u128,
    },
    /// 未知消息类型（记录 raw，方便调试）
    Unknown { raw: String },
}

fn deserialize_asset_id<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = <serde_json::Value as Deserialize>::deserialize(d)?;
    Ok(match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    })
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PolyBookData {
    #[serde(deserialize_with = "deserialize_asset_id")]
    pub asset_id: String,
    #[serde(default)]
    pub market: Option<String>,
    pub bids: Vec<PolyBookLevel>,
    pub asks: Vec<PolyBookLevel>,
    /// 最后一笔成交价（book 事件带）
    #[serde(default, rename = "last_trade_price")]
    pub last_trade_price: Option<String>,
    #[serde(default, rename = "tick_size")]
    pub tick_size: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PolyBookLevel {
    pub price: String,
    pub size: String,
}

/// 币安组合流通用消息包装
/// {"stream": "btcusdt@bookTicker", "data": {...}}
#[derive(Debug, serde::Deserialize)]
pub struct CombinedStreamMsg {
    pub stream: String,
    pub data: serde_json::Value,
}

impl CombinedStreamMsg {
    /// 判断流类型并解析为 MarketEvent
    pub fn parse_event(self, recv_ts_ns: u128) -> MarketEvent {
        let stream = &self.stream;

        if stream.contains("@bookTicker") {
            match serde_json::from_value::<BookTickerData>(self.data) {
                Ok(data) => MarketEvent::BookTicker { data, recv_ts_ns },
                Err(e) => MarketEvent::Unknown { raw: e.to_string() },
            }
        } else if stream.contains("@depth") {
            match serde_json::from_value::<DepthData>(self.data) {
                Ok(data) => MarketEvent::Depth { data, recv_ts_ns },
                Err(e) => MarketEvent::Unknown { raw: e.to_string() },
            }
        } else if stream.contains("@aggTrade") {
            match serde_json::from_value::<AggTradeData>(self.data) {
                Ok(data) => MarketEvent::AggTrade { data, recv_ts_ns },
                Err(e) => MarketEvent::Unknown { raw: e.to_string() },
            }
        } else {
            MarketEvent::Unknown {
                raw: format!("unknown stream: {}", stream),
            }
        }
    }
}
