use crate::execution::merge::MergeOutcome;
use crate::strategy::decision::BuyIntent;
use crate::tui::app::AppState;
use alloy_primitives::B256;
use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Fak,
    Gtc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone)]
pub struct PlaceOrderRequest {
    pub side: OrderSide,
    pub token_id: String,
    pub price: f64,
    pub size_shares: f64,
    pub order_type: OrderType,
}

#[derive(Debug, Clone, Default)]
pub struct PlaceOrderResult {
    pub order_id: String,
    pub success: bool,
    pub status: String,
    pub making_amount: Option<f64>,
    pub taking_amount: Option<f64>,
    pub transactions_hashes: Vec<String>,
    pub trade_ids: Vec<String>,
    pub error_msg: Option<String>,
    pub elapsed: u64,
}

#[async_trait]
pub trait OrderClient: Send + Sync {
    async fn place_order(&self, req: PlaceOrderRequest) -> anyhow::Result<PlaceOrderResult>;
    async fn cancel_order(&self, order_id: &str) -> anyhow::Result<bool>;

    fn dispatch_buy_intent(
        &self,
        intent: &BuyIntent,
        best_ask: f64,
        token_id: &str,
        now_ms: i64,
        state: &mut AppState,
    );

    // (up_filled, down_filled)：干跑由 ExecutionSim 推进；实盘恒 (false, false)。
    // force_rebalance_taker：窗口末段对配平腿强制 taker 化（dry-run 用；实盘忽略）。
    fn tick(
        &self,
        state: &mut AppState,
        target_up: f64,
        target_down: f64,
        force_rebalance_taker: bool,
        now_ms: i64,
    ) -> (bool, bool);

    // 默认实现给 dry-run 用；实盘由 LiveOrderClient 覆写走 on-chain / relayer
    async fn merge_pairs(
        &self,
        condition_id: B256,
        pair_qty: f64,
    ) -> anyhow::Result<MergeOutcome> {
        let _ = condition_id;
        Ok(MergeOutcome {
            pair_qty,
            tx_hash: B256::ZERO,
            elapsed_ms: 0,
        })
    }
}
