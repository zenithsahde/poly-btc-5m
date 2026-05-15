//! `AppState` → `DashboardSnapshot` 投影。
//!
//! 持读锁的时间尽量短：从 `state.read()` 立即克隆出 `AppState`，再按字段构造 wire format。
//! 限制盘口档数（Binance 默认 10 档，Polymarket 默认前 15 档）以控制 JSON 体积。

use std::sync::{Arc, RwLock};

use shared_types::{
    DashboardSnapshot, DelayStatsView, FairValueView, HeaderInfo, LatencyStats, Level,
    OrderEventView, OrderbookView, PnlPointView, PolyBook, PolyView, PositionSide, PositionView,
    TradeRow,
};

use crate::position::{
    ManagedOrder, OrderStatus, PendingOrderReason, PositionSide as LedgerSide, TradeRecord,
};
use crate::tui::app::{AppState, BookLevel, ChaseSide};

const POLY_BOOK_DEPTH: usize = 15;

pub fn build(state: &Arc<RwLock<AppState>>) -> DashboardSnapshot {
    // 短暂持锁克隆 AppState（与 TUI 渲染同样的策略，参见 tui/ui.rs）
    let s: AppState = match state.read() {
        Ok(g) => g.clone(),
        Err(p) => p.into_inner().clone(),
    };

    let header = HeaderInfo {
        symbol: s.symbol.clone(),
        ws_connected: s.ws_connected,
        poly_ws_connected: s.poly_ws_connected,
        msg_rate: s.msg_rate,
        total_msgs: s.total_msgs,
        uptime: s.uptime(),
        uptime_secs: s.start_time.elapsed().as_secs(),
        is_live_mode: s.is_live_mode,
    };

    let orderbook = OrderbookView {
        best_bid: s.best_bid,
        best_ask: s.best_ask,
        spread_bps: s.spread_bps,
        mid_price: s.mid_price,
        bids: levels_from(&s.bids),
        asks: levels_from(&s.asks),
    };

    let trades: Vec<TradeRow> = s
        .recent_trades
        .iter()
        .map(|t| TradeRow {
            dir_time: t.dir_time.clone(),
            is_buy: t.is_buy,
            price: t.price,
            qty: t.qty,
            notional: t.notional,
        })
        .collect();

    let latency = LatencyStats {
        p50_ms: s.latency.p50_ms,
        p99_ms: s.latency.p99_ms,
        max_ms: s.latency.max_ms,
        parse_p99_us: s.latency.parse_p99_us,
        msg_count: s.latency.msg_count,
        last_warn_ms: s.last_warn_ms,
    };

    let poly = PolyView {
        slug: s.poly_market_slug.clone(),
        token_id: s.poly_token_id.clone(),
        window_end_ts: s.poly_window_end_ts,
        expiry_minutes: s.expiry_minutes,
        poly_delay_ms: s.poly_delay_ms(),
        up: PolyBook {
            bids: levels_from_capped(&s.poly_bids, POLY_BOOK_DEPTH),
            asks: levels_from_capped(&s.poly_asks, POLY_BOOK_DEPTH),
            best_bid: s.poly_best_bid,
            best_ask: s.poly_best_ask,
            last_trade_price: s.poly_last_trade_price,
            last_trade_side: s.poly_last_trade_side.clone(),
        },
        down: PolyBook {
            bids: levels_from_capped(&s.poly_down_bids, POLY_BOOK_DEPTH),
            asks: levels_from_capped(&s.poly_down_asks, POLY_BOOK_DEPTH),
            best_bid: s.poly_down_best_bid,
            best_ask: s.poly_down_best_ask,
            last_trade_price: s.poly_down_last_trade_price,
            last_trade_side: s.poly_down_last_trade_side.clone(),
        },
    };

    let up_mid = s.poly_up_mid();
    let down_mid = s.poly_down_mid();
    let ioc = ioc_stats(&s.ledger.order_history);
    let fills = window_fill_stats(&s.ledger.trades_current_window);
    let projected_qty_up = s.projected_qty_up_after_intents();
    let projected_qty_down = s.projected_qty_down_after_intents();
    let position = PositionView {
        up: PositionSide {
            qty: s.ledger.position_up.qty,
            avg_price: s.ledger.position_up.avg_price,
            float_pnl: s.ledger.position_up.float_pnl(up_mid),
        },
        down: PositionSide {
            qty: s.ledger.position_down.qty,
            avg_price: s.ledger.position_down.avg_price,
            float_pnl: s.ledger.position_down.float_pnl(down_mid),
        },
        avg_sum: s.position_avg_sum(),
        projected_avg_sum: if s.ledger.maker_buy_intent_up.is_some()
            || s.ledger.maker_buy_intent_down.is_some()
        {
            Some(s.projected_avg_sum_after_intents())
        } else {
            None
        },
        projected_qty_up,
        projected_qty_down,
        projected_skew: projected_skew(projected_qty_up, projected_qty_down),
        mergeable_pairs: s.mergeable_pairs(),
        merged_pairs: s.ledger.merged_pairs,
        merge_count: s.ledger.merge_count,
        merge_pnl: s.ledger.merge_pnl,
        realized_pnl: s.ledger.realized_pnl,
        total_fee: s.ledger.total_fee,
        total_rebate: s.ledger.total_rebate,
        cash_received: s.ledger.cash_received,
        cash_paid: s.ledger.cash_paid,
        cash_pnl: s.cash_pnl(),
        inventory_value: s.inventory_value(),
        total_float_pnl: s.total_float_pnl(),
        net_pnl: s.net_pnl(),
        chase_side: s.chase_side.map(chase_side_str),
        rebalance_hint_up: s.ledger.rebalance_hint_up,
        rebalance_hint_down: s.ledger.rebalance_hint_down,
        maker_buy_intent_up: managed_order_tuple(s.ledger.maker_buy_intent_up.as_ref()),
        maker_buy_intent_down: managed_order_tuple(s.ledger.maker_buy_intent_down.as_ref()),
        window_fill_rows: fills.fill_rows,
        window_up_qty: fills.up_qty,
        window_up_vwap: fills.up_vwap(),
        window_down_qty: fills.down_qty,
        window_down_vwap: fills.down_vwap(),
        window_notional: fills.total_notional(),
        ioc_orders: ioc.orders,
        ioc_fill_rate: rate(ioc.filled_orders, ioc.orders),
        ioc_worst_breach_rate: rate(ioc.worst_breach, ioc.orders),
        ioc_partial_rate: rate(ioc.partial, ioc.orders),
        ioc_avg_fill_levels: if ioc.filled_orders == 0 {
            0.0
        } else {
            ioc.fill_levels_sum as f64 / ioc.filled_orders as f64
        },
        ioc_max_fill_levels: ioc.fill_levels_max,
        ioc_chase_orders: ioc.chase_orders,
        ioc_chase_filled: ioc.chase_filled,
        ioc_chase_fill_rate: rate(ioc.chase_filled, ioc.chase_orders),
        ioc_rebal_orders: ioc.rebal_orders,
        ioc_rebal_filled: ioc.rebal_filled,
        ioc_rebal_fill_rate: rate(ioc.rebal_filled, ioc.rebal_orders),
        ioc_last_order: ioc
            .last_order(&s.ledger.order_history)
            .map(ioc_order_summary),
        last_action: latest_position_action(
            &s.ledger.trades_current_window,
            &s.ledger.order_history,
        ),
        pnl_history: s
            .web_pnl_history
            .iter()
            .map(|p| PnlPointView {
                uptime_secs: p.uptime_secs,
                net_pnl: p.net_pnl,
                cash_pnl: p.cash_pnl,
                inventory_value: p.inventory_value,
            })
            .collect(),
        order_events: order_events(
            &s.ledger.trades_current_window,
            &s.ledger.order_history,
            s.ledger.maker_buy_intent_up.as_ref(),
            s.ledger.maker_buy_intent_down.as_ref(),
            s.net_pnl(),
        ),
    };

    let fair_value = FairValueView {
        fair_price: s.fair_price,
        fair_price_down: s.fair_price_down,
        volatility_annual: s.volatility_annual,
        sticky_volatility: s.sticky_volatility,
        sigma_used_default: s.sigma_used_default,
        sigma_source: s.sigma_source.clone(),
        market_state: s.market_state.clone(),
        signal_gap_bps: s.signal_gap_bps,
        last_snipe_info: s.last_snipe_info.clone(),
        snipe_count: s.snipe_count,
        snipe_threshold_bps: s.snipe_threshold_bps,
        iv_poly: s.iv_poly,
        strike_price: s.strike_price,
        expiry_minutes: s.expiry_minutes,
        poly_btc_offset: s.poly_btc_offset,
        poly_btc_price: s.poly_btc_price(),
    };

    let delay = DelayStatsView {
        count_pos: s.delay_stats.count_pos,
        sum_pos_ms: s.delay_stats.sum_pos_ms,
        min_pos_ms: s.delay_stats.min_pos_ms,
        max_pos_ms: s.delay_stats.max_pos_ms,
        count_neg: s.delay_stats.count_neg,
        sum_neg_ms: s.delay_stats.sum_neg_ms,
        min_neg_ms: s.delay_stats.min_neg_ms,
        max_neg_ms: s.delay_stats.max_neg_ms,
    };

    DashboardSnapshot {
        header,
        orderbook,
        trades,
        latency,
        poly,
        position,
        fair_value,
        delay,
    }
}

