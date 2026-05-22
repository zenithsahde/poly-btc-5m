use crate::position::PendingOrderReason;
use crate::tui::app::{AppState, ChaseSide};
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

#[derive(Debug, Clone)]
pub struct PlaceOrderResult {
    pub order_id: String,
    pub success: bool,
    pub status: String,
    pub filled_price: Option<f64>,
    pub filled_size: Option<f64>,
    pub error: Option<String>,
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
}
