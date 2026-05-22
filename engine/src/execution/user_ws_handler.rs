//! Polymarket User-Channel 事件回灌：把 WS 的 trade / order 事件接到 ledger + my_orders / my_fills。
//!
//! 关键设计：
//!   * `MATCHED` 入账：去重 → `ledger.apply_fill`（用 WS `fee_rate_bps` 真值）→ INSERT my_fills
//!   * `MINED / CONFIRMED`：UPDATE my_fills 的 status + transaction_hash + last_update（ledger 不动）
//!   * `FAILED / RETRYING`：忽略（按用户决策；接受 MATCHED 后链上失败的罕见 PnL 偏差）
//!   * UserOrderEvent 当前只用作 ManagedOrder 状态推进 + my_orders 撤单回灌；
//!     非阻塞主链路（成交以 trade 事件为准）。

use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::position::{OrderStatus, PositionSide};
use crate::tui::app::AppState;
use crate::web::db::{DbMsg, DbSender, MyFillRow};
use crate::ws::poly_user_ws::{MakerOrderInfo, UserEvent, UserOrderEvent, UserTradeEvent};

pub async fn run_user_event_handler(
    mut rx: mpsc::Receiver<UserEvent>,
    state: Arc<RwLock<AppState>>,
    db_tx: DbSender,
) {
    while let Some(ev) = rx.recv().await {
        match ev {
            UserEvent::Trade(t) => handle_trade(&state, &db_tx, t),
            UserEvent::Order(o) => handle_order(&state, &db_tx, o),
        }
    }
    warn!("user_event_handler exit (channel closed)");
}

fn handle_trade(state: &Arc<RwLock<AppState>>, db_tx: &DbSender, t: UserTradeEvent) {
    let status = t.status.to_ascii_uppercase();
    match status.as_str() {
        "MATCHED" => insert_matched(state, db_tx, &t),
        "MINED" | "CONFIRMED" => {
            let _ = db_tx.send(DbMsg::FillChainUpdate {
                trade_id: t.id,
                status,
                transaction_hash: t.transaction_hash,
                last_update: parse_i64(t.last_update.as_deref()),
            });
        }
        _ => debug!(status = %t.status, trade_id = %t.id, "user-ws trade ignored"),
    }
}

fn insert_matched(state: &Arc<RwLock<AppState>>, db_tx: &DbSender, t: &UserTradeEvent) {
    let Ok(mut s) = state.write() else {
        warn!("AppState write lock poisoned in insert_matched");
        return;
    };
    if !s.ledger.seen_fill_ids.insert(t.id.clone()) {
        return; // 去重
    }

    // 1. asset_id → outcome
    let Some(outcome_side) = resolve_outcome(&s, &t.asset_id) else {
        warn!(asset_id = %t.asset_id, trade_id = %t.id, "user-ws fill: asset_id 不在当前两侧 token 中");
        return;
    };
    let outcome_label: &'static str = match outcome_side {
        PositionSide::Up => "up",
        PositionSide::Down => "down",
    };

    let price = t.price.parse::<f64>().unwrap_or(0.0);
    let size = t.size.parse::<f64>().unwrap_or(0.0);
    if price <= 0.0 || size <= 0.0 {
        warn!(price, size, trade_id = %t.id, "user-ws fill: 无效 price/size，跳过");
        return;
    }

    // 2. ledger.apply_fill（fee 用 WS 真值，缺失 fallback 公式）
    let fee_bps = t.fee_rate_bps.as_deref().and_then(|s| s.parse::<f64>().ok());
    let is_maker = t.trader_side.eq_ignore_ascii_case("MAKER");
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (up_bid, up_ask, down_bid, down_ask) = (
        s.poly_best_bid,
        s.poly_best_ask,
        s.poly_down_best_bid,
        s.poly_down_best_ask,
    );
    s.apply_fill(
        outcome_side,
        true, // buy_sell
        is_maker,
        price,
        size,
        now_ms,
        up_bid,
        up_ask,
        down_bid,
        down_ask,
        fee_bps,
    );

    // 3. INSERT my_fills
    let row = MyFillRow {
        trade_id: t.id.clone(),
        taker_order_id: t.taker_order_id.clone(),
        market: opt_str(&t.market),
        asset_id: t.asset_id.clone(),
        side: "buy",
        outcome: outcome_label,
        size,
        price,
        usd_value: size * price,
        fee_rate_bps: t.fee_rate_bps.clone(),
        status: "MATCHED".to_string(),
        matchtime: parse_i64(t.matchtime.as_deref()),
        last_update: parse_i64(t.last_update.as_deref()),
        timestamp: parse_i64(Some(&t.timestamp)).unwrap_or(now_ms / 1000),
        maker_orders_json: serialize_maker_orders(&t.maker_orders),
        maker_address: maker_address_from(&t.maker_orders),
        transaction_hash: t.transaction_hash.clone(),
        trader_side: opt_str(&t.trader_side),
        window_end_ts: s.poly_window_end_ts,
        ts_ms: now_ms,
    };
    let _ = db_tx.send(DbMsg::FillRow(row));
}