fn levels_from(src: &[BookLevel]) -> Vec<Level> {
    src.iter()
        .map(|l| Level {
            price: l.price,
            qty: l.qty,
        })
        .collect()
}

fn levels_from_capped(src: &[BookLevel], cap: usize) -> Vec<Level> {
    src.iter()
        .take(cap)
        .map(|l| Level {
            price: l.price,
            qty: l.qty,
        })
        .collect()
}

fn chase_side_str(s: ChaseSide) -> String {
    match s {
        ChaseSide::Up => "UP".to_string(),
        ChaseSide::Down => "DOWN".to_string(),
    }
}

fn managed_order_tuple(order: Option<&ManagedOrder>) -> Option<(f64, f64, i64)> {
    order.map(|o| (o.price, o.remaining_qty(), o.placed_ts_ms))
}

fn projected_skew(up_qty: f64, down_qty: f64) -> String {
    if up_qty > down_qty {
        "UP多".to_string()
    } else if down_qty > up_qty {
        "DOWN多".to_string()
    } else {
        "—".to_string()
    }
}

#[derive(Default)]
struct IocStats {
    orders: usize,
    filled_orders: usize,
    worst_breach: usize,
    partial: usize,
    fill_levels_sum: u32,
    fill_levels_max: u32,
    chase_orders: usize,
    chase_filled: usize,
    rebal_orders: usize,
    rebal_filled: usize,
}

