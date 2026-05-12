//! `AppState` → `DashboardSnapshot` 投影。
//!
//! 持读锁的时间尽量短：从 `state.read()` 立即克隆出 `AppState`，再按字段构造 wire format。
//! 限制盘口档数（Binance 默认 10 档，Polymarket 默认前 15 档）以控制 JSON 体积。

use std::sync::{Arc, RwLock};

use shared_types::{
    DashboardSnapshot, DelayStatsView, FairValueView, HeaderInfo, LatencyStats, Level,
    OrderbookView, PolyBook, PolyView, PositionSide, PositionView, TradeRow,
};

use crate::position::{ManagedOrder, OrderStatus, PendingOrderReason};
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
        mergeable_pairs: s.mergeable_pairs(),
        merged_pairs: s.ledger.merged_pairs,
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
        ioc_chase_fill_rate: rate(ioc.chase_filled, ioc.chase_orders),
        ioc_rebal_orders: ioc.rebal_orders,
        ioc_rebal_fill_rate: rate(ioc.rebal_filled, ioc.rebal_orders),
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
