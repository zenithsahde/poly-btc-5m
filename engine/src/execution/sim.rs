use anyhow::Result;
use async_trait::async_trait;

use crate::execution::client::{OrderClient, PlaceOrderRequest, PlaceOrderResult};
use crate::position::{ManagedOrder, OrderStatus, PositionSide};
use crate::strategy::decision::BuyIntent;
use crate::tui::app::{AppState, BookLevel, ChaseSide};

/// Deterministic dry-run execution model.
///
/// Strategy remains origin/main-compatible: it emits buy intents with the same target, qty,
/// reason, and ordering. This simulator owns the non-strategy realism: REST submit delay,
/// maker queue position, FAK-style taker fills, cancel latency, and cancel/fill races.
#[derive(Clone, Debug)]
pub struct ExecutionSim {
    submit_latency_ms: i64,
    cancel_latency_ms: i64,
    maker_timeout_ms: i64,
    queue_touch_qty: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SimFills {
    pub up: bool,
    pub down: bool,
}

impl ExecutionSim {
    pub fn new(submit_latency_ms: i64, cancel_latency_ms: i64, maker_timeout_ms: i64) -> Self {
        Self {
            submit_latency_ms,
            cancel_latency_ms,
            maker_timeout_ms,
            // Realistic-conservative queue proxy: one qualifying touch consumes roughly one
            // strategy clip ahead of us. It still prevents instant maker fills, but avoids
            // making thin 5m books effectively unfillable.
            queue_touch_qty: 100.0,
        }
    }

    pub fn submit_buy_intent(&self, s: &mut AppState, intent: &BuyIntent, now_ms: i64) {
        if s.ledger.has_open_buy_order(intent.side) {
            return;
        }

        let ask = best_ask(s, intent.side);
        let marketable_on_decision = ask > 0.0 && ask <= intent.target;
        let mut order = s.ledger.create_managed_buy_order(
            intent.side,
            intent.target,
            intent.qty,
            now_ms,
            intent.reason,
        );
        order.target_price = intent.target;
        order.price = intent.target;
        order.maker_taker = !marketable_on_decision;
        order.status = OrderStatus::Submitted;
        order.exchange_arrive_ts_ms = now_ms + self.submit_latency_ms;
        order.updated_ts_ms = now_ms;

        set_pending_order(s, intent.side, Some(order));
    }

    pub fn process(
        &self,
        s: &mut AppState,
        target_up: f64,
        target_down: f64,
        now_ms: i64,
    ) -> SimFills {
        SimFills {
            up: self.process_side(s, ChaseSide::Up, target_up, now_ms),
            down: self.process_side(s, ChaseSide::Down, target_down, now_ms),
        }
    }

    fn process_side(
        &self,
        s: &mut AppState,
        side: ChaseSide,
        current_target: f64,
        now_ms: i64,
    ) -> bool {
        let Some(mut order) = take_pending_order(s, side) else {
            return false;
        };

        if matches!(order.status, OrderStatus::Submitted | OrderStatus::Created) {
            if now_ms < order.exchange_arrive_ts_ms {
                set_pending_order(s, side, Some(order));
                return false;
            }
            if order.maker_taker {
                if self.accept_maker_or_reject_cross(s, side, &mut order, now_ms) {
                    s.ledger.order_history.push(order);
                    return false;
                }
            } else {
                let filled = self.fill_taker_fak(s, side, &mut order, now_ms);
                s.ledger.order_history.push(order);
                return filled;
            }
        }

        let fill_before_cancel = self.try_fill_maker(s, side, &mut order, now_ms);
        if fill_before_cancel {
            s.ledger.order_history.push(order);
            return true;
        }

        if order.status == OrderStatus::CancelRequested {
            if now_ms - order.updated_ts_ms >= self.cancel_latency_ms {
                order.mark_cancelled(now_ms);
                s.ledger.order_history.push(order);
            } else {
                set_pending_order(s, side, Some(order));
            }
            return false;
        }

        if now_ms - order.placed_ts_ms > self.maker_timeout_ms {
            order.mark_cancel_requested(now_ms);
            order.reject_reason = Some("ttl_expired".to_string());
            set_pending_order(s, side, Some(order));
            return false;
        }

        if current_target > 0.0 && (current_target - order.price).abs() >= 0.02 {
            order.mark_cancel_requested(now_ms);
            order.reject_reason = Some("reprice".to_string());
            set_pending_order(s, side, Some(order));
            return false;
        }

        set_pending_order(s, side, Some(order));
        false
    }

    fn accept_maker_or_reject_cross(
        &self,
        s: &mut AppState,
        side: ChaseSide,
        order: &mut ManagedOrder,
        now_ms: i64,
    ) -> bool {
        let ask = best_ask(s, side);
        if ask > 0.0 && ask <= order.price {
            order.mark_rejected(now_ms, "post_only_cross");
            return true;
        }
        order.status = OrderStatus::Accepted;
        order.updated_ts_ms = now_ms;
        order.queue_ahead_qty = queue_ahead_at_price(s, side, order.price);
        false
    }