impl IocStats {
    fn last_order<'a>(&self, orders: &'a [ManagedOrder]) -> Option<&'a ManagedOrder> {
        orders.iter().max_by_key(|o| o.updated_ts_ms)
    }
}

fn ioc_stats(orders: &[ManagedOrder]) -> IocStats {
    let mut stats = IocStats::default();
    for order in orders {
        stats.orders += 1;
        let filled = order.filled_qty > 0.0;
        if filled {
            stats.filled_orders += 1;
            stats.fill_levels_sum = stats.fill_levels_sum.saturating_add(order.fill_levels);
            stats.fill_levels_max = stats.fill_levels_max.max(order.fill_levels);
        }
        if order.reject_reason.as_deref() == Some("worst_breach") {
            stats.worst_breach += 1;
        }
        if order.reject_reason.as_deref() == Some("ioc_remainder")
            || (filled && order.filled_qty < order.qty)
            || order.status == OrderStatus::PartiallyFilled
        {
            stats.partial += 1;
        }
        match order.reason {
            PendingOrderReason::Chase => {
                stats.chase_orders += 1;
                if filled {
                    stats.chase_filled += 1;
                }
            }
            PendingOrderReason::Rebalance => {
                stats.rebal_orders += 1;
                if filled {
                    stats.rebal_filled += 1;
                }
            }
        }
    }
    stats
}

