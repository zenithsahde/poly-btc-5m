use crate::position::{ManagedOrder, OrderStatus, PendingOrderReason, PositionSide};
use crate::strategy::decision::BuyIntent;
use crate::tui::app::{AppState, BookLevel, ChaseSide};

/// Deterministic dry-run execution model.
///
/// Strategy remains origin/main-compatible: it emits buy intents with the same target, qty,
/// reason, and ordering. This simulator owns the non-strategy realism: REST submit delay,
/// taker FAK fills, and quiet maker retries that keep trying without spamming cancel logs.
/// Chase can take liquidity; rebalance prefers maker to avoid paying up for the second leg.
#[derive(Clone, Debug)]
pub struct ExecutionSim {
    submit_latency_ms: i64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SimFills {
    pub up: bool,
    pub down: bool,
}

impl ExecutionSim {
    pub fn new(submit_latency_ms: i64) -> Self {
        Self { submit_latency_ms }
    }

    pub fn submit_buy_intent(&self, s: &mut AppState, intent: &BuyIntent, now_ms: i64) {
        if s.ledger.has_open_buy_order(intent.side) {
            return;
        }

        let ask = best_ask(s, intent.side);
        let prefer_maker = intent.reason == PendingOrderReason::Rebalance && !intent.force_taker;
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
        order.maker_taker = prefer_maker || (!intent.force_taker && !marketable_on_decision);
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
        force_rebalance_taker: bool,
        now_ms: i64,
    ) -> SimFills {
        SimFills {
            up: self.process_side(s, ChaseSide::Up, target_up, force_rebalance_taker, now_ms),
            down: self.process_side(
                s,
                ChaseSide::Down,
                target_down,
                force_rebalance_taker,
                now_ms,
            ),
        }
    }

    fn process_side(
        &self,
        s: &mut AppState,
        side: ChaseSide,
        current_target: f64,
        force_rebalance_taker: bool,
        now_ms: i64,
    ) -> bool {
        let Some(mut order) = take_pending_order(s, side) else {
            return false;
        };

        if force_rebalance_taker && order.reason == PendingOrderReason::Rebalance {
            order.maker_taker = false;
            order.price = 1.0;
            order.target_price = 1.0;
            order.status = OrderStatus::Submitted;
            order.exchange_arrive_ts_ms = now_ms;
            order.updated_ts_ms = now_ms;
        }

        if order.maker_taker {
            self.refresh_maker_order(&mut order, current_target, now_ms);
        }

        if matches!(order.status, OrderStatus::Submitted | OrderStatus::Created) {
            if now_ms < order.exchange_arrive_ts_ms {
                set_pending_order(s, side, Some(order));
                return false;
            }
            let filled = if order.maker_taker {
                self.fill_maker_probe(s, side, &mut order, now_ms)
            } else {
                self.fill_taker_fak(s, side, &mut order, now_ms)
            };
            if filled || !order.maker_taker {
                order.snapshot_pnl = s.net_pnl();
                s.ledger.order_history.push(order);
            } else {
                set_pending_order(s, side, Some(order));
            }
            return filled;
        }

        set_pending_order(s, side, Some(order));
        false
    }

    fn refresh_maker_order(&self, order: &mut ManagedOrder, current_target: f64, now_ms: i64) {
        if current_target <= 0.0 || (current_target - order.price).abs() < 0.01 {
            return;
        }
        order.target_price = current_target;
        order.price = current_target;
        order.status = OrderStatus::Submitted;
        order.exchange_arrive_ts_ms = now_ms + self.submit_latency_ms;
        order.updated_ts_ms = now_ms;
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

    fn fill_maker_probe(
        &self,
        s: &mut AppState,
        side: ChaseSide,
        order: &mut ManagedOrder,
        now_ms: i64,
    ) -> bool {
        let ask = best_ask(s, side);
        if ask <= 0.0 || ask > order.price {
            return false;
        }

        let asks = asks_for_side(s, side).to_vec();
        let available_qty: f64 = asks
            .iter()
            .take_while(|level| level.price <= order.price)
            .map(|level| level.qty.max(0.0))
            .sum();
        let fill_qty = order.remaining_qty().min(available_qty);
        if fill_qty <= 0.0 {
            return false;
        }

        let (ub, ua, db, da) = book_tops(s);
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
        if order.remaining_qty() > 0.0 {
            order.status = OrderStatus::Cancelled;
            order.reject_reason = Some("maker_probe_partial".to_string());
            order.updated_ts_ms = now_ms;
        }
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

fn book_tops(s: &AppState) -> (f64, f64, f64, f64) {
    (
        s.poly_best_bid,
        s.poly_best_ask,
        s.poly_down_best_bid,
        s.poly_down_best_ask,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::PendingOrderReason;

    #[test]
    fn maker_rebalance_no_fill_stays_pending_and_later_fills() {
        let sim = ExecutionSim::new(45);
        let mut s = AppState::new("BTCUSDT");
        s.poly_best_ask = 0.50;
        s.poly_asks = vec![BookLevel {
            price: 0.50,
            qty: 100.0,
        }];

        let intent = BuyIntent {
            side: ChaseSide::Up,
            qty: 100.0,
            target: 0.40,
            worst: 0.40,
            reason: PendingOrderReason::Rebalance,
            force_taker: false,
            rebalance_hint: None,
        };

        sim.submit_buy_intent(&mut s, &intent, 1_000);
        let fills = sim.process(&mut s, 0.40, 0.0, false, 1_050);
        assert!(!fills.up);
        assert!(s.ledger.has_open_buy_order(ChaseSide::Up));
        assert!(s.ledger.trades_current_window.is_empty());
        assert!(s.ledger.order_history.is_empty());

        s.poly_best_ask = 0.44;
        s.poly_asks = vec![BookLevel {
            price: 0.44,
            qty: 100.0,
        }];
        let fills = sim.process(&mut s, 0.45, 0.0, false, 1_100);
        assert!(!fills.up);
        assert!(s.ledger.has_open_buy_order(ChaseSide::Up));

        let fills = sim.process(&mut s, 0.45, 0.0, false, 1_150);
        assert!(fills.up);
        assert!(!s.ledger.has_open_buy_order(ChaseSide::Up));
        assert_eq!(s.ledger.trades_current_window.len(), 1);
        assert!(s.ledger.trades_current_window[0].maker_taker);
    }

    #[test]
    fn force_rebalance_taker_converts_existing_maker_order() {
        let sim = ExecutionSim::new(45);
        let mut s = AppState::new("BTCUSDT");
        s.poly_down_best_ask = 0.99;
        s.poly_down_asks = vec![BookLevel {
            price: 0.99,
            qty: 30.0,
        }];

        let intent = BuyIntent {
            side: ChaseSide::Down,
            qty: 29.93,
            target: 0.40,
            worst: 0.40,
            reason: PendingOrderReason::Rebalance,
            force_taker: false,
            rebalance_hint: None,
        };

        sim.submit_buy_intent(&mut s, &intent, 1_000);
        let fills = sim.process(&mut s, 0.0, 0.40, false, 1_050);
        assert!(!fills.down);
        assert!(s.ledger.has_open_buy_order(ChaseSide::Down));

        let fills = sim.process(&mut s, 0.0, 1.0, true, 1_060);
        assert!(fills.down);
        assert_eq!(s.ledger.trades_current_window.len(), 1);
        assert!(!s.ledger.trades_current_window[0].maker_taker);
        assert!((s.ledger.trades_current_window[0].qty - 29.93).abs() < 0.0001);
    }
}
