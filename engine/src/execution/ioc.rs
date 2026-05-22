/// execution/ioc.rs - Marketable IOC（Immediate-Or-Cancel）走簿买单执行器
///
/// 替代 v0.4.14 的"挂 maker 等 fill"模型。设计动机：
///   - empirical-lead-lag.md 实证 FV 领先 Poly 1.1–2.5s，maker 排队大概率被 adverse fill
///   - sim-fill-fixes.md 旧模型 100% 全量 + 0 排队不可信
///   - merge 套利只对"fill 价进 avg_sum < 1"敏感，所以 worst_price 直接对应套利分水岭
///
/// 行为：在给定 ask 簿上从低到高逐档吃，吃到 worst_price 为止；剩余永不留挂单。
use crate::position::{ManagedOrder, OrderStatus, PendingOrderReason};
use crate::tui::app::{AppState, BookLevel, ChaseSide};

/// IOC 买单执行结果，供调用侧根据 filled_qty 决定节流计时等副作用。
pub struct IocOutcome {
    pub filled_qty: f64,
}

/// 状态机映射：
///   - 完全成交       → Filled
///   - 部分成交       → PartiallyFilled → 同 ts 立刻 Cancelled（IOC 剩余撤销）
///   - 零成交（价高） → Cancelled，reject_reason="worst_breach"
///   - 簿空 / 入参非法 → 不创建订单，返回 filled=0
pub fn execute_ioc_buy(
    s: &mut AppState,
    side: ChaseSide,
    want_qty: f64,
    target_price: f64,
    worst_price: f64,
    reason: PendingOrderReason,
    ts_ms: i64,
) -> IocOutcome {
    if want_qty <= 0.0 || worst_price <= 0.0 {
        return IocOutcome { filled_qty: 0.0 };
    }
    let asks: Vec<BookLevel> = match side {
        ChaseSide::Up => s.poly_asks.clone(),
        ChaseSide::Down => s.poly_down_asks.clone(),
    };
    if asks.is_empty() {
        return IocOutcome { filled_qty: 0.0 };
    }

    let (ub, ua, db, da) = (
        s.poly_best_bid,
        s.poly_best_ask,
        s.poly_down_best_bid,
        s.poly_down_best_ask,
    );

    let mut order: ManagedOrder =
        s.ledger
            .create_managed_buy_order(side, worst_price, want_qty, ts_ms, reason);
    order.maker_taker = false;
    order.target_price = target_price;

    let mut remaining = want_qty;
    let mut fills: Vec<(f64, f64)> = Vec::new();
    for level in asks.iter() {
        if remaining <= 0.0 {
            break;
        }
        if level.price > worst_price {
            break;
        }
        if level.qty <= 0.0 {
            continue;
        }
        let take = remaining.min(level.qty);
        fills.push((level.price, take));
        remaining -= take;
    }

    for (price, qty) in &fills {
        s.apply_fill(side, true, false, *price, *qty, ts_ms, ub, ua, db, da, None);
        order.record_fill_at(*qty, *price, ts_ms);
    }

    let filled = order.filled_qty;
    if filled <= 0.0 {
        order.status = OrderStatus::Cancelled;
        order.reject_reason = Some("worst_breach".to_string());
        order.updated_ts_ms = ts_ms;
    } else if order.status == OrderStatus::PartiallyFilled {
        order.status = OrderStatus::Cancelled;
        order.reject_reason = Some("ioc_remainder".to_string());
        order.updated_ts_ms = ts_ms;
    }
    s.ledger.order_history.push(order);

    IocOutcome { filled_qty: filled }
}