fn rate(part: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64
    }
}

#[derive(Default)]
struct WindowFillStats {
    fill_rows: usize,
    up_qty: f64,
    up_notional: f64,
    down_qty: f64,
    down_notional: f64,
}

impl WindowFillStats {
    fn up_vwap(&self) -> f64 {
        if self.up_qty > 0.0 {
            self.up_notional / self.up_qty
        } else {
            0.0
        }
    }

    fn down_vwap(&self) -> f64 {
        if self.down_qty > 0.0 {
            self.down_notional / self.down_qty
        } else {
            0.0
        }
    }

    fn total_notional(&self) -> f64 {
        self.up_notional + self.down_notional
    }
}

fn window_fill_stats(trades: &[TradeRecord]) -> WindowFillStats {
    let mut stats = WindowFillStats::default();
    for trade in trades.iter().filter(|t| t.buy_sell) {
        stats.fill_rows += 1;
        let notional = trade.price * trade.qty;
        match trade.side {
            LedgerSide::Up => {
                stats.up_qty += trade.qty;
                stats.up_notional += notional;
            }
            LedgerSide::Down => {
                stats.down_qty += trade.qty;
                stats.down_notional += notional;
            }
        }
    }
    stats
}

fn ioc_order_summary(order: &ManagedOrder) -> String {
    let side = match order.side {
        LedgerSide::Up => "UP",
        LedgerSide::Down => "DOWN",
    };
    let reason = match order.reason {
        PendingOrderReason::Chase => "chase",
        PendingOrderReason::Rebalance => "rebal",
    };
    let status =
        if order.filled_qty <= 0.0 && order.reject_reason.as_deref() == Some("worst_breach") {
            "拒单"
        } else if order.reject_reason.as_deref() == Some("ioc_remainder") {
            "部分"
        } else if order.filled_qty > 0.0 {
            "成交"
        } else {
            "取消"
        };
    if order.filled_qty > 0.0 {
        format!(
            "{} {}/{} {:.0}/{:.0}张 VWAP={:.2} 档={}",
            side,
            status,
            reason,
            order.filled_qty,
            order.qty,
            order.vwap(),
            order.fill_levels
        )
    } else {
        format!(
            "{} {}/{} 0/{:.0}张 worst={:.2}",
            side, status, reason, order.qty, order.price
        )
    }
}

fn latest_position_action(trades: &[TradeRecord], orders: &[ManagedOrder]) -> Option<String> {
    let last_trade = trades.iter().max_by_key(|t| t.ts_ms);
    let last_order = orders.iter().max_by_key(|o| o.updated_ts_ms);
    match (last_trade, last_order) {
        (Some(t), Some(o)) if t.ts_ms >= o.updated_ts_ms => Some(trade_action_summary(t)),
        (Some(_), Some(o)) => Some(order_action_summary(o)),
        (Some(t), None) => Some(trade_action_summary(t)),
        (None, Some(o)) => Some(order_action_summary(o)),
        (None, None) => None,
    }
}

fn trade_action_summary(trade: &TradeRecord) -> String {
    let venue = if trade.maker_taker { "MAKER" } else { "TAKER" };
    let side = match trade.side {
        LedgerSide::Up => "UP",
        LedgerSide::Down => "DOWN",
    };
    let direction = if trade.buy_sell { "BUY" } else { "SELL" };
    format!(
        "{} {} {} {:.0}@{:.2} cost={:.2}",
        venue,
        side,
        direction,
        trade.qty,
        trade.price,
        trade.qty * trade.price
    )
}