    fn fill_taker_fak(
        &self,
        s: &mut AppState,
        side: ChaseSide,
        order: &mut ManagedOrder,
        now_ms: i64,
    ) -> bool {
        let asks = asks_for_side(s, side).to_vec();
        if asks.is_empty() {
            order.mark_cancelled(now_ms);
            order.reject_reason = Some("empty_book_on_arrival".to_string());
            return false;
        }

        let (ub, ua, db, da) = book_tops(s);
        let mut remaining = order.remaining_qty();
        for level in asks.iter() {
            if remaining <= 0.0 || level.price > order.price {
                break;
            }
            if level.qty <= 0.0 {
                continue;
            }
            let take = remaining.min(level.qty);
            s.apply_fill(side, true, false, level.price, take, now_ms, ub, ua, db, da);
            order.record_fill_at(take, level.price, now_ms);
            remaining -= take;
        }

        if order.filled_qty <= 0.0 {
            order.mark_cancelled(now_ms);
            order.reject_reason = Some("not_marketable_on_arrival".to_string());
            return false;
        }
        if order.remaining_qty() > 0.0 {
            order.status = OrderStatus::Cancelled;
            order.reject_reason = Some("fak_remainder".to_string());
            order.updated_ts_ms = now_ms;
        }
        true
    }

    fn try_fill_maker(
        &self,
        s: &mut AppState,
        side: ChaseSide,
        order: &mut ManagedOrder,
        now_ms: i64,
    ) -> bool {
        if !matches!(
            order.status,
            OrderStatus::Accepted | OrderStatus::PartiallyFilled | OrderStatus::CancelRequested
        ) {
            return false;
        }
        let ask = best_ask(s, side);
        if ask <= 0.0 || ask > order.price {
            return false;
        }

        if order.queue_ahead_qty > 0.0 {
            order.queue_ahead_qty = (order.queue_ahead_qty - self.queue_touch_qty).max(0.0);
            order.updated_ts_ms = now_ms;
            if order.queue_ahead_qty > 0.0 {
                return false;
            }
        }

        let (ub, ua, db, da) = book_tops(s);
        let fill_qty = order.remaining_qty();
        if fill_qty <= 0.0 {
            return false;
        }
        s.apply_fill(
            side,
            true,
            true,
            order.price,
            fill_qty,
            now_ms,
            ub,
            ua,
            db,
            da,
        );
        order.record_fill_at(fill_qty, order.price, now_ms);
        true
    }
}

fn take_pending_order(s: &mut AppState, side: ChaseSide) -> Option<ManagedOrder> {
    match side {
        PositionSide::Up => s.ledger.maker_buy_intent_up.take(),
        PositionSide::Down => s.ledger.maker_buy_intent_down.take(),
    }
}

fn set_pending_order(s: &mut AppState, side: ChaseSide, order: Option<ManagedOrder>) {
    match side {
        PositionSide::Up => s.ledger.maker_buy_intent_up = order,
        PositionSide::Down => s.ledger.maker_buy_intent_down = order,
    }
}

fn best_ask(s: &AppState, side: ChaseSide) -> f64 {
    match side {
        PositionSide::Up => s.poly_best_ask,
        PositionSide::Down => s.poly_down_best_ask,
    }
}

fn asks_for_side(s: &AppState, side: ChaseSide) -> &[BookLevel] {
    match side {
        PositionSide::Up => &s.poly_asks,
        PositionSide::Down => &s.poly_down_asks,
    }
}

fn bids_for_side(s: &AppState, side: ChaseSide) -> &[BookLevel] {
    match side {
        PositionSide::Up => &s.poly_bids,
        PositionSide::Down => &s.poly_down_bids,
    }
}

fn queue_ahead_at_price(s: &AppState, side: ChaseSide, price: f64) -> f64 {
    bids_for_side(s, side)
        .iter()
        .filter(|level| (level.price - price).abs() < 0.0001)
        .map(|level| level.qty)
        .sum()
}

fn book_tops(s: &AppState) -> (f64, f64, f64, f64) {
    (
        s.poly_best_bid,
        s.poly_best_ask,
        s.poly_down_best_bid,
        s.poly_down_best_ask,
    )
}

#[async_trait]
impl OrderClient for ExecutionSim {
    async fn place_order(&self, _req: PlaceOrderRequest) -> Result<PlaceOrderResult> {
        Ok(PlaceOrderResult {
            order_id: "dry".to_string(),
            success: true,
            status: "dry_run".to_string(),
            filled_price: None,
            filled_size: None,
            error: None,
            elapsed: 0,
        })
    }

    async fn cancel_order(&self, _order_id: &str) -> Result<bool> {
        Ok(true)
    }

    fn dispatch_buy_intent(
        &self,
        intent: &BuyIntent,
        _best_ask: f64,
        _token_id: &str,
        now_ms: i64,
        state: &mut AppState,
    ) {
        self.submit_buy_intent(state, intent, now_ms);
    }

    fn tick(
        &self,
        state: &mut AppState,
        target_up: f64,
        target_down: f64,
        now_ms: i64,
    ) -> (bool, bool) {
        let fills = self.process(state, target_up, target_down, now_ms);
        (fills.up, fills.down)
    }
}
