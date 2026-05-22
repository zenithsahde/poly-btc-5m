use crate::tui::app::ChaseSide;

pub type PositionSide = ChaseSide;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingOrderReason {
    Chase,
    Rebalance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderStatus {
    Created,
    Submitted,
    Accepted,
    PartiallyFilled,
    Filled,
    CancelRequested,
    Cancelled,
    Rejected,
    Expired,
}

impl OrderStatus {
    pub fn is_open(self) -> bool {
        matches!(
            self,
            OrderStatus::Created
                | OrderStatus::Submitted
                | OrderStatus::Accepted
                | OrderStatus::PartiallyFilled
                | OrderStatus::CancelRequested
        )
    }
}

#[derive(Clone, Debug)]
pub struct ManagedOrder {
    pub client_order_id: String,
    pub order_hash: Option<String>,
    pub side: PositionSide,
    pub buy_sell: bool,
    pub maker_taker: bool,
    pub target_price: f64,
    pub price: f64,
    pub qty: f64,
    pub filled_qty: f64,
    pub filled_notional: f64,
    pub fill_levels: u32,
    pub placed_ts_ms: i64,
    pub updated_ts_ms: i64,
    pub exchange_arrive_ts_ms: i64,
    pub queue_ahead_qty: f64,
    pub reason: PendingOrderReason,
    pub status: OrderStatus,
    pub reject_reason: Option<String>,
}

fn side_label(side: PositionSide) -> &'static str {
    match side {
        ChaseSide::Up => "UP",
        ChaseSide::Down => "DOWN",
    }
}

impl ManagedOrder {
    pub fn new_buy(
        side: PositionSide,
        price: f64,
        qty: f64,
        placed_ts_ms: i64,
        reason: PendingOrderReason,
    ) -> Self {
        Self {
            client_order_id: format!("local-{}-{}", placed_ts_ms, side_label(side)),
            order_hash: None,
            side,
            buy_sell: true,
            maker_taker: true,
            target_price: price,
            price,
            qty,
            filled_qty: 0.0,
            filled_notional: 0.0,
            fill_levels: 0,
            placed_ts_ms,
            updated_ts_ms: placed_ts_ms,
            exchange_arrive_ts_ms: placed_ts_ms,
            queue_ahead_qty: 0.0,
            reason,
            status: OrderStatus::Created,
            reject_reason: None,
        }
    }

    pub fn remaining_qty(&self) -> f64 {
        (self.qty - self.filled_qty).max(0.0)
    }

    pub fn mark_cancel_requested(&mut self, ts_ms: i64) {
        self.status = OrderStatus::CancelRequested;
        self.updated_ts_ms = ts_ms;
    }

    pub fn mark_cancelled(&mut self, ts_ms: i64) {
        self.status = OrderStatus::Cancelled;
        self.updated_ts_ms = ts_ms;
    }

    pub fn mark_rejected(&mut self, ts_ms: i64, reason: impl Into<String>) {
        self.status = OrderStatus::Rejected;
        self.reject_reason = Some(reason.into());
        self.updated_ts_ms = ts_ms;
    }

    pub fn record_fill_at(&mut self, fill_qty: f64, fill_price: f64, ts_ms: i64) {
        self.filled_qty = (self.filled_qty + fill_qty).min(self.qty);
        self.filled_notional += fill_qty * fill_price;
        self.fill_levels = self.fill_levels.saturating_add(1);
        self.status = if self.remaining_qty() > 0.0 {
            OrderStatus::PartiallyFilled
        } else {
            OrderStatus::Filled
        };
        self.updated_ts_ms = ts_ms;
    }
}
