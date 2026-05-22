use anyhow::{anyhow, Result};
use ordered_float::OrderedFloat;
use serde::Deserialize;
use std::collections::BTreeMap;
use tracing::warn;

// ──────────────────────────────────────
// 币安 depth WebSocket 消息结构
// ──────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
pub struct DepthData {
    #[serde(rename = "e")]
    pub event_type: String,

    #[serde(rename = "E")]
    pub event_time: u64,

    #[serde(rename = "s")]
    pub symbol: String,

    /// 第一个更新 ID（用于与快照同步校验）
    #[serde(rename = "U")]
    pub first_update_id: u64,

    /// 最后一个更新 ID
    #[serde(rename = "u")]
    pub final_update_id: u64,

    /// 买单差量 [price, qty]（qty=0 表示删除）
    #[serde(rename = "b")]
    pub bids: Vec<[String; 2]>,

    /// 卖单差量
    #[serde(rename = "a")]
    pub asks: Vec<[String; 2]>,
}

// ──────────────────────────────────────
// 本地订单簿模型
// ──────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LocalOrderBook {
    pub symbol: String,
    /// 价格 -> 数量
    bids: BTreeMap<OrderedFloat<f64>, f64>,
    asks: BTreeMap<OrderedFloat<f64>, f64>,
    pub last_update_id: u64,
    depth_limit: usize,
}

impl LocalOrderBook {
    pub fn new(symbol: &str, depth_limit: usize) -> Self {
        Self {
            symbol: symbol.to_string(),
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            last_update_id: 0,
            depth_limit,
        }
    }

    /// 应用深度差量更新
    pub fn apply_depth_update(&mut self, data: &DepthData) -> Result<()> {
        // 校验 Final Update ID 连续性
        if self.last_update_id > 0 && data.first_update_id > self.last_update_id + 1 {
            warn!(
                "OrderBook gap detected! expected={} got={}",
                self.last_update_id + 1,
                data.first_update_id
            );
            self.bids.clear();
            self.asks.clear();
            self.last_update_id = 0;
            return Err(anyhow!("orderbook_gap"));
        }

        // 应用买单差量
        for entry in &data.bids {
            let price: f64 = entry[0].parse()?;
            let qty: f64 = entry[1].parse()?;
            let key = OrderedFloat(price);
            if qty == 0.0 {
                self.bids.remove(&key);
            } else {
                self.bids.insert(key, qty);
            }
        }

        // 应用卖单差量
        for entry in &data.asks {
            let price: f64 = entry[0].parse()?;
            let qty: f64 = entry[1].parse()?;
            let key = OrderedFloat(price);
            if qty == 0.0 {
                self.asks.remove(&key);
            } else {
                self.asks.insert(key, qty);
            }
        }

        // 裁减深度
        self.trim();

        self.last_update_id = data.final_update_id;
        Ok(())
    }

    pub fn best_bid(&self) -> Option<(f64, f64)> {
        self.bids
            .iter()
            .next_back()
            .map(|(p, q)| (p.into_inner(), *q))
    }

    pub fn best_ask(&self) -> Option<(f64, f64)> {
        self.asks.iter().next().map(|(p, q)| (p.into_inner(), *q))
    }

    pub fn mid_price(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some((bid, _)), Some((ask, _))) => Some((bid + ask) / 2.0),
            _ => None,
        }
    }

    pub fn spread(&self) -> Option<f64> {
        match (self.best_bid(), self.best_ask()) {
            (Some((bid, _)), Some((ask, _))) => Some(ask - bid),
            _ => None,
        }
    }

    pub fn spread_bps(&self) -> Option<f64> {
        let spread = self.spread()?;
        let mid = self.mid_price()?;
        Some((spread / mid) * 10_000.0)
    }

    pub fn top_bids(&self, n: usize) -> Vec<(f64, f64)> {
        self.bids
            .iter()
            .rev()
            .take(n)
            .map(|(p, q)| (p.into_inner(), *q))
            .collect()
    }

    pub fn top_asks(&self, n: usize) -> Vec<(f64, f64)> {
        self.asks
            .iter()
            .take(n)
            .map(|(p, q)| (p.into_inner(), *q))
            .collect()
    }

    fn trim(&mut self) {
        while self.bids.len() > self.depth_limit {
            self.bids.pop_first();
        }
        while self.asks.len() > self.depth_limit {
            self.asks.pop_last();
        }
    }

    pub fn is_ready(&self) -> bool {
        !self.bids.is_empty() && !self.asks.is_empty()
    }
}