fn handle_order(state: &Arc<RwLock<AppState>>, db_tx: &DbSender, o: UserOrderEvent) {
    let kind = o.order_event_type.to_ascii_uppercase();
    let size_matched: f64 = o.size_matched.parse().unwrap_or(0.0);
    let original_size: f64 = o.original_size.parse().unwrap_or(0.0);
    let now_ms = chrono::Utc::now().timestamp_millis();

    if kind == "CANCELLATION" {
        let _ = db_tx.send(DbMsg::OrderCancelled {
            server_order_id: o.id.clone(),
            ts_ms: now_ms,
        });
    }

    // 推进 ManagedOrder：trade 事件才是 PnL 来源，这里只更新 status / filled_qty 显示
    let Ok(mut s) = state.write() else {
        return;
    };
    let Some(side) = resolve_outcome(&s, &o.asset_id) else {
        return;
    };
    let pending = match side {
        PositionSide::Up => s.ledger.maker_buy_intent_up.as_mut(),
        PositionSide::Down => s.ledger.maker_buy_intent_down.as_mut(),
    };
    if let Some(order) = pending {
        if order.order_hash.as_deref() != Some(o.id.as_str()) {
            return;
        }
        if size_matched > order.filled_qty {
            order.filled_qty = size_matched.min(order.qty);
        }
        order.updated_ts_ms = now_ms;
        order.status = match kind.as_str() {
            "PLACEMENT" => OrderStatus::Accepted,
            "UPDATE" => {
                if size_matched > 0.0 && size_matched < order.qty {
                    OrderStatus::PartiallyFilled
                } else if size_matched >= order.qty && order.qty > 0.0 {
                    OrderStatus::Filled
                } else {
                    order.status
                }
            }
            "CANCELLATION" => OrderStatus::Cancelled,
            _ => order.status,
        };
        debug!(
            ?side,
            order_id = %o.id,
            kind = %kind,
            size_matched,
            original_size,
            "user-ws order event applied"
        );
        if matches!(order.status, OrderStatus::Cancelled | OrderStatus::Filled) {
            archive_pending(&mut s, side);
        }
    }
}

fn archive_pending(s: &mut AppState, side: PositionSide) {
    let slot = match side {
        PositionSide::Up => &mut s.ledger.maker_buy_intent_up,
        PositionSide::Down => &mut s.ledger.maker_buy_intent_down,
    };
    if let Some(o) = slot.take() {
        s.ledger.order_history.push(o);
    }
}

#[inline]
fn resolve_outcome(s: &AppState, asset_id: &str) -> Option<PositionSide> {
    if asset_id == s.poly_token_id {
        Some(PositionSide::Up)
    } else if asset_id == s.poly_down_token_id {
        Some(PositionSide::Down)
    } else {
        None
    }
}

#[inline]
fn parse_i64(s: Option<&str>) -> Option<i64> {
    s.and_then(|v| v.parse::<i64>().ok())
}

#[inline]
fn opt_str(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn serialize_maker_orders(v: &[MakerOrderInfo]) -> Option<String> {
    if v.is_empty() {
        return None;
    }
    let trimmed: Vec<serde_json::Value> = v
        .iter()
        .map(|m| {
            serde_json::json!({
                "order_id": m.order_id,
                "owner": m.owner,
                "matched_amount": m.matched_amount,
                "price": m.price,
                "asset_id": m.asset_id,
                "outcome": m.outcome,
                "fee_rate_bps": m.fee_rate_bps,
            })
        })
        .collect();
    serde_json::to_string(&trimmed).ok()
}

#[inline]
fn maker_address_from(v: &[MakerOrderInfo]) -> Option<String> {
    v.first().map(|m| m.owner.clone()).filter(|s| !s.is_empty())
}
