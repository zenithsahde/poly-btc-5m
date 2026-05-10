use alloy_primitives::U256;
use serde::{Deserialize, Serialize};
/// execution/order_manager.rs - 挂单管理器
/// 本地维护 Polymarket 所有活动挂单 (Active Orders)，支持快速检索以实现闪电撤单
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveOrder {
    pub order_hash: String,
    pub price: f64,
    pub qty: f64,
    pub side: u8, // 0=BUY, 1=SELL
    pub token_id: String,
}

pub struct OrderManager {
    /// 以 OrderHash 为 Key 的活动订单池
    orders: HashMap<String, ActiveOrder>,
}

impl OrderManager {
    pub fn new() -> Self {
        Self {
            orders: HashMap::new(),
        }
    }

    /// 记录新挂单
    pub fn add_order(&mut self, order: ActiveOrder) {
        self.orders.insert(order.order_hash.clone(), order);
    }

    /// 移除订单 (用于成交或撤单成功后)
    pub fn remove_order(&mut self, order_hash: &str) {
        self.orders.remove(order_hash);
    }

    /// 获取所有活动订单
    pub fn get_active_orders(&self) -> Vec<ActiveOrder> {
        self.orders.values().cloned().collect()
    }

    /// 获取特定价格劣后的订单 (用于批量撤单触发检查)
    pub fn get_stale_orders(&self, current_fair_price: f64, tolerance_bps: f64) -> Vec<String> {
        let mut to_cancel = Vec::new();
        for (hash, order) in &self.orders {
            let diff_bps = (order.price - current_fair_price).abs() / current_fair_price * 10000.0;

            // 如果是 Buy 单且价格远高于公允价 (Overbid)
            // 或如果是 Sell 单且价格远低于公允价 (Underask) -> 表明面临被吃单风险
            if (order.side == 0 && order.price > current_fair_price)
                || (order.side == 1 && order.price < current_fair_price)
            {
                if diff_bps > tolerance_bps {
                    to_cancel.push(hash.clone());
                }
            }
        }
        to_cancel
    }
}
