use crate::execution::merge::MergeOutcome;
use crate::position::PendingOrderReason;
use crate::tui::app::{AppState, ChaseSide};
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
    /// 服务端回包的 makingAmount（字符串原值经 parse；缺失则 None）
    pub making_amount: Option<f64>,
    /// 服务端回包的 takingAmount（FAK 部分成交的成交量在这里）
    pub taking_amount: Option<f64>,
    /// 服务端附带的链上交易 hash 数组（unmatched 时缺，故空）
    pub transactions_hashes: Vec<String>,
    /// 服务端附带的 trade_ids 数组（用于回灌 / 调试）
    pub trade_ids: Vec<String>,
    /// 失败 (400/5xx) 或服务端返回 success=false 时的 errorMsg
    pub error_msg: Option<String>,
    pub elapsed: u64,
}

/// Strategy-emitted buy intent. ExecutionSim/LiveOrderClient consume it and create
/// the actual ManagedOrder placeholder in AppState.
#[derive(Debug, Clone)]
pub struct BuyIntent {
    pub side: ChaseSide,
    pub qty: f64,
    pub target: f64,
    pub reason: PendingOrderReason,
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

    /// (up_filled, down_filled): dry-run advances via ExecutionSim; live always returns (false, false).
    fn tick(
        &self,
        state: &mut AppState,
        target_up: f64,
        target_down: f64,
        now_ms: i64,
    ) -> (bool, bool);

    /// 默认给 dry-run 用：返回零哈希，表示「虚拟兑现」。LiveOrderClient 会覆写走 on-chain / relayer。
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