fn order_action_summary(order: &ManagedOrder) -> String {
    let side = match order.side {
        LedgerSide::Up => "UP",
        LedgerSide::Down => "DOWN",
    };
    let reason = match order.reason {
        PendingOrderReason::Chase => "chase",
        PendingOrderReason::Rebalance => "rebal",
    };
    let status = if order.filled_qty > 0.0 {
        "FILL"
    } else if order.reject_reason.as_deref() == Some("worst_breach") {
        "REJECT"
    } else {
        match order.status {
            OrderStatus::Cancelled => "CANCEL",
            OrderStatus::Expired => "CANCEL",
            OrderStatus::Rejected => "REJECT",
            _ => "ORDER",
        }
    };
    format!(
        "{} {} {} {} {:.0}@{:.2}",
        status,
        liquidity_name(order.maker_taker),
        side,
        reason,
        order.qty,
        order.price
    )
}

fn order_event_summary(order: &ManagedOrder) -> String {
    format!(
        "{} {} {:.0}@{:.2}",
        side_name(order.side),
        reason_name(order.reason),
        if order.filled_qty > 0.0 {
            order.filled_qty
        } else {
            order.qty
        },
        if order.filled_qty > 0.0 {
            order.vwap()
        } else {
            order.price
        }
    )
}

fn order_events(
    trades: &[TradeRecord],
    orders: &[ManagedOrder],
    pending_up: Option<&ManagedOrder>,
    pending_down: Option<&ManagedOrder>,
    current_pnl: f64,
) -> Vec<OrderEventView> {
    let mut events = Vec::new();

    for order in [pending_up, pending_down].into_iter().flatten() {
        events.push(OrderEventView {
            ts_ms: order.placed_ts_ms,
            kind: "POST".to_string(),
            liquidity: liquidity_name(order.maker_taker).to_string(),
            side: side_name(order.side).to_string(),
            summary: format!(
                "{} {} {:.0}@{:.2}",
                side_name(order.side),
                reason_name(order.reason),
                order.remaining_qty(),
                order.price
            ),
            qty: order.remaining_qty(),
            price: order.price,
            pnl: current_pnl,
        });
    }

    for trade in trades {
        let kind = if trade.maker_taker { "MAKER" } else { "TAKER" };
        events.push(OrderEventView {
            ts_ms: trade.ts_ms,
            kind: "FILL".to_string(),
            liquidity: kind.to_string(),
            side: side_name(trade.side).to_string(),
            summary: format!(
                "{} {} {:.0}@{:.2}",
                if trade.buy_sell { "BUY" } else { "SELL" },
                side_name(trade.side),
                trade.qty,
                trade.price
            ),
            qty: trade.qty,
            price: trade.price,
            pnl: current_pnl,
        });
    }

    for order in orders {
        events.push(OrderEventView {
            ts_ms: order.updated_ts_ms,
            kind: order_event_kind(order).to_string(),
            liquidity: liquidity_name(order.maker_taker).to_string(),
            side: side_name(order.side).to_string(),
            summary: order_event_summary(order),
            qty: if order.filled_qty > 0.0 {
                order.filled_qty
            } else {
                order.qty
            },
            price: if order.filled_qty > 0.0 {
                order.vwap()
            } else {
                order.price
            },
            pnl: current_pnl,
        });
    }

    events.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
    events.truncate(40);
    events
}

fn order_event_kind(order: &ManagedOrder) -> &'static str {
    if order.filled_qty > 0.0 {
        "MATCH"
    } else if order.reject_reason.as_deref() == Some("worst_breach") {
        "REJECT"
    } else {
        match order.status {
            OrderStatus::Cancelled => "CANCEL",
            OrderStatus::Expired => "CANCEL",
            OrderStatus::Rejected => "REJECT",
            _ => "ORDER",
        }
    }
}

fn side_name(side: LedgerSide) -> &'static str {
    match side {
        LedgerSide::Up => "UP",
        LedgerSide::Down => "DOWN",
    }
}

fn reason_name(reason: PendingOrderReason) -> &'static str {
    match reason {
        PendingOrderReason::Chase => "chase",
        PendingOrderReason::Rebalance => "rebal",
    }
}

fn liquidity_name(maker_taker: bool) -> &'static str {
    if maker_taker {
        "MAKER"
    } else {
        "TAKER"
    }
}
